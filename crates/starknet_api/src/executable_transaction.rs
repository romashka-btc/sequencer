use serde::{Deserialize, Serialize};

use crate::contract_class::{ClassInfo, ContractClass};
use crate::core::{calculate_contract_address, ChainId, ClassHash, ContractAddress, Nonce};
use crate::data_availability::DataAvailabilityMode;
use crate::rpc_transaction::{
    RpcDeployAccountTransaction,
    RpcDeployAccountTransactionV3,
    RpcInvokeTransaction,
    RpcInvokeTransactionV3,
};
use crate::transaction::fields::{
    AccountDeploymentData,
    AllResourceBounds,
    Calldata,
    ContractAddressSalt,
    Fee,
    PaymasterData,
    Tip,
    TransactionSignature,
    ValidResourceBounds,
};
use crate::transaction::{TransactionHash, TransactionHasher, TransactionVersion};
use crate::StarknetApiError;

macro_rules! implement_inner_tx_getter_calls {
    ($(($field:ident, $field_type:ty)),*) => {
        $(pub fn $field(&self) -> $field_type {
            self.tx.$field().clone()
        })*
    };
}

macro_rules! implement_getter_calls {
    ($(($field:ident, $field_type:ty)),*) => {
        $(pub fn $field(&self) -> $field_type {
            self.$field
        })*
};
}

macro_rules! implement_account_tx_inner_getters {
    ($(($field:ident, $field_type:ty)),*) => {
        $(pub fn $field(&self) -> $field_type {
            match self {
                AccountTransaction::Declare(tx) => tx.tx.$field().clone(),
                AccountTransaction::DeployAccount(tx) => tx.tx.$field().clone(),
                AccountTransaction::Invoke(tx) => tx.tx.$field().clone(),
            }
        })*
    };
}

/// Represents a paid Starknet transaction.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum AccountTransaction {
    Declare(DeclareTransaction),
    DeployAccount(DeployAccountTransaction),
    Invoke(InvokeTransaction),
}

impl AccountTransaction {
    implement_account_tx_inner_getters!(
        (resource_bounds, ValidResourceBounds),
        (tip, Tip),
        (signature, TransactionSignature),
        (nonce, Nonce),
        (nonce_data_availability_mode, DataAvailabilityMode),
        (fee_data_availability_mode, DataAvailabilityMode),
        (paymaster_data, PaymasterData),
        (version, TransactionVersion)
    );

    pub fn contract_address(&self) -> ContractAddress {
        match self {
            AccountTransaction::Declare(tx_data) => tx_data.tx.sender_address(),
            AccountTransaction::DeployAccount(tx_data) => tx_data.contract_address,
            AccountTransaction::Invoke(tx_data) => tx_data.tx.sender_address(),
        }
    }

    pub fn sender_address(&self) -> ContractAddress {
        self.contract_address()
    }

    pub fn tx_hash(&self) -> TransactionHash {
        match self {
            AccountTransaction::Declare(tx_data) => tx_data.tx_hash,
            AccountTransaction::DeployAccount(tx_data) => tx_data.tx_hash,
            AccountTransaction::Invoke(tx_data) => tx_data.tx_hash,
        }
    }
}

// TODO: add a converter for Declare transactions as well.

impl From<InvokeTransaction> for RpcInvokeTransactionV3 {
    fn from(tx: InvokeTransaction) -> Self {
        Self {
            sender_address: tx.sender_address(),
            tip: tx.tip(),
            nonce: tx.nonce(),
            resource_bounds: match tx.resource_bounds() {
                ValidResourceBounds::AllResources(all_resource_bounds) => all_resource_bounds,
                ValidResourceBounds::L1Gas(l1_gas) => {
                    AllResourceBounds { l1_gas, ..Default::default() }
                }
            },
            signature: tx.signature(),
            calldata: tx.calldata(),
            nonce_data_availability_mode: tx.nonce_data_availability_mode(),
            fee_data_availability_mode: tx.fee_data_availability_mode(),
            paymaster_data: tx.paymaster_data(),
            account_deployment_data: tx.account_deployment_data(),
        }
    }
}

