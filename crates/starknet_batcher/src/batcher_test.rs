use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use assert_matches::assert_matches;
use async_trait::async_trait;
use blockifier::abi::constants;
use blockifier::test_utils::struct_impls::BlockInfoExt;
use chrono::Utc;
use futures::future::BoxFuture;
use futures::FutureExt;
use indexmap::indexmap;
use mockall::automock;
use mockall::predicate::{always, eq};
use rstest::rstest;
use starknet_api::block::{BlockInfo, BlockNumber};
use starknet_api::core::{ContractAddress, Nonce, StateDiffCommitment};
use starknet_api::executable_transaction::Transaction;
use starknet_api::hash::PoseidonHash;
use starknet_api::state::ThinStateDiff;
use starknet_api::transaction::TransactionHash;
use starknet_api::{contract_address, felt, nonce, tx_hash};
use starknet_batcher_types::batcher_types::{
    DecisionReachedInput,
    GetHeightResponse,
    GetProposalContent,
    GetProposalContentInput,
    GetProposalContentResponse,
    ProposalCommitment,
    ProposalId,
    ProposalStatus,
    ProposeBlockInput,
    SendProposalContent,
    SendProposalContentInput,
    SendProposalContentResponse,
    StartHeightInput,
    ValidateBlockInput,
};
use starknet_batcher_types::errors::BatcherError;
use starknet_l1_provider_types::MockL1ProviderClient;
use starknet_mempool_types::communication::MockMempoolClient;
use starknet_mempool_types::mempool_types::CommitBlockArgs;
use starknet_state_sync_types::state_sync_types::SyncBlock;
use tokio::sync::Mutex;

use crate::batcher::{Batcher, MockBatcherStorageReaderTrait, MockBatcherStorageWriterTrait};
use crate::block_builder::{
    AbortSignalSender,
    BlockBuilderError,
    BlockBuilderTrait,
    FailOnErrorCause,
    MockBlockBuilderFactoryTrait,
    MockBlockBuilderTrait,
};
use crate::config::BatcherConfig;
use crate::proposal_manager::{GenerateProposalError, ProposalManagerTrait};
use crate::test_utils::test_txs;
use crate::transaction_provider::NextTxs;
use crate::utils::{ProposalOutput, ProposalResult};

const INITIAL_HEIGHT: BlockNumber = BlockNumber(3);
const STREAMING_CHUNK_SIZE: usize = 3;
const BLOCK_GENERATION_TIMEOUT: tokio::time::Duration = tokio::time::Duration::from_secs(1);
const PROPOSAL_ID: ProposalId = ProposalId(0);

fn initial_block_info() -> BlockInfo {
    BlockInfo { block_number: INITIAL_HEIGHT, ..BlockInfo::create_for_testing() }
}

fn proposal_commitment() -> ProposalCommitment {
    ProposalCommitment {
        state_diff_commitment: StateDiffCommitment(PoseidonHash(felt!(u128::try_from(7).unwrap()))),
    }
}

fn proposal_output() -> ProposalOutput {
    ProposalOutput { commitment: proposal_commitment(), ..Default::default() }
}

fn deadline() -> chrono::DateTime<Utc> {
    chrono::Utc::now() + BLOCK_GENERATION_TIMEOUT
}

fn propose_block_input() -> ProposeBlockInput {
    ProposeBlockInput {
        proposal_id: PROPOSAL_ID,
        deadline: deadline(),
        retrospective_block_hash: None,
        block_info: initial_block_info(),
    }
}

fn validate_block_input() -> ValidateBlockInput {
    ValidateBlockInput {
        proposal_id: PROPOSAL_ID,
        deadline: deadline(),
        retrospective_block_hash: None,
        block_info: initial_block_info(),
    }
}

fn invalid_proposal_result() -> ProposalResult<ProposalOutput> {
    Err(Arc::new(BlockBuilderError::FailOnError(FailOnErrorCause::BlockFull)))
}

struct MockDependencies {
    storage_reader: MockBatcherStorageReaderTrait,
    storage_writer: MockBatcherStorageWriterTrait,
    mempool_client: MockMempoolClient,
    l1_provider_client: MockL1ProviderClient,
    proposal_manager: MockProposalManagerTraitWrapper,
    block_builder_factory: MockBlockBuilderFactoryTrait,
}

