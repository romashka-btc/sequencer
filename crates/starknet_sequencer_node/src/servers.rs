use std::future::pending;
use std::pin::Pin;

use futures::{Future, FutureExt};
use starknet_batcher::communication::{LocalBatcherServer, RemoteBatcherServer};
use starknet_consensus_manager::communication::ConsensusManagerServer;
use starknet_gateway::communication::{LocalGatewayServer, RemoteGatewayServer};
use starknet_http_server::communication::HttpServer;
use starknet_l1_provider::communication::{LocalL1ProviderServer, RemoteL1ProviderServer};
use starknet_mempool::communication::{LocalMempoolServer, RemoteMempoolServer};
use starknet_mempool_p2p::propagator::{
    LocalMempoolP2pPropagatorServer,
    RemoteMempoolP2pPropagatorServer,
};
use starknet_mempool_p2p::runner::MempoolP2pRunnerServer;
use starknet_monitoring_endpoint::communication::MonitoringEndpointServer;
use starknet_sequencer_infra::component_server::{
    ComponentServerStarter,
    LocalComponentServer,
    RemoteComponentServer,
    WrapperServer,
};
use starknet_sequencer_infra::errors::ComponentServerError;
use starknet_state_sync::runner::StateSyncRunnerServer;
use starknet_state_sync::{LocalStateSyncServer, RemoteStateSyncServer};
use tracing::error;

use crate::clients::SequencerNodeClients;
use crate::communication::SequencerNodeCommunication;
use crate::components::SequencerNodeComponents;
use crate::config::component_execution_config::{
    ActiveComponentExecutionMode,
    ReactiveComponentExecutionMode,
};
use crate::config::node_config::SequencerNodeConfig;

// Component servers that can run locally.
struct LocalServers {
    pub(crate) batcher: Option<Box<LocalBatcherServer>>,
    pub(crate) gateway: Option<Box<LocalGatewayServer>>,
    pub(crate) l1_provider: Option<Box<LocalL1ProviderServer>>,
    pub(crate) mempool: Option<Box<LocalMempoolServer>>,
    pub(crate) mempool_p2p_propagator: Option<Box<LocalMempoolP2pPropagatorServer>>,
    pub(crate) state_sync: Option<Box<LocalStateSyncServer>>,
}

// Component servers that wrap a component without a server.
struct WrapperServers {
    pub(crate) consensus_manager: Option<Box<ConsensusManagerServer>>,
    pub(crate) http_server: Option<Box<HttpServer>>,
    pub(crate) monitoring_endpoint: Option<Box<MonitoringEndpointServer>>,
    pub(crate) mempool_p2p_runner: Option<Box<MempoolP2pRunnerServer>>,
    pub(crate) state_sync_runner: Option<Box<StateSyncRunnerServer>>,
}

// Component servers that can run remotely.
// TODO(Nadin): Remove pub from the struct and update the fields to be pub(crate).
pub struct RemoteServers {
    pub batcher: Option<Box<RemoteBatcherServer>>,
    pub gateway: Option<Box<RemoteGatewayServer>>,
    pub l1_provider: Option<Box<RemoteL1ProviderServer>>,
    pub mempool: Option<Box<RemoteMempoolServer>>,
    pub mempool_p2p_propagator: Option<Box<RemoteMempoolP2pPropagatorServer>>,
    pub state_sync: Option<Box<RemoteStateSyncServer>>,
}

pub struct SequencerNodeServers {
    local_servers: LocalServers,
    remote_servers: RemoteServers,
    wrapper_servers: WrapperServers,
}

