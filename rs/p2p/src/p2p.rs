//! The P2P module exposes the peer-to-peer functionality.
//!
//! Specifically, it constructs all the artifact pools and the Consensus/P2P
//! time source.

use crate::gossip_protocol::{Gossip, GossipImpl};
use crate::{
    event_handler::IngressEventHandlerImpl,
    event_handler::{
        AdvertSubscriber, IngressThrottler, P2PEventHandlerControl, P2PEventHandlerImpl,
    },
};
use ic_artifact_manager::{manager, processors};
use ic_artifact_pool::{
    certification_pool::CertificationPoolImpl, consensus_pool::ConsensusPoolImpl,
    dkg_pool::DkgPoolImpl, ensure_persistent_pool_replica_version_compatibility,
    ingress_pool::IngressPoolImpl,
};
use ic_base_thread::async_safe_block_on_await;
use ic_config::{artifact_pool::ArtifactPoolConfig, consensus::ConsensusConfig};
use ic_consensus::{
    certification,
    consensus::{ConsensusCrypto, Membership},
    dkg,
};
use ic_crypto_tls_interfaces::TlsHandshake;
use ic_cycles_account_manager::CyclesAccountManager;
use ic_ingress_manager::IngressManager;
use ic_interfaces::{
    artifact_manager::{ArtifactClient, ArtifactManager, ArtifactProcessor},
    consensus_pool::ConsensusPoolCache,
    crypto::{Crypto, IngressSigVerifier},
    execution_environment::IngressHistoryReader,
    messaging::{MessageRouting, XNetPayloadBuilder},
    p2p::{IngressEventHandler, P2PRunner},
    registry::RegistryClient,
    state_manager::StateManager,
    time_source::SysTimeSource,
    transport::Transport,
};
use ic_logger::{debug, replica_logger::ReplicaLogger};
use ic_metrics::MetricsRegistry;
use ic_protobuf::registry::subnet::v1::GossipConfig;
use ic_registry_client::helper::subnet::SubnetRegistry;
use ic_replicated_state::ReplicatedState;
use ic_state_manager::StateManagerImpl;
use ic_transport::transport::create_transport;
use ic_types::{
    artifact::{Advert, ArtifactKind, ArtifactTag, FileTreeSyncAttribute},
    consensus::catchup::CUPWithOriginalProtobuf,
    crypto::CryptoHash,
    filetree_sync::{FileTreeSyncArtifact, FileTreeSyncId},
    p2p,
    replica_config::ReplicaConfig,
    transport::{FlowTag, TransportClientType, TransportConfig},
    NodeId, SubnetId,
};
use std::sync::{
    atomic::{AtomicBool, Ordering::SeqCst},
    Arc, RwLock,
};
use std::time::Duration;
use tokio::task::JoinHandle;

// import of malicious flags definition for p2p
use ic_interfaces::registry::LocalStoreCertifiedTimeReader;
use ic_types::malicious_flags::MaliciousFlags;

/// Periodic timer duration in milliseconds between polling calls to the P2P
/// component.
const P2P_TIMER_DURATION_MS: u64 = 100;

/// The P2P struct, which encapsulates all relevant components including gossip
/// and event handler control.
#[allow(unused)]
struct P2P {
    /// The logger.
    pub(crate) log: ReplicaLogger,
    /// Handle to the Tokio runtime to be used by p2p.
    rt_handle: tokio::runtime::Handle,
    /// The *Gossip* struct with automatic reference counting.
    gossip: Arc<GossipImpl>,
    /// The task handles.
    task_handles: Vec<JoinHandle<()>>,
    /// Flag indicating if P2P has been terminated.
    killed: Arc<AtomicBool>,
    /// The P2P event handler control with automatic reference counting.
    event_handler: Arc<dyn P2PEventHandlerControl>,
}

/// The P2P state sync client.
#[derive(Clone)]
pub enum P2PStateSyncClient {
    /// The main client variant.
    Client(Arc<StateManagerImpl>),
    /// The test client variant.
    TestClient(),
    /// The test chunking pool variant.
    TestChunkingPool(
        Arc<dyn ArtifactClient<TestArtifact>>,
        Arc<dyn ArtifactProcessor<TestArtifact> + Sync + 'static>,
    ),
}