impl Default for MockDependencies {
    fn default() -> Self {
        let mut storage_reader = MockBatcherStorageReaderTrait::new();
        storage_reader.expect_height().returning(|| Ok(INITIAL_HEIGHT));
        Self {
            storage_reader,
            storage_writer: MockBatcherStorageWriterTrait::new(),
            l1_provider_client: MockL1ProviderClient::new(),
            mempool_client: MockMempoolClient::new(),
            proposal_manager: MockProposalManagerTraitWrapper::new(),
            block_builder_factory: MockBlockBuilderFactoryTrait::new(),
        }
    }
}

fn create_batcher(mock_dependencies: MockDependencies) -> Batcher {
    Batcher::new(
        BatcherConfig { outstream_content_buffer_size: STREAMING_CHUNK_SIZE, ..Default::default() },
        Arc::new(mock_dependencies.storage_reader),
        Box::new(mock_dependencies.storage_writer),
        Arc::new(mock_dependencies.l1_provider_client),
        Arc::new(mock_dependencies.mempool_client),
        Box::new(mock_dependencies.block_builder_factory),
        Box::new(mock_dependencies.proposal_manager),
    )
}

fn abort_signal_sender() -> AbortSignalSender {
    tokio::sync::oneshot::channel().0
}

fn mock_create_builder_for_validate_block() -> MockBlockBuilderFactoryTrait {
    let mut block_builder_factory = MockBlockBuilderFactoryTrait::new();
    block_builder_factory.expect_create_block_builder().times(1).return_once(
        |_, _, mut tx_provider, _| {
            // Spawn a task to keep tx_provider alive until all transactions are read.
            // Without this, the provider would be dropped, causing the batcher to fail when sending
            // transactions to it during the test.
            tokio::spawn(async move {
                while tx_provider.get_txs(1).await.is_ok_and(|v| v != NextTxs::End) {
                    tokio::task::yield_now().await;
                }
            });
            Ok((Box::new(MockBlockBuilderTrait::new()), abort_signal_sender()))
        },
    );
    block_builder_factory
}

fn mock_create_builder_for_propose_block(
    output_txs: Vec<Transaction>,
) -> MockBlockBuilderFactoryTrait {
    let mut block_builder_factory = MockBlockBuilderFactoryTrait::new();
    block_builder_factory.expect_create_block_builder().times(1).return_once(
        |_, _, _, output_content_sender| {
            // Simulate the streaming of the block builder output.
            for tx in output_txs {
                output_content_sender.as_ref().unwrap().send(tx).unwrap();
            }
            Ok((Box::new(MockBlockBuilderTrait::new()), abort_signal_sender()))
        },
    );
    block_builder_factory
}

fn mock_start_proposal(proposal_manager: &mut MockProposalManagerTraitWrapper) {
    proposal_manager.expect_wrap_reset().times(1).return_once(|| async {}.boxed());
    proposal_manager
        .expect_wrap_spawn_proposal()
        .times(1)
        .with(eq(PROPOSAL_ID), always(), always())
        .return_once(|_, _, _| { async move { Ok(()) } }.boxed());
}

fn mock_completed_proposal(
    proposal_manager: &mut MockProposalManagerTraitWrapper,
    proposal_result: ProposalResult<ProposalOutput>,
) {
    proposal_manager.expect_wrap_get_completed_proposals().times(1).return_once(move || {
        async move { Arc::new(Mutex::new(HashMap::from([(PROPOSAL_ID, proposal_result)]))) }.boxed()
    });
}

async fn batcher_with_validated_proposal(
    proposal_result: ProposalResult<ProposalOutput>,
) -> Batcher {
    let block_builder_factory = mock_create_builder_for_validate_block();
    let mut proposal_manager = MockProposalManagerTraitWrapper::new();
    mock_start_proposal(&mut proposal_manager);
    mock_completed_proposal(&mut proposal_manager, proposal_result);
    proposal_manager.expect_wrap_get_active_proposal().returning(|| async move { None }.boxed());

    let mut batcher = create_batcher(MockDependencies {
        proposal_manager,
        block_builder_factory,
        ..Default::default()
    });

    batcher.start_height(StartHeightInput { height: INITIAL_HEIGHT }).await.unwrap();

    batcher.validate_block(validate_block_input()).await.unwrap();

    batcher
}