/// A macro for creating a remote component server based on the component's execution mode.
/// Returns a remote server if the component is configured with Remote execution mode; otherwise,
/// returns None.
///
/// # Arguments
///
/// * `$execution_mode` - A reference to the component's execution mode, of type
///   `&ReactiveComponentExecutionMode`.
/// * `$local_client` - The local client to be used for the remote server initialization if the
///   execution mode is `Remote`.
/// * `$config` - The configuration for the remote server.
///
/// # Returns
///
/// An `Option<Box<RemoteComponentServer<LocalClientType, RequestType, ResponseType>>>` containing
/// the remote server if the execution mode is Remote, or None if the execution mode is Disabled,
/// LocalExecutionWithRemoteEnabled or LocalExecutionWithRemoteDisabled.
///
/// # Example
///
/// ```rust,ignore
/// let batcher_remote_server = create_remote_server!(
///     &config.components.batcher.execution_mode,
///     clients.get_gateway_local_client(),
///     config.remote_server_config
/// );
/// match batcher_remote_server {
///     Some(server) => println!("Remote server created: {:?}", server),
///     None => println!("Remote server not created because the execution mode is not remote."),
/// }
/// ```
#[macro_export]
macro_rules! create_remote_server {
    ($execution_mode:expr, $local_client:expr, $remote_server_config:expr) => {
        match *$execution_mode {
            ReactiveComponentExecutionMode::LocalExecutionWithRemoteEnabled => {
                let local_client = $local_client
                    .expect("Local client should be set for inbound remote connections.");
                let remote_server_config = $remote_server_config
                    .as_ref()
                    .expect("Remote server config should be set for inbound remote connections.");

                Some(Box::new(RemoteComponentServer::new(
                    local_client,
                    remote_server_config.clone(),
                )))
            }
            ReactiveComponentExecutionMode::LocalExecutionWithRemoteDisabled
            | ReactiveComponentExecutionMode::Remote
            | ReactiveComponentExecutionMode::Disabled => None,
        }
    };
}

/// A macro for creating a component server, determined by the component's execution mode. Returns a
/// local server if the component is run locally, otherwise None.
///
/// # Arguments
///
/// * $execution_mode - A reference to the component's execution mode, i.e., type
///   &ReactiveComponentExecutionMode.
/// * $component - The component that will be taken to initialize the server if the execution mode
///   is enabled(LocalExecutionWithRemoteDisabled / LocalExecutionWithRemoteEnabled).
/// * $Receiver - receiver side for the server.
///
/// # Returns
///
/// An Option<Box<LocalComponentServer<ComponentType, RequestType, ResponseType>>> containing the
/// server if the execution mode is enabled(LocalExecutionWithRemoteDisabled /
/// LocalExecutionWithRemoteEnabled), or None if the execution mode is Disabled.
///
/// # Example
///
/// ```rust,ignore
/// let batcher_server = create_local_server!(
///     &config.components.batcher.execution_mode,
///     components.batcher,
///     communication.take_batcher_rx()
/// );
/// match batcher_server {
///     Some(server) => println!("Server created: {:?}", server),
///     None => println!("Server not created because the execution mode is disabled."),
/// }
/// ```
macro_rules! create_local_server {
    ($execution_mode:expr, $component:expr, $receiver:expr) => {
        match *$execution_mode {
            ReactiveComponentExecutionMode::LocalExecutionWithRemoteDisabled
            | ReactiveComponentExecutionMode::LocalExecutionWithRemoteEnabled => {
                Some(Box::new(LocalComponentServer::new(
                    $component
                        .take()
                        .expect(concat!(stringify!($component), " is not initialized.")),
                    $receiver,
                )))
            }
            ReactiveComponentExecutionMode::Disabled | ReactiveComponentExecutionMode::Remote => {
                None
            }
        }
    };
}

