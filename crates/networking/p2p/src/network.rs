use std::{
    collections::HashMap,
    fmt::Debug,
    num::{NonZeroU8, NonZeroUsize},
    pin::Pin,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use anyhow::anyhow;
use discv5::Enr;
use libp2p::{
    Multiaddr, PeerId, Swarm, SwarmBuilder, Transport, connection_limits,
    core::{muxing::StreamMuxerBox, transport::Boxed},
    futures::StreamExt,
    identify,
    multiaddr::Protocol,
    noise,
    swarm::{NetworkBehaviour, SwarmEvent},
    yamux,
};
use libp2p_identity::{Keypair, PublicKey, secp256k1};
use ream_discv5::{
    config::NetworkConfig,
    discovery::{DiscoveredPeers, Discovery},
};
use ream_executor::ReamExecutor;
use ream_node::{
    peer::{Peer, PeerCountData, PeerStatus},
    network_channel::NetworkRequest,
};
use tokio::sync::mpsc::Receiver;
use tracing::{error, info, warn};

#[derive(NetworkBehaviour)]
pub(crate) struct ReamBehaviour {
    pub identify: identify::Behaviour,

    /// The discovery domain: discv5
    pub discovery: Discovery,

    pub connection_registry: connection_limits::Behaviour,
}

// TODO: these are stub events which needs to be replaced
#[derive(Debug)]
pub enum ReamNetworkEvent {
    PeerConnectedIncoming(PeerId),
    PeerConnectedOutgoing(PeerId),
    PeerDisconnected(PeerId),
    Status(PeerId),
    Ping(PeerId),
    MetaData(PeerId),
    DisconnectPeer(PeerId),
    DiscoverPeers(usize),
}

pub struct Network {
    peer_id: PeerId,
    swarm: Swarm<ReamBehaviour>,
    peers: Arc<RwLock<HashMap<PeerId, Peer>>>,
}

struct Executor(ReamExecutor);

impl libp2p::swarm::Executor for Executor {
    fn exec(&self, f: Pin<Box<dyn futures::Future<Output = ()> + Send>>) {
        self.0.spawn(f);
    }
}

impl Network {
    pub async fn init(executor: ReamExecutor, config: &NetworkConfig) -> anyhow::Result<Self> {
        let local_key = secp256k1::Keypair::generate();

        let discovery = {
            let mut discovery = Discovery::new(Keypair::from(local_key.clone()), config).await?;
            discovery.discover_peers(16);
            discovery
        };

        let connection_limits = {
            let limits = libp2p::connection_limits::ConnectionLimits::default()
                .with_max_pending_incoming(Some(5))
                .with_max_pending_outgoing(Some(16))
                .with_max_established_per_peer(Some(1));

            libp2p::connection_limits::Behaviour::new(limits)
        };

        let identify = {
            let local_public_key = local_key.public();
            let identify_config = identify::Config::new(
                "eth2/1.0.0".into(),
                PublicKey::from(local_public_key.clone()),
            )
            .with_agent_version("0.0.1".to_string())
            .with_cache_size(0);

            identify::Behaviour::new(identify_config)
        };

        let behaviour = {
            ReamBehaviour {
                discovery,
                identify,
                connection_registry: connection_limits,
            }
        };

        let transport = build_transport(Keypair::from(local_key.clone()))
            .map_err(|err| anyhow!("Failed to build transport: {err:?}"))?;

        let swarm = {
            let config = libp2p::swarm::Config::with_executor(Executor(executor))
                .with_notify_handler_buffer_size(NonZeroUsize::new(7).expect("Not zero"))
                .with_per_connection_event_buffer_size(4)
                .with_dial_concurrency_factor(NonZeroU8::new(1).unwrap());

            let builder = SwarmBuilder::with_existing_identity(Keypair::from(local_key.clone()))
                .with_tokio()
                .with_other_transport(|_key| transport)
                .expect("initializing swarm");

            builder
                .with_behaviour(|_| behaviour)
                .expect("initializing swarm")
                .with_swarm_config(|_| config)
                .build()
        };

        let mut network = Network {
            peer_id: PeerId::from_public_key(&PublicKey::from(local_key.public().clone())),
            peers: Arc::new(RwLock::new(HashMap::new())),
            swarm,
        };

        network.start_network_worker(config).await?;

        Ok(network)
    }

    async fn start_network_worker(&mut self, config: &NetworkConfig) -> anyhow::Result<()> {
        info!("Libp2p starting .... ");

        let mut multi_addr: Multiaddr = config.socket_address.into();
        multi_addr.push(Protocol::Tcp(config.socket_port));

        match self.swarm.listen_on(multi_addr.clone()) {
            Ok(listener_id) => {
                info!(
                    "Listening on {:?} with peer_id {:?} {listener_id:?}",
                    multi_addr, self.peer_id
                );
            }
            Err(err) => {
                error!("Failed to start libp2p peer listen on {multi_addr:?}, error: {err:?}",);
            }
        }

        for bootnode in &config.bootnodes {
            if let (Some(ipv4), Some(tcp_port)) = (bootnode.ip4(), bootnode.tcp4()) {
                let mut multi_addr = Multiaddr::empty();
                multi_addr.push(ipv4.into());
                multi_addr.push(Protocol::Tcp(tcp_port));
                self.swarm.dial(multi_addr).unwrap();
            }
        }

        Ok(())
    }

    /// polling the libp2p swarm for network events.
    pub async fn polling_events(
        &mut self,
        mut request_rx: Receiver<NetworkRequest>,
    ) -> ReamNetworkEvent {
        loop {
            tokio::select! {
                 Some(request) = request_rx.recv() => {
                    match request {
                        NetworkRequest::GetPeerCount(response_tx) => {
                            let _ = response_tx.send(self.get_peer_counts());
                        },
                    }
                }
                Some(event) = self.swarm.next() => {
                    if let Some(event) = self.parse_swarm_event(event){
                        return event;
                    }
                }
            }
        }
    }

    fn parse_swarm_event(
        &mut self,
        event: SwarmEvent<ReamBehaviourEvent>,
    ) -> Option<ReamNetworkEvent> {
        info!("Event: {:?}", event);
        match event {
            SwarmEvent::Behaviour(behaviour_event) => match behaviour_event {
                ReamBehaviourEvent::Identify(_) => None,
                ReamBehaviourEvent::Discovery(DiscoveredPeers { peers }) => {
                    self.handle_discovered_peers(peers);
                    None
                }
                ream_behavior_event => {
                    info!("Unhandled behaviour event: {ream_behavior_event:?}");
                    None
                }
            },
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                self.upsert_peer_status(peer_id, PeerStatus::Connected);
                info!("Peer connected: {}", peer_id);
                None
            }
            SwarmEvent::OutgoingConnectionError { peer_id, .. } => {
                if let Some(peer_id) = peer_id {
                    self.upsert_peer_status(peer_id, PeerStatus::Disconnected);
                }
                None
            }
            SwarmEvent::IncomingConnectionError { .. } => {
                // No specific peer to track for incoming connection errors
                None
            }
            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                self.upsert_peer_status(peer_id, PeerStatus::Disconnected);
                info!("Peer disconnected: {}", peer_id);
                None
            }
            SwarmEvent::Dialing { peer_id, .. } => {
                if let Some(peer_id) = peer_id {
                    self.upsert_peer_status(peer_id, PeerStatus::Connecting);
                    info!("Dialing peer: {}", peer_id);
                } else {
                    warn!("Dialing event without peer_id");
                }
                None
            }
            swarm_event => {
                info!("Unhandled swarm event: {swarm_event:?}");
                None
            }
        }
    }

    fn handle_discovered_peers(&mut self, peers: HashMap<Enr, Option<Instant>>) {
        info!("Discovered peers: {:?}", peers);
        for (enr, _) in peers {
            let mut multiaddrs: Vec<Multiaddr> = Vec::new();
            if let Some(ip) = enr.ip4() {
                if let Some(tcp) = enr.tcp4() {
                    let mut multiaddr: Multiaddr = ip.into();
                    multiaddr.push(Protocol::Tcp(tcp));
                    multiaddrs.push(multiaddr);
                }
            }
            if let Some(ip6) = enr.ip6() {
                if let Some(tcp6) = enr.tcp6() {
                    let mut multiaddr: Multiaddr = ip6.into();
                    multiaddr.push(Protocol::Tcp(tcp6));
                    multiaddrs.push(multiaddr);
                }
            }
            for multiaddr in multiaddrs {
                if let Err(err) = self.swarm.dial(multiaddr) {
                    warn!("Failed to dial peer: {err:?}");
                }
            }
        }
    }

    fn upsert_peer_status(&self, peer_id: PeerId, status: PeerStatus) {
        if let Ok(mut peers) = self.peers.write() {
            peers.insert(
                peer_id,
                Peer {
                    id: peer_id,
                    status,
                },
            );
            info!("Peer {}: status changed to {:?}", peer_id, status);
        }
    }

    // todo: when should a peer be removed?
    // fn remove_peer(&self, peer_id: &PeerId) {
    //     if let Ok(mut peers) = self.peers.write() {
    //         peers.remove(peer_id);
    //         info!("Peer {}: removed from tracking", peer_id);
    //     }
    // }

    pub fn get_peer_counts(&self) -> PeerCountData {
        let mut connected = 0;
        let mut connecting = 0;
        let mut disconnecting = 0;
        let mut disconnected = 0;
        if let Ok(peers) = self.peers.read() {
            for peer in peers.values() {
                match peer.status {
                    PeerStatus::Connected => connected += 1,
                    PeerStatus::Connecting => connecting += 1,
                    PeerStatus::Disconnecting => disconnecting += 1,
                    PeerStatus::Disconnected => disconnected += 1,
                }
            }
        }
        PeerCountData { connected, connecting, disconnecting, disconnected }
    }
}

type BoxedTransport = Boxed<(PeerId, StreamMuxerBox)>;
pub fn build_transport(local_private_key: Keypair) -> std::io::Result<BoxedTransport> {
    // mplex config
    let mut mplex_config = libp2p_mplex::MplexConfig::new();
    mplex_config.set_max_buffer_size(256);
    mplex_config.set_max_buffer_behaviour(libp2p_mplex::MaxBufferBehaviour::Block);

    let yamux_config = yamux::Config::default();

    let tcp = libp2p::tcp::tokio::Transport::new(libp2p::tcp::Config::default().nodelay(true))
        .upgrade(libp2p::core::upgrade::Version::V1)
        .authenticate(noise::Config::new(&local_private_key).expect("Noise disabled"))
        .multiplex(libp2p::core::upgrade::SelectUpgrade::new(
            yamux_config,
            mplex_config,
        ))
        .timeout(Duration::from_secs(10));
    let transport = tcp.boxed();

    let transport = libp2p::dns::tokio::Transport::system(transport)?.boxed();

    Ok(transport)
}