/// Fetch the Gossip configuration from the registry.
pub(crate) fn fetch_gossip_config(
    registry_client: Arc<dyn RegistryClient>,
    subnet_id: SubnetId,
) -> GossipConfig {
    if let Ok(Some(Some(gossip_config))) =
        registry_client.get_gossip_config(subnet_id, registry_client.get_latest_version())
    {
        gossip_config
    } else {
        p2p::build_default_gossip_config()
    }
}

/// The function constructs a P2P instance. Currently, it constructs all the
/// artifact pools and the Consensus/P2P time source. Artifact
/// clients are constructed and run in their separate actors.
#[allow(
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::new_ret_no_self
)]
pub fn create_networking_stack(
    metrics_registry: MetricsRegistry,
    log: ReplicaLogger,
    rt_handle: tokio::runtime::Handle,
    transport_config: TransportConfig,
    artifact_pool_config: ArtifactPoolConfig,
    consensus_config: ConsensusConfig,
    malicious_flags: MaliciousFlags,
    node_id: NodeId,
    subnet_id: SubnetId,
    // For testing purposes the caller can pass a transport object instead. Otherwise, the callee
    // constructs it from the 'transport_config'.
    transport: Option<Arc<dyn Transport>>,
    tls_handshake: Arc<dyn TlsHandshake + Send + Sync>,
    state_manager: Arc<dyn StateManager<State = ReplicatedState>>,
    state_sync_client: P2PStateSyncClient,
    xnet_payload_builder: Arc<dyn XNetPayloadBuilder>,
    message_router: Arc<dyn MessageRouting>,
    crypto: Arc<dyn Crypto + Send + Sync>,
    consensus_crypto: Arc<dyn ConsensusCrypto + Send + Sync>,
    certifier_crypto: Arc<dyn certification::CertificationCrypto + Send + Sync>,
    ingress_sig_crypto: Arc<dyn IngressSigVerifier + Send + Sync>,
    registry_client: Arc<dyn RegistryClient>,
    ingress_history_reader: Box<dyn IngressHistoryReader>,
    catch_up_package: CUPWithOriginalProtobuf,
    cycles_account_manager: Arc<CyclesAccountManager>,
    local_store_time_reader: Option<Arc<dyn LocalStoreCertifiedTimeReader>>,
    registry_poll_delay_duration_ms: u64,
) -> Result<
    (
        Arc<dyn IngressEventHandler>,
        Box<dyn P2PRunner>,
        Arc<dyn ConsensusPoolCache>,
    ),
    String,
> {
    let transport = transport.unwrap_or_else(|| {
        create_transport(
            node_id,
            transport_config.clone(),
            registry_client.get_latest_version(),
            metrics_registry.clone(),
            tls_handshake,
            tokio::runtime::Handle::current(),
            log.clone(),
        )
    });
    let p2p_flow_tags = transport_config
        .p2p_flows
        .iter()
        .map(|flow_config| FlowTag::from(flow_config.flow_tag))
        .collect();

    let event_handler = Arc::new(P2PEventHandlerImpl::new(
        rt_handle.clone(),
        node_id,
        log.clone(),
        &metrics_registry,
        fetch_gossip_config(registry_client.clone(), subnet_id),
    ));
    transport
        .register_client(TransportClientType::P2P, event_handler.clone())
        .map_err(|e| format!("transport registration failed: {:?}", e))?;

    // Now we setup the Artifact Pools and the manager.
    let (artifact_manager, consensus_pool_cache, ingress_throttle) = setup_artifact_manager(
        rt_handle.clone(),
        node_id,
        Arc::clone(&crypto) as Arc<_>,
        Arc::clone(&consensus_crypto) as Arc<_>,
        Arc::clone(&certifier_crypto) as Arc<_>,
        Arc::clone(&ingress_sig_crypto) as Arc<_>,
        subnet_id,
        artifact_pool_config,
        consensus_config,
        log.clone(),
        metrics_registry.clone(),
        Arc::clone(&registry_client),
        state_manager,
        state_sync_client,
        xnet_payload_builder,
        message_router,
        ingress_history_reader,
        catch_up_package,
        malicious_flags.clone(),
        cycles_account_manager,
        local_store_time_reader,
        registry_poll_delay_duration_ms,
        Arc::clone(&event_handler) as Arc<_>,
    )
    .unwrap();

    let gossip = Arc::new(GossipImpl::new(
        node_id,
        subnet_id,
        registry_client.clone(),
        artifact_manager.clone(),
        transport.clone(),
        event_handler.clone(),
        p2p_flow_tags,
        log.clone(),
        &metrics_registry,
        malicious_flags,
    ));
    event_handler.start(gossip.clone());

    let p2p = P2P {
        log,
        rt_handle,
        gossip: gossip.clone(),
        task_handles: Vec::new(),
        killed: Arc::new(AtomicBool::new(false)),
        event_handler,
    };

    let ingress_handler = Arc::from(IngressEventHandlerImpl::new(
        ingress_throttle,
        gossip,
        node_id,
    ));
    Ok((ingress_handler, Box::new(p2p), consensus_pool_cache))
}