/// A macro for creating a WrapperServer, determined by the component's execution mode. Returns a
/// wrapper server if the component is run locally, otherwise None.
///
/// # Arguments
///
/// * $execution_mode - A reference to the component's execution mode, i.e., type
///   &ReactiveComponentExecutionMode.
/// * $component - The component that will be taken to initialize the server if the execution mode
///   is enabled(LocalExecutionWithRemoteDisabled / LocalExecutionWithRemoteEnabled).
///
/// # Returns
///
/// An `Option<Box<WrapperServer<ComponentType>>>` containing the server if the execution mode is
/// enabled(LocalExecutionWithRemoteDisabled / LocalExecutionWithRemoteEnabled), or `None` if the
/// execution mode is `Disabled`.
///
/// # Example
///
/// ```rust, ignore
/// // Assuming ReactiveComponentExecutionMode and components are defined, and WrapperServer
/// // has a new method that accepts a component.
/// let consensus_manager_server = create_wrapper_server!(
///     &config.components.consensus_manager.execution_mode,
///     components.consensus_manager
/// );
///
/// match consensus_manager_server {
///     Some(server) => println!("Server created: {:?}", server),
///     None => println!("Server not created because the execution mode is disabled."),
/// }
/// ```
macro_rules! create_wrapper_server {
    ($execution_mode:expr, $component:expr) => {
        match *$execution_mode {
            ActiveComponentExecutionMode::Enabled => Some(Box::new(WrapperServer::new(
                $component.take().expect(concat!(stringify!($component), " is not initialized.")),
            ))),
            ActiveComponentExecutionMode::Disabled => None,
        }
    };
}

fn create_local_servers(
    config: &SequencerNodeConfig,
    communication: &mut SequencerNodeCommunication,
    components: &mut SequencerNodeComponents,
) -> LocalServers {
    let batcher_server = create_local_server!(
        &config.components.batcher.execution_mode,
        components.batcher,
        communication.take_batcher_rx()
    );
    let gateway_server = create_local_server!(
        &config.components.gateway.execution_mode,
        components.gateway,
        communication.take_gateway_rx()
    );
    let l1_provider_server = create_local_server!(
        &config.components.l1_provider.execution_mode,
        components.l1_provider,
        communication.take_l1_provider_rx()
    );
    let mempool_server = create_local_server!(
        &config.components.mempool.execution_mode,
        components.mempool,
        communication.take_mempool_rx()
    );
    let mempool_p2p_propagator_server = create_local_server!(
        &config.components.mempool_p2p.execution_mode,
        components.mempool_p2p_propagator,
        communication.take_mempool_p2p_propagator_rx()
    );
    let state_sync_server = create_local_server!(
        &config.components.state_sync.execution_mode,
        components.state_sync,
        communication.take_state_sync_rx()
    );

    LocalServers {
        batcher: batcher_server,
        gateway: gateway_server,
        l1_provider: l1_provider_server,
        mempool: mempool_server,
        mempool_p2p_propagator: mempool_p2p_propagator_server,
        state_sync: state_sync_server,
    }
}

pub fn create_remote_servers(
    config: &SequencerNodeConfig,
    clients: &SequencerNodeClients,
) -> RemoteServers {
    let batcher_client = clients.get_batcher_local_client();
    let batcher_server = create_remote_server!(
        &config.components.batcher.execution_mode,
        batcher_client,
        config.components.batcher.remote_server_config
    );

    let gateway_client = clients.get_gateway_local_client();
    let gateway_server = create_remote_server!(
        &config.components.gateway.execution_mode,
        gateway_client,
        config.components.gateway.remote_server_config
    );

    let l1_provider_client = clients.get_l1_provider_local_client();
    let l1_provider_server = create_remote_server!(
        &config.components.l1_provider.execution_mode,
        l1_provider_client,
        config.components.l1_provider.remote_server_config
    );

    let mempool_client = clients.get_mempool_local_client();
    let mempool_server = create_remote_server!(
        &config.components.mempool.execution_mode,
        mempool_client,
        config.components.mempool.remote_server_config
    );

    let mempool_p2p_propagator_client = clients.get_mempool_p2p_propagator_local_client();
    let mempool_p2p_propagator_server = create_remote_server!(
        &config.components.mempool_p2p.execution_mode,
        mempool_p2p_propagator_client,
        config.components.mempool_p2p.remote_server_config
    );
    let state_sync_client = clients.get_state_sync_local_client();
    let state_sync_server = create_remote_server!(
        &config.components.state_sync.execution_mode,
        state_sync_client,
        config.components.state_sync.remote_server_config
    );

    RemoteServers {
        batcher: batcher_server,
        gateway: gateway_server,
        l1_provider: l1_provider_server,
        mempool: mempool_server,
        mempool_p2p_propagator: mempool_p2p_propagator_server,
        state_sync: state_sync_server,
    }
}