impl From<DeployAccountTransaction> for RpcDeployAccountTransactionV3 {
    fn from(tx: DeployAccountTransaction) -> Self {
        Self {
            class_hash: tx.class_hash(),
            constructor_calldata: tx.constructor_calldata(),
            contract_address_salt: tx.contract_address_salt(),
            nonce: tx.nonce(),
            signature: tx.signature(),
            resource_bounds: match tx.resource_bounds() {
                ValidResourceBounds::AllResources(all_resource_bounds) => all_resource_bounds,
                ValidResourceBounds::L1Gas(l1_gas) => {
                    AllResourceBounds { l1_gas, ..Default::default() }
                }
            },
            tip: tx.tip(),
            nonce_data_availability_mode: tx.nonce_data_availability_mode(),
            fee_data_availability_mode: tx.fee_data_availability_mode(),
            paymaster_data: tx.paymaster_data(),
        }
    }
}

// TODO(Mohammad): Add constructor for all the transaction's structs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeclareTransaction {
    pub tx: crate::transaction::DeclareTransaction,
    pub tx_hash: TransactionHash,
    pub class_info: ClassInfo,
}

impl DeclareTransaction {
    implement_inner_tx_getter_calls!(
        (class_hash, ClassHash),
        (nonce, Nonce),
        (sender_address, ContractAddress),
        (signature, TransactionSignature),
        (version, TransactionVersion)
    );

    pub fn create(
        declare_tx: crate::transaction::DeclareTransaction,
        class_info: ClassInfo,
        chain_id: &ChainId,
    ) -> Result<Self, StarknetApiError> {
        validate_class_version_matches_tx_version(
            declare_tx.version(),
            &class_info.contract_class,
        )?;
        let tx_hash = declare_tx.calculate_transaction_hash(chain_id, &declare_tx.version())?;
        Ok(Self { tx: declare_tx, tx_hash, class_info })
    }

    /// Validates that the compiled class hash of the compiled class matches the supplied
    /// compiled class hash.
    /// Relevant only for version 3 transactions.
    pub fn validate_compiled_class_hash(&self) -> bool {
        let supplied_compiled_class_hash = match &self.tx {
            crate::transaction::DeclareTransaction::V3(tx) => tx.compiled_class_hash,
            crate::transaction::DeclareTransaction::V2(tx) => tx.compiled_class_hash,
            crate::transaction::DeclareTransaction::V1(_)
            | crate::transaction::DeclareTransaction::V0(_) => return true,
        };

        let contract_class = &self.class_info.contract_class;
        let compiled_class_hash = contract_class.compiled_class_hash();

        compiled_class_hash == supplied_compiled_class_hash
    }

    pub fn contract_class(&self) -> ContractClass {
        self.class_info.contract_class.clone()
    }
}