impl P2PRunner for P2P {
    /// The method starts the P2P timer task in the background.
    fn run(&mut self) {
        let gossip = self.gossip.clone();
        let event_handler = self.event_handler.clone();
        let log = self.log.clone();
        let killed = Arc::clone(&self.killed);
        let handle = self.rt_handle.spawn_blocking(move || {
            debug!(log, "P2P::p2p_timer(): started processing",);

            let timer_duration = Duration::from_millis(P2P_TIMER_DURATION_MS);
            while !killed.load(SeqCst) {
                std::thread::sleep(timer_duration);
                gossip.on_timer(&event_handler);
            }
        });
        self.task_handles.push(handle);
    }
}

impl Drop for P2P {
    /// The method signals the tasks to exit and waits for them to complete.
    fn drop(&mut self) {
        self.killed.store(true, SeqCst);
        while let Some(handle) = self.task_handles.pop() {
            async_safe_block_on_await(handle).ok();
        }
        self.event_handler.stop();
    }
}

/// The function sets up and returns the Artifact Manager and Consensus Pool.
///
/// The Artifact Manager runs all artifact clients as separate actors.
#[allow(clippy::too_many_arguments, clippy::type_complexity)]
fn setup_artifact_manager(
    rt_handle: tokio::runtime::Handle,
    node_id: NodeId,
    _crypto: Arc<dyn Crypto>,
    // ConsensusCrypto is an extension of the Crypto trait and we can
    // not downcast traits.
    consensus_crypto: Arc<dyn ConsensusCrypto>,
    certifier_crypto: Arc<dyn certification::CertificationCrypto>,
    ingress_sig_crypto: Arc<dyn IngressSigVerifier + Send + Sync>,
    subnet_id: SubnetId,
    artifact_pool_config: ArtifactPoolConfig,
    consensus_config: ConsensusConfig,
    replica_logger: ReplicaLogger,
    metrics_registry: MetricsRegistry,
    registry_client: Arc<dyn RegistryClient>,
    state_manager: Arc<dyn StateManager<State = ReplicatedState>>,
    state_sync_client: P2PStateSyncClient,
    xnet_payload_builder: Arc<dyn XNetPayloadBuilder>,
    message_router: Arc<dyn MessageRouting>,
    ingress_history_reader: Box<dyn IngressHistoryReader>,
    catch_up_package: CUPWithOriginalProtobuf,
    malicious_flags: MaliciousFlags,
    cycles_account_manager: Arc<CyclesAccountManager>,
    local_store_time_reader: Option<Arc<dyn LocalStoreCertifiedTimeReader>>,
    registry_poll_delay_duration_ms: u64,
    event_handler: Arc<dyn AdvertSubscriber + Send + Sync>,
) -> std::io::Result<(
    Arc<dyn ArtifactManager>,
    Arc<dyn ConsensusPoolCache>,
    IngressThrottler,
)> {
    // Initialize the time source.
    let time_source = Arc::new(SysTimeSource::new());

    let mut artifact_manager_maker = manager::ArtifactManagerMaker::new(time_source.clone());

    ensure_persistent_pool_replica_version_compatibility(
        artifact_pool_config.persistent_pool_db_path(),
    );

    let (ingress_pool, consensus_pool, cert_pool, dkg_pool) = init_artifact_pools(
        subnet_id,
        artifact_pool_config,
        metrics_registry.clone(),
        replica_logger.clone(),
        catch_up_package,
    );

    let consensus_cache = consensus_pool.read().unwrap().get_cache();

    if let P2PStateSyncClient::TestChunkingPool(client, client_on_state_change) = state_sync_client
    {
        let c_event_handler = event_handler;
        let addr = processors::ArtifactProcessorManager::new(
            Arc::clone(&time_source) as Arc<_>,
            metrics_registry,
            processors::BoxOrArcClient::ArcClient(client_on_state_change),
            move |advert| c_event_handler.broadcast_advert(advert.into()),
            rt_handle,
        );
        artifact_manager_maker.add_arc_client(client, addr);
        return Ok((
            artifact_manager_maker.finish(),
            consensus_cache,
            ingress_pool as Arc<_>,
        ));
    }
    if let P2PStateSyncClient::Client(state_sync_client) = state_sync_client {
        let event_handler = event_handler.clone();
        let addr = processors::ArtifactProcessorManager::new(
            Arc::clone(&time_source) as Arc<_>,
            metrics_registry.clone(),
            processors::BoxOrArcClient::ArcClient(Arc::clone(&state_sync_client) as Arc<_>),
            move |advert| event_handler.broadcast_advert(advert.into()),
            rt_handle.clone(),
        );
        artifact_manager_maker.add_arc_client(state_sync_client, addr);
    }

    let consensus_replica_config = ReplicaConfig { node_id, subnet_id };
    let membership = Membership::new(
        consensus_cache.clone(),
        Arc::clone(&registry_client),
        subnet_id,
    );
    let membership = Arc::new(membership);

    let ingress_manager = IngressManager::new(
        consensus_cache.clone(),
        ingress_history_reader,
        Arc::clone(&registry_client),
        Arc::clone(&ingress_sig_crypto) as Arc<_>,
        metrics_registry.clone(),
        subnet_id,
        replica_logger.clone(),
        Arc::clone(&state_manager) as Arc<_>,
        cycles_account_manager,
        malicious_flags.clone(),
    );
    let ingress_manager = Arc::new(ingress_manager);

    {
        // Create the consensus client.
        let event_handler = event_handler.clone();
        let (consensus_client, actor) = processors::ConsensusProcessor::build(
            move |advert| event_handler.broadcast_advert(advert.into()),
            || {
                ic_consensus::consensus::setup(
                    consensus_replica_config.clone(),
                    consensus_config,
                    Arc::clone(&registry_client),
                    Arc::clone(&membership) as Arc<_>,
                    Arc::clone(&consensus_crypto),
                    Arc::clone(&ingress_manager) as Arc<_>,
                    Arc::clone(&xnet_payload_builder) as Arc<_>,
                    Arc::clone(&dkg_pool) as Arc<_>,
                    Arc::clone(&message_router) as Arc<_>,
                    Arc::clone(&state_manager) as Arc<_>,
                    Arc::clone(&time_source) as Arc<_>,
                    malicious_flags.clone(),
                    metrics_registry.clone(),
                    replica_logger.clone(),
                    local_store_time_reader,
                    registry_poll_delay_duration_ms,
                )
            },
            Arc::clone(&time_source) as Arc<_>,
            Arc::clone(&consensus_pool),
            Arc::clone(&ingress_pool),
            rt_handle.clone(),
            replica_logger.clone(),
            metrics_registry.clone(),
        );
        artifact_manager_maker.add_client(consensus_client, actor);
    }

    {
        // Create the ingress client.
        let event_handler = event_handler.clone();
        let (ingress_client, actor) = processors::IngressProcessor::build(
            move |advert| event_handler.broadcast_advert(advert.into()),
            Arc::clone(&time_source) as Arc<_>,
            Arc::clone(&ingress_pool),
            ingress_manager,
            rt_handle.clone(),
            replica_logger.clone(),
            metrics_registry.clone(),
            malicious_flags,
        );
        artifact_manager_maker.add_client(ingress_client, actor);
    }

    {
        // Create the certification client.
        let event_handler = event_handler.clone();
        let (certification_client, actor) = processors::CertificationProcessor::build(
            move |advert| event_handler.broadcast_advert(advert.into()),
            || {
                certification::setup(
                    consensus_replica_config.clone(),
                    Arc::clone(&membership) as Arc<_>,
                    Arc::clone(&certifier_crypto),
                    Arc::clone(&state_manager) as Arc<_>,
                    metrics_registry.clone(),
                    replica_logger.clone(),
                )
            },
            Arc::clone(&time_source) as Arc<_>,
            Arc::clone(&consensus_cache) as Arc<_>,
            Arc::clone(&cert_pool),
            rt_handle.clone(),
            replica_logger.clone(),
            metrics_registry.clone(),
        );
        artifact_manager_maker.add_client(certification_client, actor);
    }

    {
        let event_handler = event_handler;
        let (dkg_client, actor) = processors::DkgProcessor::build(
            move |advert| event_handler.broadcast_advert(advert.into()),
            || {
                (
                    dkg::DkgImpl::new(
                        consensus_replica_config.node_id,
                        Arc::clone(&consensus_crypto),
                        Arc::clone(&consensus_cache),
                        metrics_registry.clone(),
                        replica_logger.clone(),
                    ),
                    dkg::DkgGossipImpl {},
                )
            },
            Arc::clone(&time_source) as Arc<_>,
            Arc::clone(&dkg_pool),
            rt_handle,
            replica_logger.clone(),
            metrics_registry.clone(),
        );
        artifact_manager_maker.add_client(dkg_client, actor);
    }

    Ok((
        artifact_manager_maker.finish(),
        consensus_cache,
        ingress_pool as Arc<_>,
    ))
}