fn create_wrapper_servers(
    config: &SequencerNodeConfig,
    components: &mut SequencerNodeComponents,
) -> WrapperServers {
    let consensus_manager_server = create_wrapper_server!(
        &config.components.consensus_manager.execution_mode,
        components.consensus_manager
    );
    let http_server = create_wrapper_server!(
        &config.components.http_server.execution_mode,
        components.http_server
    );

    let monitoring_endpoint_server = create_wrapper_server!(
        &config.components.monitoring_endpoint.execution_mode,
        components.monitoring_endpoint
    );

    let mempool_p2p_runner_server = create_wrapper_server!(
        &config.components.mempool_p2p.execution_mode.clone().into(),
        components.mempool_p2p_runner
    );
    let state_sync_runner_server = create_wrapper_server!(
        &config.components.state_sync.execution_mode.clone().into(),
        components.state_sync_runner
    );

    WrapperServers {
        consensus_manager: consensus_manager_server,
        http_server,
        monitoring_endpoint: monitoring_endpoint_server,
        mempool_p2p_runner: mempool_p2p_runner_server,
        state_sync_runner: state_sync_runner_server,
    }
}

pub fn create_node_servers(
    config: &SequencerNodeConfig,
    communication: &mut SequencerNodeCommunication,
    components: SequencerNodeComponents,
    clients: &SequencerNodeClients,
) -> SequencerNodeServers {
    let mut components = components;
    let local_servers = create_local_servers(config, communication, &mut components);
    let remote_servers = create_remote_servers(config, clients);
    let wrapper_servers = create_wrapper_servers(config, &mut components);

    SequencerNodeServers { local_servers, remote_servers, wrapper_servers }
}