/// Validates that the Declare transaction version is compatible with the Cairo contract version.
/// Versions 0 and 1 declare Cairo 0 contracts, while versions >=2 declare Cairo 1 contracts.
fn validate_class_version_matches_tx_version(
    declare_version: TransactionVersion,
    class: &ContractClass,
) -> Result<(), StarknetApiError> {
    let expected_cairo_version = if declare_version <= TransactionVersion::ONE { 0 } else { 1 };

    match class {
        ContractClass::V0(_) if expected_cairo_version == 0 => Ok(()),
        ContractClass::V1(_) if expected_cairo_version == 1 => Ok(()),
        _ => Err(StarknetApiError::ContractClassVersionMismatch {
            declare_version,
            cairo_version: expected_cairo_version,
        }),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeployAccountTransaction {
    pub tx: crate::transaction::DeployAccountTransaction,
    pub tx_hash: TransactionHash,
    pub contract_address: ContractAddress,
}

impl DeployAccountTransaction {
    implement_inner_tx_getter_calls!(
        (class_hash, ClassHash),
        (constructor_calldata, Calldata),
        (contract_address_salt, ContractAddressSalt),
        (nonce, Nonce),
        (signature, TransactionSignature),
        (version, TransactionVersion),
        (resource_bounds, ValidResourceBounds),
        (tip, Tip),
        (nonce_data_availability_mode, DataAvailabilityMode),
        (fee_data_availability_mode, DataAvailabilityMode),
        (paymaster_data, PaymasterData)
    );
    implement_getter_calls!((tx_hash, TransactionHash), (contract_address, ContractAddress));

    pub fn tx(&self) -> &crate::transaction::DeployAccountTransaction {
        &self.tx
    }

    pub fn create(
        deploy_account_tx: crate::transaction::DeployAccountTransaction,
        chain_id: &ChainId,
    ) -> Result<Self, StarknetApiError> {
        let contract_address = calculate_contract_address(
            deploy_account_tx.contract_address_salt(),
            deploy_account_tx.class_hash(),
            &deploy_account_tx.constructor_calldata(),
            ContractAddress::default(),
        )?;
        let tx_hash =
            deploy_account_tx.calculate_transaction_hash(chain_id, &deploy_account_tx.version())?;
        Ok(Self { tx: deploy_account_tx, tx_hash, contract_address })
    }

    pub fn from_rpc_tx(
        rpc_tx: RpcDeployAccountTransaction,
        chain_id: &ChainId,
    ) -> Result<Self, StarknetApiError> {
        let deploy_account_tx: crate::transaction::DeployAccountTransaction = rpc_tx.into();
        Self::create(deploy_account_tx, chain_id)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InvokeTransaction {
    pub tx: crate::transaction::InvokeTransaction,
    pub tx_hash: TransactionHash,
}

impl InvokeTransaction {
    implement_inner_tx_getter_calls!(
        (calldata, Calldata),
        (nonce, Nonce),
        (signature, TransactionSignature),
        (sender_address, ContractAddress),
        (version, TransactionVersion),
        (resource_bounds, ValidResourceBounds),
        (tip, Tip),
        (nonce_data_availability_mode, DataAvailabilityMode),
        (fee_data_availability_mode, DataAvailabilityMode),
        (paymaster_data, PaymasterData),
        (account_deployment_data, AccountDeploymentData)
    );
    implement_getter_calls!((tx_hash, TransactionHash));

    pub fn tx(&self) -> &crate::transaction::InvokeTransaction {
        &self.tx
    }

    pub fn create(
        invoke_tx: crate::transaction::InvokeTransaction,
        chain_id: &ChainId,
    ) -> Result<Self, StarknetApiError> {
        let tx_hash = invoke_tx.calculate_transaction_hash(chain_id, &invoke_tx.version())?;
        Ok(Self { tx: invoke_tx, tx_hash })
    }

    pub fn from_rpc_tx(
        rpc_tx: RpcInvokeTransaction,
        chain_id: &ChainId,
    ) -> Result<Self, StarknetApiError> {
        let invoke_tx: crate::transaction::InvokeTransaction = rpc_tx.into();
        Self::create(invoke_tx, chain_id)
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct L1HandlerTransaction {
    pub tx: crate::transaction::L1HandlerTransaction,
    pub tx_hash: TransactionHash,
    pub paid_fee_on_l1: Fee,
}

impl L1HandlerTransaction {
    pub fn payload_size(&self) -> usize {
        // The calldata includes the "from" field, which is not a part of the payload.
        self.tx.calldata.0.len() - 1
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum Transaction {
    Account(AccountTransaction),
    L1Handler(L1HandlerTransaction),
}

impl Transaction {
    pub fn tx_hash(&self) -> TransactionHash {
        match self {
            Transaction::Account(tx) => tx.tx_hash(),
            Transaction::L1Handler(tx) => tx.tx_hash,
        }
    }
}
