#![allow(non_local_definitions)]

use std::collections::HashMap;

use blockifier::abi::constants as abi_constants;
use blockifier::blockifier::config::{ContractClassManagerConfig, TransactionExecutorConfig};
use blockifier::blockifier::transaction_executor::{TransactionExecutor, TransactionExecutorError};
use blockifier::bouncer::BouncerConfig;
use blockifier::context::{BlockContext, ChainInfo, FeeTokenAddresses};
use blockifier::execution::call_info::CallInfo;
use blockifier::execution::contract_class::VersionedRunnableCompiledClass;
use blockifier::fee::receipt::TransactionReceipt;
use blockifier::state::global_cache::GlobalContractCache;
use blockifier::transaction::objects::{ExecutionResourcesTraits, TransactionExecutionInfo};
use blockifier::transaction::transaction_execution::Transaction;
use blockifier::utils::usize_from_u64;
use blockifier::versioned_constants::VersionedConstants;
use papyrus_state_reader::papyrus_state::PapyrusReader;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyList};
use pyo3::{FromPyObject, PyAny, Python};
use serde::Serialize;
use starknet_api::block::BlockNumber;
use starknet_api::core::{ChainId, ContractAddress};
use starknet_api::execution_resources::GasVector;
use starknet_api::transaction::fields::Fee;
use starknet_types_core::felt::Felt;

use crate::errors::{NativeBlockifierError, NativeBlockifierResult};
use crate::py_objects::{
    PyBouncerConfig,
    PyConcurrencyConfig,
    PyContractClassManagerConfig,
    PyVersionedConstantsOverrides,
};
use crate::py_state_diff::{PyBlockInfo, PyStateDiff};
use crate::py_transaction::{py_tx, PyClassInfo, PY_TX_PARSING_ERR};
use crate::py_utils::{int_to_chain_id, into_block_number_hash_pair, PyFelt};
use crate::storage::{
    PapyrusStorage,
    RawDeclaredClassMapping,
    RawDeprecatedDeclaredClassMapping,
    Storage,
    StorageConfig,
};

pub(crate) type RawTransactionExecutionResult = Vec<u8>;
pub(crate) type PyVisitedSegmentsMapping = Vec<(PyFelt, Vec<usize>)>;

#[cfg(test)]
#[path = "py_block_executor_test.rs"]
mod py_block_executor_test;

const RESULT_SERIALIZE_ERR: &str = "Failed serializing execution info.";

/// A mapping from a transaction execution resource to its actual usage.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ResourcesMapping(pub HashMap<String, usize>);

/// Stripped down `TransactionExecutionInfo` for Python serialization, containing only the required
/// fields.
#[derive(Debug, Serialize)]
pub(crate) struct ThinTransactionExecutionInfo {
    pub validate_call_info: Option<CallInfo>,
    pub execute_call_info: Option<CallInfo>,
    pub fee_transfer_call_info: Option<CallInfo>,
    pub actual_fee: Fee,
    pub da_gas: GasVector,
    pub actual_resources: ResourcesMapping,
    pub revert_error: Option<String>,
    pub total_gas: GasVector,
}

impl ThinTransactionExecutionInfo {
    pub fn from_tx_execution_info(tx_execution_info: TransactionExecutionInfo) -> Self {
        Self {
            validate_call_info: tx_execution_info.validate_call_info,
            execute_call_info: tx_execution_info.execute_call_info,
            fee_transfer_call_info: tx_execution_info.fee_transfer_call_info,
            actual_fee: tx_execution_info.receipt.fee,
            da_gas: tx_execution_info.receipt.da_gas,
            actual_resources: ThinTransactionExecutionInfo::receipt_to_resources_mapping(
                &tx_execution_info.receipt,
            ),
            revert_error: tx_execution_info.revert_error.map(|error| error.to_string()),
            total_gas: tx_execution_info.receipt.gas,
        }
    }
    pub fn serialize(self) -> RawTransactionExecutionResult {
        serde_json::to_vec(&self).expect(RESULT_SERIALIZE_ERR)
    }