fn mock_proposal_manager_validate_flow() -> MockProposalManagerTraitWrapper {
    let mut proposal_manager = MockProposalManagerTraitWrapper::new();
    mock_start_proposal(&mut proposal_manager);
    proposal_manager
        .expect_wrap_get_active_proposal()
        .returning(|| async move { Some(PROPOSAL_ID) }.boxed());
    proposal_manager
        .expect_wrap_await_active_proposal()
        .times(1)
        .returning(|| async move { true }.boxed());
    mock_completed_proposal(&mut proposal_manager, Ok(proposal_output()));
    proposal_manager
}

#[rstest]
#[tokio::test]
async fn start_height_success() {
    let mut proposal_manager = MockProposalManagerTraitWrapper::new();
    proposal_manager.expect_wrap_reset().times(1).return_once(|| async {}.boxed());

    let mut batcher = create_batcher(MockDependencies { proposal_manager, ..Default::default() });
    assert_eq!(batcher.start_height(StartHeightInput { height: INITIAL_HEIGHT }).await, Ok(()));
}

#[rstest]
#[case::height_already_passed(
    INITIAL_HEIGHT.prev().unwrap(),
    BatcherError::HeightAlreadyPassed {
        storage_height: INITIAL_HEIGHT,
        requested_height: INITIAL_HEIGHT.prev().unwrap()
    }
)]
#[case::storage_not_synced(
    INITIAL_HEIGHT.unchecked_next(),
    BatcherError::StorageNotSynced {
        storage_height: INITIAL_HEIGHT,
        requested_height: INITIAL_HEIGHT.unchecked_next()
    }
)]
#[tokio::test]
async fn start_height_fail(#[case] height: BlockNumber, #[case] expected_error: BatcherError) {
    let mut proposal_manager = MockProposalManagerTraitWrapper::new();
    proposal_manager.expect_wrap_reset().never();

    let mut batcher = create_batcher(MockDependencies { proposal_manager, ..Default::default() });
    assert_eq!(batcher.start_height(StartHeightInput { height }).await, Err(expected_error));
}

#[rstest]
#[tokio::test]
async fn duplicate_start_height() {
    let mut proposal_manager = MockProposalManagerTraitWrapper::new();
    proposal_manager.expect_wrap_reset().times(1).return_once(|| async {}.boxed());

    let mut batcher = create_batcher(MockDependencies { proposal_manager, ..Default::default() });

    let initial_height = StartHeightInput { height: INITIAL_HEIGHT };
    assert_eq!(batcher.start_height(initial_height.clone()).await, Ok(()));
    assert_eq!(batcher.start_height(initial_height).await, Err(BatcherError::HeightInProgress));
}

#[rstest]
#[tokio::test]
async fn no_active_height() {
    let proposal_manager = MockProposalManagerTraitWrapper::new();
    let mut batcher = create_batcher(MockDependencies { proposal_manager, ..Default::default() });

    // Calling `propose_block` and `validate_block` without starting a height should fail.

    let result = batcher.propose_block(propose_block_input()).await;
    assert_eq!(result, Err(BatcherError::NoActiveHeight));

    let result = batcher.validate_block(validate_block_input()).await;
    assert_eq!(result, Err(BatcherError::NoActiveHeight));
}

#[rstest]
#[tokio::test]
async fn validate_block_full_flow() {
    let block_builder_factory = mock_create_builder_for_validate_block();
    let proposal_manager = mock_proposal_manager_validate_flow();
    let mut batcher = create_batcher(MockDependencies {
        proposal_manager,
        block_builder_factory,
        ..Default::default()
    });

    batcher.start_height(StartHeightInput { height: INITIAL_HEIGHT }).await.unwrap();
    batcher.validate_block(validate_block_input()).await.unwrap();

    let send_proposal_input_txs = SendProposalContentInput {
        proposal_id: PROPOSAL_ID,
        content: SendProposalContent::Txs(test_txs(0..1)),
    };
    let send_txs_result = batcher.send_proposal_content(send_proposal_input_txs).await.unwrap();
    assert_eq!(
        send_txs_result,
        SendProposalContentResponse { response: ProposalStatus::Processing }
    );

    let send_proposal_input_finish =
        SendProposalContentInput { proposal_id: PROPOSAL_ID, content: SendProposalContent::Finish };
    let send_finish_result =
        batcher.send_proposal_content(send_proposal_input_finish).await.unwrap();
    assert_eq!(
        send_finish_result,
        SendProposalContentResponse { response: ProposalStatus::Finished(proposal_commitment()) }
    );
}

