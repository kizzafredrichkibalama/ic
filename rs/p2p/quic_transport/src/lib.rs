//! Quic Transport.
//!
//! Transport layer based on QUIC. Provides connectivity to all peers in a subnet and
//! the ability to do rpc's to any peer in the subnet. RPC's are routed to the corresponding
//! handlers. Each RPC occurs on a different substream and are therefore fully decoupled from
//! each other.
//!
//! COMPONENTS:
//!  - Connection Manager (connection_manager.rs): Keeps peers connected.
//!  - Request Handler (request_handler.rs): Accepts streams on an active connection.
//!    Spawned by the connection manager for each connection.
//!  - Connection Handle (connection_handle.rs): Provides rpc and push interfaces to a peer.
//!
//! API:
//!  - Constructor takes a topology watcher. The topology defines the
//!    set of peers, to which transport tries to keep active connections.
//!  - Constructor also takes a Router. Incoming requests are routed to a handler
//!    based on the URI specified in the request.
//!  - `get_conn_handle`: Can be used to get a `ConnectionHandle` to a peer.
//!     The connection handle is small wrapper around the actual quic connection
//!     with an rpc/push interface. Passed in requests need to specify an URI to get
//!     routed to the correct handler.
//!
//! GUARANTEES:
//!  - If a peer is reachable, part of the topology and well-behaving transport will eventually
//!    open a connection.
//!  - The connection handle returned by `get_conn_handle` can be broken.
//!    It is responsibility of the transport user to have an adequate retry logic.
//!
//!
use std::{
    collections::{BTreeSet, HashMap},
    fmt::Debug,
    net::SocketAddr,
    sync::{Arc, RwLock},
};

use async_trait::async_trait;
use axum::{
    http::{Request, Response},
    Router,
};
use bytes::Bytes;
use either::Either;
use ic_base_types::{NodeId, RegistryVersion};
use ic_crypto_tls_interfaces::{TlsConfig, TlsStream};
use ic_icos_sev::ValidateAttestedStream;
use ic_interfaces_registry::RegistryClient;
use ic_logger::{info, ReplicaLogger};
use ic_metrics::MetricsRegistry;
use phantom_newtype::AmountOf;
use quinn::{AsyncUdpSocket, ConnectionError, WriteError};
use thiserror::Error;
use tokio::sync::watch;
use tokio_util::{sync::CancellationToken, task::task_tracker::TaskTracker};

use crate::connection_handle::ConnectionHandle;
use crate::connection_manager::start_connection_manager;

mod connection_handle;
mod connection_manager;
mod metrics;
mod request_handler;
mod utils;

#[derive(Clone)]
pub struct QuicTransport {
    conn_handles: Arc<RwLock<HashMap<NodeId, ConnectionHandle>>>,
    cancellation: CancellationToken,
    conn_manager_task_tracker: TaskTracker,
}

/// This is the main transport handle used for communication between peers.
/// The handler can safely be shared across threads and tasks.
///
/// Instead of the common `connect` and `disconnect`` methods the implementation
/// listens for changes of the topology using a watcher.
/// (The watcher matches better the semantics of peer discovery in the IC).
///
/// This enables complete separation between peer discovery and the core P2P
/// protocols that use `QuicTransport`.
/// For example, "P2P for consensus" implements a generic replication protocol which is
/// agnostic to the subnet membership logic required by the consensus algorithm.
/// This makes "P2P for consensus" a generic implementation that potentially can be used
/// not only by the consensus protocol of the IC.
impl QuicTransport {
    /// This is the entry point for creating (e.g. binding) and starting the quic transport.
    pub fn start(
        log: &ReplicaLogger,
        metrics_registry: &MetricsRegistry,
        rt: &tokio::runtime::Handle,
        tls_config: Arc<dyn TlsConfig + Send + Sync>,
        registry_client: Arc<dyn RegistryClient>,
        sev_handshake: Arc<dyn ValidateAttestedStream<Box<dyn TlsStream>> + Send + Sync>,
        node_id: NodeId,
        // The receiver is passed here mainly to be consistent with other managers that also
        // require receivers on construction.
        topology_watcher: watch::Receiver<SubnetTopology>,
        udp_socket: Either<SocketAddr, impl AsyncUdpSocket>,
        // Make sure this is respected https://docs.rs/axum/latest/axum/struct.Router.html#a-note-about-performance
        router: Router,
    ) -> QuicTransport {
        info!(log, "Starting Quic transport.");

        let cancellation = CancellationToken::new();
        let conn_handles = Arc::new(RwLock::new(HashMap::new()));
        let conn_manager_task_tracker = TaskTracker::new();

        start_connection_manager(
            log,
            metrics_registry,
            rt,
            tls_config.clone(),
            registry_client,
            sev_handshake,
            node_id,
            conn_handles.clone(),
            topology_watcher,
            cancellation.clone(),
            conn_manager_task_tracker.clone(),
            udp_socket,
            router,
        );

        QuicTransport {
            conn_handles,
            cancellation,
            conn_manager_task_tracker,
        }
    }