    pub fn receipt_to_resources_mapping(receipt: &TransactionReceipt) -> ResourcesMapping {
        let GasVector { l1_gas, l1_data_gas, l2_gas } = receipt.gas;
        let vm_resources = &receipt.resources.computation.vm_resources;
        let mut resources = HashMap::from([(
            abi_constants::N_STEPS_RESOURCE.to_string(),
            vm_resources.total_n_steps(),
        )]);
        resources.extend(
            vm_resources
                .prover_builtins()
                .iter()
                .map(|(builtin, value)| (builtin.to_str_with_suffix().to_string(), *value)),
        );
        // TODO(Yoni) remove these since we pass the gas vector in separate.
        resources.extend(HashMap::from([
            (
                abi_constants::L1_GAS_USAGE.to_string(),
                usize_from_u64(l1_gas.0)
                    .expect("This conversion should not fail as the value is a converted usize."),
            ),
            (
                abi_constants::BLOB_GAS_USAGE.to_string(),
                usize_from_u64(l1_data_gas.0)
                    .expect("This conversion should not fail as the value is a converted usize."),
            ),
            (
                abi_constants::L2_GAS_USAGE.to_string(),
                usize_from_u64(l2_gas.0)
                    .expect("This conversion should not fail as the value is a converted usize."),
            ),
        ]));
        *resources.get_mut(abi_constants::N_STEPS_RESOURCE).unwrap_or(&mut 0) +=
            receipt.resources.computation.n_reverted_steps;

        ResourcesMapping(resources)
    }
}

#[pyclass]
pub struct PyBlockExecutor {
    pub bouncer_config: BouncerConfig,
    pub tx_executor_config: TransactionExecutorConfig,
    pub chain_info: ChainInfo,
    pub versioned_constants: VersionedConstants,
    pub tx_executor: Option<TransactionExecutor<PapyrusReader>>,
    /// `Send` trait is required for `pyclass` compatibility as Python objects must be threadsafe.
    pub storage: Box<dyn Storage + Send>,
    pub contract_class_manager_config: ContractClassManagerConfig,
    pub global_contract_cache: GlobalContractCache<VersionedRunnableCompiledClass>,
}

#[pymethods]
impl PyBlockExecutor {
    #[new]
    #[pyo3(signature = (bouncer_config, concurrency_config, contract_class_manager_config, os_config, target_storage_config, py_versioned_constants_overrides))]
    pub fn create(
        bouncer_config: PyBouncerConfig,
        concurrency_config: PyConcurrencyConfig,
        contract_class_manager_config: PyContractClassManagerConfig,
        os_config: PyOsConfig,
        target_storage_config: StorageConfig,
        py_versioned_constants_overrides: PyVersionedConstantsOverrides,
    ) -> Self {
        log::debug!("Initializing Block Executor...");
        let storage =
            PapyrusStorage::new(target_storage_config).expect("Failed to initialize storage.");
        let versioned_constants =
            VersionedConstants::get_versioned_constants(py_versioned_constants_overrides.into());
        log::debug!("Initialized Block Executor.");

        Self {
            bouncer_config: bouncer_config.try_into().expect("Failed to parse bouncer config."),
            tx_executor_config: TransactionExecutorConfig {
                concurrency_config: concurrency_config.into(),
            },
            chain_info: os_config.into_chain_info(),
            versioned_constants,
            tx_executor: None,
            storage: Box::new(storage),
            contract_class_manager_config: contract_class_manager_config.into(),
            global_contract_cache: GlobalContractCache::new(
                contract_class_manager_config.contract_cache_size,
            ),
        }
    }

    // Transaction Execution API.

    /// Initializes the transaction executor for the given block.
    #[pyo3(signature = (next_block_info, old_block_number_and_hash))]
    fn setup_block_execution(
        &mut self,
        next_block_info: PyBlockInfo,
        old_block_number_and_hash: Option<(u64, PyFelt)>,
    ) -> NativeBlockifierResult<()> {
        // Create block context.
        let block_context = BlockContext::new(
            next_block_info.try_into()?,
            self.chain_info.clone(),
            self.versioned_constants.clone(),
            self.bouncer_config.clone(),
        );
        let next_block_number = block_context.block_info().block_number;

        // Create state reader.
        let papyrus_reader = self.get_aligned_reader(next_block_number);
        // Create and set executor.
        self.tx_executor = Some(TransactionExecutor::pre_process_and_create(
            papyrus_reader,
            block_context,
            into_block_number_hash_pair(old_block_number_and_hash),
            self.tx_executor_config.clone(),
        )?);
        Ok(())
    }