#[rstest]
#[tokio::test]
async fn send_content_after_proposal_already_finished() {
    let successful_proposal_result = Ok(proposal_output());
    let mut batcher = batcher_with_validated_proposal(successful_proposal_result).await;

    // Send transactions after the proposal has finished.
    let send_proposal_input_txs = SendProposalContentInput {
        proposal_id: PROPOSAL_ID,
        content: SendProposalContent::Txs(test_txs(0..1)),
    };
    let result = batcher.send_proposal_content(send_proposal_input_txs).await;
    assert_eq!(result, Err(BatcherError::ProposalAlreadyFinished { proposal_id: PROPOSAL_ID }));
}

#[rstest]
#[tokio::test]
async fn send_content_to_unknown_proposal() {
    let mut batcher = create_batcher(MockDependencies::default());

    // Send transactions to an unknown proposal.
    let send_proposal_input_txs = SendProposalContentInput {
        proposal_id: PROPOSAL_ID,
        content: SendProposalContent::Txs(test_txs(0..1)),
    };
    let result = batcher.send_proposal_content(send_proposal_input_txs).await;
    assert_eq!(result, Err(BatcherError::ProposalNotFound { proposal_id: PROPOSAL_ID }));

    // Send finish to an unknown proposal.
    let send_proposal_input_txs =
        SendProposalContentInput { proposal_id: PROPOSAL_ID, content: SendProposalContent::Finish };
    let result = batcher.send_proposal_content(send_proposal_input_txs).await;
    assert_eq!(result, Err(BatcherError::ProposalNotFound { proposal_id: PROPOSAL_ID }));
}

#[rstest]
#[tokio::test]
async fn send_txs_to_an_invalid_proposal() {
    let mut batcher = batcher_with_validated_proposal(invalid_proposal_result()).await;

    let send_proposal_input_txs = SendProposalContentInput {
        proposal_id: PROPOSAL_ID,
        content: SendProposalContent::Txs(test_txs(0..1)),
    };
    let result = batcher.send_proposal_content(send_proposal_input_txs).await.unwrap();
    assert_eq!(result, SendProposalContentResponse { response: ProposalStatus::InvalidProposal });
}

#[rstest]
#[tokio::test]
async fn send_finish_to_an_invalid_proposal() {
    let mut batcher = batcher_with_validated_proposal(invalid_proposal_result()).await;

    let send_proposal_input_txs =
        SendProposalContentInput { proposal_id: PROPOSAL_ID, content: SendProposalContent::Finish };
    let result = batcher.send_proposal_content(send_proposal_input_txs).await.unwrap();
    assert_eq!(result, SendProposalContentResponse { response: ProposalStatus::InvalidProposal });
}

#[rstest]
#[tokio::test]
async fn propose_block_full_flow() {
    // Expecting 3 chunks of streamed txs.
    let expected_streamed_txs = test_txs(0..STREAMING_CHUNK_SIZE * 2 + 1);
    let txs_to_stream = expected_streamed_txs.clone();

    let block_builder_factory = mock_create_builder_for_propose_block(txs_to_stream);
    let mut proposal_manager = MockProposalManagerTraitWrapper::new();
    mock_start_proposal(&mut proposal_manager);
    mock_completed_proposal(&mut proposal_manager, Ok(proposal_output()));

    let mut batcher = create_batcher(MockDependencies {
        proposal_manager,
        block_builder_factory,
        ..Default::default()
    });

    batcher.start_height(StartHeightInput { height: INITIAL_HEIGHT }).await.unwrap();
    batcher.propose_block(propose_block_input()).await.unwrap();

    let expected_n_chunks = expected_streamed_txs.len().div_ceil(STREAMING_CHUNK_SIZE);
    let mut aggregated_streamed_txs = Vec::new();
    for _ in 0..expected_n_chunks {
        let content = batcher
            .get_proposal_content(GetProposalContentInput { proposal_id: PROPOSAL_ID })
            .await
            .unwrap()
            .content;
        let mut txs = assert_matches!(content, GetProposalContent::Txs(txs) => txs);
        assert!(txs.len() <= STREAMING_CHUNK_SIZE, "{} < {}", txs.len(), STREAMING_CHUNK_SIZE);
        aggregated_streamed_txs.append(&mut txs);
    }
    assert_eq!(aggregated_streamed_txs, expected_streamed_txs);

    let commitment = batcher
        .get_proposal_content(GetProposalContentInput { proposal_id: PROPOSAL_ID })
        .await
        .unwrap();
    assert_eq!(
        commitment,
        GetProposalContentResponse { content: GetProposalContent::Finished(proposal_commitment()) }
    );

    let exhausted =
        batcher.get_proposal_content(GetProposalContentInput { proposal_id: PROPOSAL_ID }).await;
    assert_matches!(exhausted, Err(BatcherError::ProposalNotFound { .. }));
}