// TODO(Nadin): refactor this function to reduce code duplication.
pub async fn run_component_servers(servers: SequencerNodeServers) -> anyhow::Result<()> {
    // Batcher servers.
    let local_batcher_future = get_server_future(servers.local_servers.batcher);
    let remote_batcher_future = get_server_future(servers.remote_servers.batcher);

    // Consensus Manager server.
    let consensus_manager_future = get_server_future(servers.wrapper_servers.consensus_manager);

    // Gateway servers.
    let local_gateway_future = get_server_future(servers.local_servers.gateway);
    let remote_gateway_future = get_server_future(servers.remote_servers.gateway);

    // HttpServer server.
    let http_server_future = get_server_future(servers.wrapper_servers.http_server);

    // Mempool servers.
    let local_mempool_future = get_server_future(servers.local_servers.mempool);
    let remote_mempool_future = get_server_future(servers.remote_servers.mempool);

    // Sequencer Monitoring server.
    let monitoring_endpoint_future = get_server_future(servers.wrapper_servers.monitoring_endpoint);

    // MempoolP2pPropagator servers.
    let local_mempool_p2p_propagator_future =
        get_server_future(servers.local_servers.mempool_p2p_propagator);
    let remote_mempool_p2p_propagator_future =
        get_server_future(servers.remote_servers.mempool_p2p_propagator);

    // MempoolP2pRunner server.
    let mempool_p2p_runner_future = get_server_future(servers.wrapper_servers.mempool_p2p_runner);

    // StateSync servers.
    let local_state_sync_future = get_server_future(servers.local_servers.state_sync);
    let remote_state_sync_future = get_server_future(servers.remote_servers.state_sync);

    // StateSyncRunner server.
    let state_sync_runner_future = get_server_future(servers.wrapper_servers.state_sync_runner);

    // L1Provider server.
    let local_l1_provider_future = get_server_future(servers.local_servers.l1_provider);
    let remote_l1_provider_future = get_server_future(servers.remote_servers.l1_provider);

    // Start servers.
    let local_batcher_handle = tokio::spawn(local_batcher_future);
    let remote_batcher_handle = tokio::spawn(remote_batcher_future);
    let consensus_manager_handle = tokio::spawn(consensus_manager_future);
    let local_gateway_handle = tokio::spawn(local_gateway_future);
    let remote_gateway_handle = tokio::spawn(remote_gateway_future);
    let http_server_handle = tokio::spawn(http_server_future);
    let local_mempool_handle = tokio::spawn(local_mempool_future);
    let remote_mempool_handle = tokio::spawn(remote_mempool_future);
    let monitoring_endpoint_handle = tokio::spawn(monitoring_endpoint_future);
    let local_mempool_p2p_propagator_handle = tokio::spawn(local_mempool_p2p_propagator_future);
    let remote_mempool_p2p_propagator_handle = tokio::spawn(remote_mempool_p2p_propagator_future);
    let mempool_p2p_runner_handle = tokio::spawn(mempool_p2p_runner_future);
    let local_state_sync_handle = tokio::spawn(local_state_sync_future);
    let remote_state_sync_handle = tokio::spawn(remote_state_sync_future);
    let state_sync_runner_handle = tokio::spawn(state_sync_runner_future);
    let local_l1_provider_handle = tokio::spawn(local_l1_provider_future);
    let remote_l1_provider_handle = tokio::spawn(remote_l1_provider_future);

    let result = tokio::select! {
        res = local_batcher_handle => {
            error!("Local Batcher Server stopped.");
            res?
        }
        res = remote_batcher_handle => {
            error!("Remote Batcher Server stopped.");
            res?
        }
        res = consensus_manager_handle => {
            error!("Consensus Manager Server stopped.");
            res?
        }
        res = local_gateway_handle => {
            error!("Local Gateway Server stopped.");
            res?
        }
        res = remote_gateway_handle => {
            error!("Remote Gateway Server stopped.");
            res?
        }
        res = http_server_handle => {
            error!("Http Server stopped.");
            res?
        }
        res = local_mempool_handle => {
            error!("Local Mempool Server stopped.");
            res?
        }
        res = remote_mempool_handle => {
            error!("Remote Mempool Server stopped.");
            res?
        }
        res = monitoring_endpoint_handle => {
            error!("Monitoring Endpoint Server stopped.");
            res?
        }
        res = local_mempool_p2p_propagator_handle => {
            error!("Local Mempool P2P Propagator Server stopped.");
            res?
        }
        res = remote_mempool_p2p_propagator_handle => {
            error!("Remote Mempool P2P Propagator Server stopped.");
            res?
        }
        res = mempool_p2p_runner_handle => {
            error!("Mempool P2P Runner Server stopped.");
            res?
        }
        res = local_state_sync_handle => {
            error!("Local State Sync Server stopped.");
            res?
        }
        res = remote_state_sync_handle => {
            error!("Remote State Sync Server stopped.");
            res?
        }
        res = state_sync_runner_handle => {
            error!("State Sync Runner Server stopped.");
            res?
        }
        res = local_l1_provider_handle => {
            error!("Local L1 Provider Server stopped.");
            res?
        }
        res = remote_l1_provider_handle => {
            error!("Remote L1 Provider Server stopped.");
            res?
        }
    };
    error!("Servers ended with unexpected Ok.");

    Ok(result?)
}

pub fn get_server_future(
    server: Option<Box<impl ComponentServerStarter + Send + 'static>>,
) -> Pin<Box<dyn Future<Output = Result<(), ComponentServerError>> + Send>> {
    match server {
        Some(mut server) => async move { server.start().await }.boxed(),
        None => pending().boxed(),
    }
}