/// The function initializes the artifact pools.
#[allow(clippy::type_complexity)]
pub(crate) fn init_artifact_pools(
    subnet_id: SubnetId,
    config: ArtifactPoolConfig,
    registry: MetricsRegistry,
    log: ReplicaLogger,
    catch_up_package: CUPWithOriginalProtobuf,
) -> (
    Arc<RwLock<IngressPoolImpl>>,
    Arc<RwLock<ConsensusPoolImpl>>,
    Arc<RwLock<CertificationPoolImpl>>,
    Arc<RwLock<DkgPoolImpl>>,
) {
    (
        Arc::new(RwLock::new(IngressPoolImpl::new(
            config.clone(),
            registry.clone(),
            log.clone(),
        ))),
        Arc::new(RwLock::new(ConsensusPoolImpl::new(
            subnet_id,
            catch_up_package,
            config.clone(),
            registry.clone(),
            log.clone(),
        ))),
        Arc::new(RwLock::new(CertificationPoolImpl::new(
            config,
            log,
            registry.clone(),
        ))),
        Arc::new(RwLock::new(DkgPoolImpl::new(registry))),
    )
}

// The following types are used for testing only. Ideally, they should only
// appear in the test module, but `TestArtifact` is used by
// `P2PStateSyncClient` so these definitions are still required here.

#[derive(Eq, PartialEq)]
/// The artifact struct used by the testing framework.
pub struct TestArtifact;
/// The artifact message used by the testing framework.
pub type TestArtifactMessage = FileTreeSyncArtifact;
/// The artifact ID used by the testing framework.
pub type TestArtifactId = FileTreeSyncId;
/// The attribute of the artifact used by the testing framework.
pub type TestArtifactAttribute = FileTreeSyncAttribute;

/// `TestArtifact` implements the `ArtifactKind` trait.
impl ArtifactKind for TestArtifact {
    const TAG: ArtifactTag = ArtifactTag::FileTreeSyncArtifact;
    type Message = TestArtifactMessage;
    type SerializeAs = TestArtifactMessage;
    type Id = TestArtifactId;
    type Attribute = TestArtifactAttribute;
    type Filter = ();

    /// The function converts a TestArtifactMessage to an advert for a
    /// TestArtifact.
    fn message_to_advert(msg: &TestArtifactMessage) -> Advert<TestArtifact> {
        Advert {
            attribute: msg.id.to_string(),
            size: 0,
            id: msg.id.clone(),
            integrity_hash: CryptoHash(msg.id.clone().into_bytes()),
        }
    }
}