#[rstest]
#[tokio::test]
async fn get_height() {
    let mut storage_reader = MockBatcherStorageReaderTrait::new();
    storage_reader.expect_height().returning(|| Ok(INITIAL_HEIGHT));

    let mut batcher = create_batcher(MockDependencies { storage_reader, ..Default::default() });

    let result = batcher.get_height().await.unwrap();
    assert_eq!(result, GetHeightResponse { height: INITIAL_HEIGHT });
}

#[rstest]
#[tokio::test]
async fn propose_block_without_retrospective_block_hash() {
    let mut proposal_manager = MockProposalManagerTraitWrapper::new();
    proposal_manager.expect_wrap_reset().times(1).return_once(|| async {}.boxed());

    let mut storage_reader = MockBatcherStorageReaderTrait::new();
    storage_reader
        .expect_height()
        .returning(|| Ok(BlockNumber(constants::STORED_BLOCK_HASH_BUFFER)));

    let mut batcher =
        create_batcher(MockDependencies { proposal_manager, storage_reader, ..Default::default() });

    batcher
        .start_height(StartHeightInput { height: BlockNumber(constants::STORED_BLOCK_HASH_BUFFER) })
        .await
        .unwrap();
    let result = batcher.propose_block(propose_block_input()).await;

    assert_matches!(result, Err(BatcherError::MissingRetrospectiveBlockHash));
}

#[rstest]
#[tokio::test]
async fn get_content_from_unknown_proposal() {
    let mut proposal_manager = MockProposalManagerTraitWrapper::new();
    proposal_manager.expect_wrap_get_completed_proposals().times(0);

    let mut batcher = create_batcher(MockDependencies { proposal_manager, ..Default::default() });

    let get_proposal_content_input = GetProposalContentInput { proposal_id: PROPOSAL_ID };
    let result = batcher.get_proposal_content(get_proposal_content_input).await;
    assert_eq!(result, Err(BatcherError::ProposalNotFound { proposal_id: PROPOSAL_ID }));
}

#[rstest]
#[tokio::test]
async fn add_sync_block() {
    let mut mock_dependencies = MockDependencies::default();

    mock_dependencies
        .storage_writer
        .expect_commit_proposal()
        .times(1)
        .with(eq(INITIAL_HEIGHT), eq(test_state_diff()))
        .returning(|_, _| Ok(()));

    mock_dependencies
        .mempool_client
        .expect_commit_block()
        .times(1)
        .with(eq(CommitBlockArgs {
            address_to_nonce: test_contract_nonces(),
            tx_hashes: test_tx_hashes(),
        }))
        .returning(|_| Ok(()));

    let mut batcher = create_batcher(mock_dependencies);

    let sync_block = SyncBlock {
        block_number: INITIAL_HEIGHT,
        state_diff: test_state_diff(),
        transaction_hashes: test_tx_hashes().into_iter().collect(),
    };
    batcher.add_sync_block(sync_block).await.unwrap();
}

#[rstest]
#[tokio::test]
async fn decision_reached() {
    let mut mock_dependencies = MockDependencies::default();

    mock_dependencies
        .proposal_manager
        .expect_wrap_take_proposal_result()
        .times(1)
        .with(eq(PROPOSAL_ID))
        .return_once(move |_| {
            async move {
                Some(Ok(ProposalOutput {
                    state_diff: test_state_diff(),
                    commitment: ProposalCommitment::default(),
                    tx_hashes: test_tx_hashes(),
                    nonces: test_contract_nonces(),
                }))
            }
            .boxed()
        });

    mock_dependencies
        .mempool_client
        .expect_commit_block()
        .times(1)
        .with(eq(CommitBlockArgs {
            address_to_nonce: test_contract_nonces(),
            tx_hashes: test_tx_hashes(),
        }))
        .returning(|_| Ok(()));

    mock_dependencies
        .storage_writer
        .expect_commit_proposal()
        .times(1)
        .with(eq(INITIAL_HEIGHT), eq(test_state_diff()))
        .returning(|_, _| Ok(()));

    let mut batcher = create_batcher(mock_dependencies);

    let response =
        batcher.decision_reached(DecisionReachedInput { proposal_id: PROPOSAL_ID }).await.unwrap();
    assert_eq!(response.state_diff, test_state_diff());
}