    fn teardown_block_execution(&mut self) {
        self.tx_executor = None;
    }

    #[pyo3(signature = (tx, optional_py_class_info))]
    pub fn execute(
        &mut self,
        tx: &PyAny,
        optional_py_class_info: Option<PyClassInfo>,
    ) -> NativeBlockifierResult<Py<PyBytes>> {
        let tx: Transaction = py_tx(tx, optional_py_class_info).expect(PY_TX_PARSING_ERR);
        let tx_execution_info = self.tx_executor().execute(&tx)?;
        let thin_tx_execution_info =
            ThinTransactionExecutionInfo::from_tx_execution_info(tx_execution_info);

        // Serialize and convert to PyBytes.
        let serialized_tx_execution_info = thin_tx_execution_info.serialize();
        Ok(Python::with_gil(|py| PyBytes::new(py, &serialized_tx_execution_info).into()))
    }

    /// Executes the given transactions on the Blockifier state.
    /// Stops if and when there is no more room in the block, and returns the executed transactions'
    /// results as a PyList of (success (bool), serialized result (bytes)) tuples.
    #[pyo3(signature = (txs_with_class_infos))]
    pub fn execute_txs(
        &mut self,
        txs_with_class_infos: Vec<(&PyAny, Option<PyClassInfo>)>,
    ) -> Py<PyList> {
        // Parse Py transactions.
        let txs: Vec<Transaction> = txs_with_class_infos
            .into_iter()
            .map(|(tx, optional_py_class_info)| {
                py_tx(tx, optional_py_class_info).expect(PY_TX_PARSING_ERR)
            })
            .collect();

        // Run.
        let results =
            Python::with_gil(|py| py.allow_threads(|| self.tx_executor().execute_txs(&txs)));

        // Process results.
        // TODO(Yoni, 15/5/2024): serialize concurrently.
        let serialized_results: Vec<(bool, RawTransactionExecutionResult)> = results
            .into_iter()
            // Note: there might be less results than txs (if there is no room for all of them).
            .map(|result| match result {
                Ok(tx_execution_info) => (
                    true,
                    ThinTransactionExecutionInfo::from_tx_execution_info(
                        tx_execution_info,
                    )
                    .serialize(),
                ),
                Err(error) => (false, serialize_failure_reason(error)),
            })
            .collect();

        // Convert to Py types and allocate it on Python's heap, to be visible for Python's
        // garbage collector.
        Python::with_gil(|py| {
            let py_serialized_results: Vec<(bool, Py<PyBytes>)> = serialized_results
                .into_iter()
                .map(|(success, execution_result)| {
                    // Note that PyList converts the inner elements recursively, yet the default
                    // conversion of the execution result (Vec<u8>) is to a list of integers, which
                    // might be less efficient than bytes.
                    (success, PyBytes::new(py, &execution_result).into())
                })
                .collect();
            PyList::new(py, py_serialized_results).into()
        })
    }

    /// Returns the state diff, a list of contract class hash with the corresponding list of
    /// visited segment values and the block weights.
    pub fn finalize(
        &mut self,
    ) -> NativeBlockifierResult<(PyStateDiff, PyVisitedSegmentsMapping, Py<PyBytes>)> {
        log::debug!("Finalizing execution...");
        let (commitment_state_diff, visited_pcs, block_weights) = self.tx_executor().finalize()?;
        let visited_pcs = visited_pcs
            .into_iter()
            .map(|(class_hash, class_visited_pcs_vec)| {
                (PyFelt::from(class_hash), class_visited_pcs_vec)
            })
            .collect();
        let py_state_diff = PyStateDiff::from(commitment_state_diff);

        let serialized_block_weights =
            serde_json::to_vec(&block_weights).expect("Failed serializing bouncer weights.");
        let raw_block_weights =
            Python::with_gil(|py| PyBytes::new(py, &serialized_block_weights).into());

        log::debug!("Finalized execution.");

        Ok((py_state_diff, visited_pcs, raw_block_weights))
    }