    /// Graceful shutdown of transport.
    pub async fn shutdown(&self) {
        let _ = self.conn_manager_task_tracker.close();
        // If an error is returned it means the conn manager is already stopped.
        self.cancellation.cancel();
        self.conn_manager_task_tracker.wait().await;
    }

    pub(crate) fn get_conn_handle(&self, peer_id: &NodeId) -> Result<ConnectionHandle, SendError> {
        let conn = self
            .conn_handles
            .read()
            .unwrap()
            .get(peer_id)
            .ok_or(SendError::ConnectionUnavailable(
                "Currently not connected to this peer".to_string(),
            ))?
            .clone();
        Ok(conn)
    }
}

#[async_trait]
impl Transport for QuicTransport {
    async fn rpc(
        &self,
        peer_id: &NodeId,
        request: Request<Bytes>,
    ) -> Result<Response<Bytes>, SendError> {
        let peer = self.get_conn_handle(peer_id)?;
        peer.rpc(request).await
    }

    async fn push(&self, peer_id: &NodeId, request: Request<Bytes>) -> Result<(), SendError> {
        let peer = self.get_conn_handle(peer_id)?;
        peer.push(request).await
    }

    fn peers(&self) -> Vec<(NodeId, ConnId)> {
        self.conn_handles
            .read()
            .unwrap()
            .iter()
            .map(|(n, c)| (*n, c.conn_id()))
            .collect()
    }
}

#[derive(Debug, Error)]
pub enum SendError {
    #[error("the connection to peer `{0}` is unavailable")]
    ConnectionUnavailable(String),
    // This serves as catch-all error for invariant breaking errors.
    // E.g. failing to serialize, peer closing connections unexpectedly, etc.
    #[error("internal error `{0}`")]
    Internal(String),
}

impl From<ConnectionError> for SendError {
    fn from(conn_err: ConnectionError) -> Self {
        SendError::Internal(conn_err.to_string())
    }
}

impl From<WriteError> for SendError {
    fn from(write_err: WriteError) -> Self {
        match write_err {
            WriteError::ConnectionLost(conn_err) => conn_err.into(),
            _ => SendError::Internal(write_err.to_string()),
        }
    }
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn rpc(
        &self,
        peer_id: &NodeId,
        request: Request<Bytes>,
    ) -> Result<Response<Bytes>, SendError>;

    async fn push(&self, peer_id: &NodeId, request: Request<Bytes>) -> Result<(), SendError>;

    fn peers(&self) -> Vec<(NodeId, ConnId)>;
}

pub struct ConnIdTag {}
pub type ConnId = AmountOf<ConnIdTag, u64>;

/// This is a workaround for being able to initiate quic transport
/// with both a real and virtual udp socket. This is needed due
/// to an inconsistency with the quinn API. This is fixed upstream
/// and can be removed with quinn 0.11.0.
/// https://github.com/quinn-rs/quinn/pull/1595
#[derive(Debug)]
pub struct DummyUdpSocket;

impl AsyncUdpSocket for DummyUdpSocket {
    fn poll_send(
        &self,
        _state: &quinn::udp::UdpState,
        _cx: &mut std::task::Context,
        _transmits: &[quinn::udp::Transmit],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        todo!()
    }
    fn poll_recv(
        &self,
        _cx: &mut std::task::Context,
        _bufs: &mut [std::io::IoSliceMut<'_>],
        _meta: &mut [quinn::udp::RecvMeta],
    ) -> std::task::Poll<std::io::Result<usize>> {
        todo!()
    }
    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        todo!()
    }
    fn may_fragment(&self) -> bool {
        todo!()
    }
}

/// Holds socket addresses of all peers in a subnet.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct SubnetTopology {
    subnet_nodes: HashMap<NodeId, SocketAddr>,
    earliest_registry_version: RegistryVersion,
    latest_registry_version: RegistryVersion,
}

impl SubnetTopology {
    pub fn new<T: IntoIterator<Item = (NodeId, SocketAddr)>>(
        subnet_nodes: T,
        earliest_registry_version: RegistryVersion,
        latest_registry_version: RegistryVersion,
    ) -> Self {
        Self {
            subnet_nodes: HashMap::from_iter(subnet_nodes),
            earliest_registry_version,
            latest_registry_version,
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&NodeId, &SocketAddr)> {
        self.subnet_nodes.iter()
    }

    pub fn is_member(&self, node: &NodeId) -> bool {
        self.subnet_nodes.contains_key(node)
    }

    pub fn get_addr(&self, node: &NodeId) -> Option<SocketAddr> {
        self.subnet_nodes.get(node).copied()
    }

    pub fn latest_registry_version(&self) -> RegistryVersion {
        self.latest_registry_version
    }

    pub fn earliest_registry_version(&self) -> RegistryVersion {
        self.earliest_registry_version
    }

    pub fn get_subnet_nodes(&self) -> BTreeSet<NodeId> {
        self.subnet_nodes.keys().copied().collect()
    }
}