#[rstest]
#[tokio::test]
async fn decision_reached_no_executed_proposal() {
    let expected_error = BatcherError::ExecutedProposalNotFound { proposal_id: PROPOSAL_ID };

    let mut proposal_manager = MockProposalManagerTraitWrapper::new();
    proposal_manager
        .expect_wrap_take_proposal_result()
        .times(1)
        .with(eq(PROPOSAL_ID))
        .return_once(|_| async move { None }.boxed());

    let mut batcher = create_batcher(MockDependencies { proposal_manager, ..Default::default() });
    let decision_reached_result =
        batcher.decision_reached(DecisionReachedInput { proposal_id: PROPOSAL_ID }).await;
    assert_eq!(decision_reached_result, Err(expected_error));
}

// A wrapper trait to allow mocking the ProposalManagerTrait in tests.
#[automock]
trait ProposalManagerTraitWrapper: Send + Sync {
    fn wrap_spawn_proposal(
        &mut self,
        proposal_id: ProposalId,
        block_builder: Box<dyn BlockBuilderTrait>,
        abort_signal_sender: tokio::sync::oneshot::Sender<()>,
    ) -> BoxFuture<'_, Result<(), GenerateProposalError>>;

    fn wrap_take_proposal_result(
        &mut self,
        proposal_id: ProposalId,
    ) -> BoxFuture<'_, Option<ProposalResult<ProposalOutput>>>;

    fn wrap_get_active_proposal(&self) -> BoxFuture<'_, Option<ProposalId>>;

    #[allow(clippy::type_complexity)]
    fn wrap_get_completed_proposals(
        &self,
    ) -> BoxFuture<'_, Arc<tokio::sync::Mutex<HashMap<ProposalId, ProposalResult<ProposalOutput>>>>>;

    fn wrap_await_active_proposal(&mut self) -> BoxFuture<'_, bool>;

    fn wrap_abort_proposal(&mut self, proposal_id: ProposalId) -> BoxFuture<'_, ()>;

    fn wrap_reset(&mut self) -> BoxFuture<'_, ()>;
}

#[async_trait]
impl<T: ProposalManagerTraitWrapper> ProposalManagerTrait for T {
    async fn spawn_proposal(
        &mut self,
        proposal_id: ProposalId,
        block_builder: Box<dyn BlockBuilderTrait>,
        abort_signal_sender: tokio::sync::oneshot::Sender<()>,
    ) -> Result<(), GenerateProposalError> {
        self.wrap_spawn_proposal(proposal_id, block_builder, abort_signal_sender).await
    }

    async fn take_proposal_result(
        &mut self,
        proposal_id: ProposalId,
    ) -> Option<ProposalResult<ProposalOutput>> {
        self.wrap_take_proposal_result(proposal_id).await
    }

    async fn get_active_proposal(&self) -> Option<ProposalId> {
        self.wrap_get_active_proposal().await
    }

    async fn get_completed_proposals(
        &self,
    ) -> Arc<tokio::sync::Mutex<HashMap<ProposalId, ProposalResult<ProposalOutput>>>> {
        self.wrap_get_completed_proposals().await
    }

    async fn await_active_proposal(&mut self) -> bool {
        self.wrap_await_active_proposal().await
    }

    async fn abort_proposal(&mut self, proposal_id: ProposalId) {
        self.wrap_abort_proposal(proposal_id).await
    }

    async fn reset(&mut self) {
        self.wrap_reset().await
    }
}

fn test_tx_hashes() -> HashSet<TransactionHash> {
    (0..5u8).map(|i| tx_hash!(i + 12)).collect()
}

fn test_contract_nonces() -> HashMap<ContractAddress, Nonce> {
    HashMap::from_iter((0..3u8).map(|i| (contract_address!(i + 33), nonce!(i + 9))))
}

pub fn test_state_diff() -> ThinStateDiff {
    ThinStateDiff {
        storage_diffs: indexmap! {
            4u64.into() => indexmap! {
                5u64.into() => 6u64.into(),
                7u64.into() => 8u64.into(),
            },
            9u64.into() => indexmap! {
                10u64.into() => 11u64.into(),
            },
        },
        nonces: test_contract_nonces().into_iter().collect(),
        ..Default::default()
    }
}