    // Storage Alignment API.

    /// Appends state diff and block header into Papyrus storage.
    // Previous block ID can either be a block hash (starting from a Papyrus snapshot), or a
    // sequential ID (throughout sequencing).
    #[pyo3(signature = (
        block_id,
        previous_block_id,
        py_block_info,
        py_state_diff,
        declared_class_hash_to_class,
        deprecated_declared_class_hash_to_class
    ))]
    pub fn append_block(
        &mut self,
        block_id: u64,
        previous_block_id: Option<PyFelt>,
        py_block_info: PyBlockInfo,
        py_state_diff: PyStateDiff,
        declared_class_hash_to_class: RawDeclaredClassMapping,
        deprecated_declared_class_hash_to_class: RawDeprecatedDeclaredClassMapping,
    ) -> NativeBlockifierResult<()> {
        self.storage.append_block(
            block_id,
            previous_block_id,
            py_block_info,
            py_state_diff,
            declared_class_hash_to_class,
            deprecated_declared_class_hash_to_class,
        )
    }

    /// Returns the next block number, for which block header was not yet appended.
    /// Block header stream is usually ahead of the state diff stream, so this is the indicative
    /// marker.
    pub fn get_header_marker(&self) -> NativeBlockifierResult<u64> {
        self.storage.get_header_marker()
    }

    /// Returns the unique identifier of the given block number in bytes.
    #[pyo3(signature = (block_number))]
    fn get_block_id_at_target(&self, block_number: u64) -> NativeBlockifierResult<Option<PyFelt>> {
        let optional_block_id_bytes = self.storage.get_block_id(block_number)?;
        let Some(block_id_bytes) = optional_block_id_bytes else { return Ok(None) };

        let mut block_id_fixed_bytes = [0_u8; 32];
        block_id_fixed_bytes.copy_from_slice(&block_id_bytes);

        Ok(Some(PyFelt(Felt::from_bytes_be(&block_id_fixed_bytes))))
    }

    #[pyo3(signature = (source_block_number))]
    pub fn validate_aligned(&self, source_block_number: u64) {
        self.storage.validate_aligned(source_block_number);
    }

    /// Atomically reverts block header and state diff of given block number.
    /// If header exists without a state diff (usually the case), only the header is reverted.
    /// (this is true for every partial existence of information at tables).
    #[pyo3(signature = (block_number))]
    pub fn revert_block(&mut self, block_number: u64) -> NativeBlockifierResult<()> {
        // Clear global class cache, to peroperly revert classes declared in the reverted block.
        self.global_contract_cache.clear();
        self.storage.revert_block(block_number)
    }

    /// Deallocate the transaction executor and close storage connections.
    pub fn close(&mut self) {
        log::debug!("Closing Block Executor.");
        // If the block was not finalized (due to some exception occuring _in Python_), we need
        // to deallocate the transaction executor here to prevent leaks.
        self.teardown_block_execution();
        self.storage.close();
    }

    #[pyo3(signature = (concurrency_config, contract_class_manager_config, os_config, path, max_state_diff_size))]
    #[staticmethod]
    fn create_for_testing(
        concurrency_config: PyConcurrencyConfig,
        contract_class_manager_config: PyContractClassManagerConfig,
        os_config: PyOsConfig,
        path: std::path::PathBuf,
        max_state_diff_size: usize,
    ) -> Self {
        use blockifier::bouncer::BouncerWeights;
        // TODO(Meshi, 01/01/2025): Remove this once we fix all python tests that re-declare cairo0
        // contracts.
        let mut versioned_constants = VersionedConstants::latest_constants().clone();
        versioned_constants.disable_cairo0_redeclaration = false;
        Self {
            bouncer_config: BouncerConfig {
                block_max_capacity: BouncerWeights {
                    state_diff_size: max_state_diff_size,
                    ..BouncerWeights::max()
                },
            },
            tx_executor_config: TransactionExecutorConfig {
                concurrency_config: concurrency_config.into(),
            },
            storage: Box::new(PapyrusStorage::new_for_testing(path, &os_config.chain_id)),
            chain_info: os_config.into_chain_info(),
            versioned_constants,
            tx_executor: None,
            contract_class_manager_config: contract_class_manager_config.into(),
            global_contract_cache: GlobalContractCache::new(
                contract_class_manager_config.contract_cache_size,
            ),
        }
    }
}

impl PyBlockExecutor {
    pub fn tx_executor(&mut self) -> &mut TransactionExecutor<PapyrusReader> {
        self.tx_executor.as_mut().expect("Transaction executor should be initialized")
    }

    fn get_aligned_reader(&self, next_block_number: BlockNumber) -> PapyrusReader {
        // Full-node storage must be aligned to the Python storage before initializing a reader.
        self.storage.validate_aligned(next_block_number.0);
        PapyrusReader::new(
            self.storage.reader().clone(),
            next_block_number,
            self.global_contract_cache.clone(),
        )
    }

    pub fn create_for_testing_with_storage(storage: impl Storage + Send + 'static) -> Self {
        use blockifier::state::global_cache::GLOBAL_CONTRACT_CACHE_SIZE_FOR_TEST;
        Self {
            bouncer_config: BouncerConfig::max(),
            tx_executor_config: TransactionExecutorConfig::create_for_testing(true),
            storage: Box::new(storage),
            chain_info: ChainInfo::default(),
            versioned_constants: VersionedConstants::latest_constants().clone(),
            tx_executor: None,
            contract_class_manager_config: ContractClassManagerConfig {
                run_cairo_native: false,
                wait_on_native_compilation: false,
                contract_cache_size: GLOBAL_CONTRACT_CACHE_SIZE_FOR_TEST,
            },
            global_contract_cache: GlobalContractCache::new(GLOBAL_CONTRACT_CACHE_SIZE_FOR_TEST),
        }
    }

    #[cfg(test)]
    pub(crate) fn native_create_for_testing(
        concurrency_config: PyConcurrencyConfig,
        contract_class_manager_config: PyContractClassManagerConfig,
        os_config: PyOsConfig,
        path: std::path::PathBuf,
        max_state_diff_size: usize,
    ) -> Self {
        Self::create_for_testing(
            concurrency_config,
            contract_class_manager_config,
            os_config,
            path,
            max_state_diff_size,
        )
    }
}

#[derive(Clone, FromPyObject)]
pub struct PyOsConfig {
    #[pyo3(from_py_with = "int_to_chain_id")]
    pub chain_id: ChainId,
    pub deprecated_fee_token_address: PyFelt,
    pub fee_token_address: PyFelt,
}

impl PyOsConfig {
    pub fn into_chain_info(self) -> ChainInfo {
        ChainInfo::try_from(self).expect("Failed to convert chain info.")
    }
}

impl TryFrom<PyOsConfig> for ChainInfo {
    type Error = NativeBlockifierError;

    fn try_from(py_os_config: PyOsConfig) -> Result<Self, Self::Error> {
        Ok(Self {
            chain_id: py_os_config.chain_id,
            fee_token_addresses: FeeTokenAddresses {
                eth_fee_token_address: ContractAddress::try_from(
                    py_os_config.deprecated_fee_token_address.0,
                )?,
                strk_fee_token_address: ContractAddress::try_from(
                    py_os_config.fee_token_address.0,
                )?,
            },
        })
    }
}

impl Default for PyOsConfig {
    fn default() -> Self {
        Self {
            chain_id: ChainId::Other("".to_string()),
            deprecated_fee_token_address: Default::default(),
            fee_token_address: Default::default(),
        }
    }
}

fn serialize_failure_reason(error: TransactionExecutorError) -> RawTransactionExecutionResult {
    // TODO(Yoni, 1/7/2024): re-consider this serialization.
    serde_json::to_vec(&format!("{}", error)).expect(RESULT_SERIALIZE_ERR)
}
