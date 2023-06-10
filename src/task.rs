use anyhow::{anyhow, format_err};
use either::Either;
use futures::{
    channel::{
        mpsc::{Receiver, UnboundedSender},
        oneshot,
    },
    sink::SinkExt,
    stream::Fuse,
    FutureExt, StreamExt,
};

use crate::{
    p2p::{addr::extract_peer_id_from_multiaddr, MultiaddrExt, PeerInfo},
    Channel,
};
use crate::{
    p2p::{ProviderStream, RecordStream},
    TSwarmEvent,
};
use beetle_bitswap_next::BitswapEvent;
use tokio::{sync::Notify, task::JoinHandle};

use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    io,
    sync::{atomic::AtomicU64, Arc},
    time::Duration,
};

use crate::{config::BOOTSTRAP_NODES, IpfsEvent, TSwarmEventFn};

use crate::{
    p2p::TSwarm,
    repo::{Repo, RepoEvent},
};

pub use crate::{
    error::Error,
    p2p::BehaviourEvent,
    p2p::KadResult,
    path::IpfsPath,
    repo::{PinKind, PinMode},
};

use libipld::multibase::{self, Base};
pub use libp2p::{
    self,
    core::transport::ListenerId,
    gossipsub::{MessageId, PublishError},
    identity::Keypair,
    identity::PublicKey,
    kad::{record::Key, Quorum},
    multiaddr::multiaddr,
    multiaddr::Protocol,
    swarm::NetworkBehaviour,
    Multiaddr, PeerId,
};

use libp2p::{
    autonat,
    identify::{Event as IdentifyEvent, Info as IdentifyInfo},
    kad::{
        AddProviderError, AddProviderOk, BootstrapError, BootstrapOk, GetClosestPeersError,
        GetClosestPeersOk, GetProvidersError, GetProvidersOk, GetRecordError, GetRecordOk,
        KademliaEvent::*, PutRecordError, PutRecordOk, QueryId, QueryResult::*, Record,
    },
    mdns::Event as MdnsEvent,
    ping::Success as PingSuccess,
    swarm::SwarmEvent,
};

#[allow(dead_code)]
static BITSWAP_ID: AtomicU64 = AtomicU64::new(0);

/// Background task of `Ipfs` created when calling `UninitializedIpfs::start`.
// The receivers are Fuse'd so that we don't have to manage state on them being exhausted.
#[allow(clippy::type_complexity)]
pub(crate) struct IpfsTask {
    pub(crate) swarm: TSwarm,
    pub(crate) repo_events: Fuse<Receiver<RepoEvent>>,
    pub(crate) from_facade: Fuse<Receiver<IpfsEvent>>,
    pub(crate) listening_addresses: HashMap<Multiaddr, ListenerId>,
    pub(crate) listeners: HashSet<ListenerId>,
    pub(crate) provider_stream: HashMap<QueryId, UnboundedSender<PeerId>>,
    pub(crate) bitswap_provider_stream:
        HashMap<QueryId, tokio::sync::mpsc::Sender<Result<HashSet<PeerId>, String>>>,
    pub(crate) record_stream: HashMap<QueryId, UnboundedSender<Record>>,
    pub(crate) repo: Repo,
    pub(crate) kad_subscriptions: HashMap<QueryId, Channel<KadResult>>,
    pub(crate) dht_peer_lookup: HashMap<PeerId, Vec<Channel<PeerInfo>>>,
    pub(crate) listener_subscriptions:
        HashMap<ListenerId, oneshot::Sender<Either<Multiaddr, Result<(), io::Error>>>>,
    pub(crate) bootstraps: HashSet<Multiaddr>,
    pub(crate) swarm_event: Option<TSwarmEventFn>,
    pub(crate) bitswap_sessions: HashMap<u64, Vec<(oneshot::Sender<()>, JoinHandle<()>)>>,
}

impl IpfsTask {
    pub(crate) async fn run(&mut self, delay: bool, notify: Arc<Notify>) {
        let mut first_run = false;
        let mut connected_peer_timer = tokio::time::interval(Duration::from_secs(60));
        let mut session_cleanup = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                Some(swarm) = self.swarm.next() => {
                    if delay {
                        tokio::time::sleep(Duration::from_nanos(10)).await;
                    }
                    self.handle_swarm_event(swarm);
                },
                Some(event) = self.from_facade.next() => {
                    if matches!(event, IpfsEvent::Exit) {
                        break;
                    }
                    if delay {
                        tokio::time::sleep(Duration::from_nanos(10)).await;
                    }
                    self.handle_event(event);
                },
                Some(repo) = self.repo_events.next() => {
                    self.handle_repo_event(repo);
                },
                _ = connected_peer_timer.tick() => {
                    info!("Connected Peers: {}", self.swarm.connected_peers().count());
                }
                _ = session_cleanup.tick() => {
                    let mut to_remove = Vec::new();
                    for (id, tasks) in &mut self.bitswap_sessions {
                        tasks.retain(|(_, task)| !task.is_finished());

                        if tasks.is_empty() {
                            to_remove.push(*id);
                        }

                        // Only do a small chunk of cleanup on each iteration
                        // TODO(arqu): magic number
                        if to_remove.len() >= 10 {
                            break;
                        }
                    }

                    for id in to_remove {
                        let (tx, _rx) = oneshot::channel();
                        self.destroy_bs_session(id, tx);
                    }
                }
            }
            if !first_run {
                first_run = true;
                notify.notify_one();
            }
        }
    }

    fn destroy_bs_session(&mut self, ctx: u64, ret: oneshot::Sender<anyhow::Result<()>>) {
        if let Some(bitswap) = self.swarm.behaviour().bitswap.as_ref() {
            let client = bitswap.client().clone();
            let workers: Option<Vec<(oneshot::Sender<()>, JoinHandle<()>)>> =
                self.bitswap_sessions.remove(&ctx);
            tokio::task::spawn(async move {
                debug!("stopping session {}", ctx);
                if let Some(workers) = workers {
                    debug!("stopping workers {} for session {}", workers.len(), ctx);
                    // first shutdown workers
                    for (closer, worker) in workers {
                        if closer.send(()).is_ok() {
                            worker.await.ok();
                        }
                    }
                    debug!("all workers stopped for session {}", ctx);
                }
                if let Err(err) = client.stop_session(ctx).await {
                    warn!("failed to stop session {}: {:?}", ctx, err);
                }
                if let Err(err) = ret.send(Ok(())) {
                    warn!("session {} failed to send stop response: {:?}", ctx, err);
                }
                debug!("session {} stopped", ctx);
            });
        }
    }

    fn handle_swarm_event(&mut self, swarm_event: TSwarmEvent) {
        if let Some(handler) = self.swarm_event.clone() {
            handler(&mut self.swarm, &swarm_event)
        }
        match swarm_event {
            SwarmEvent::NewListenAddr {
                listener_id,
                address,
            } => {
                self.listening_addresses
                    .insert(address.clone(), listener_id);

                if let Some(ret) = self.listener_subscriptions.remove(&listener_id) {
                    let _ = ret.send(Either::Left(address));
                }
            }
            SwarmEvent::ExpiredListenAddr {
                listener_id,
                address,
            } => {
                self.listeners.remove(&listener_id);
                self.listening_addresses.remove(&address);
                if let Some(ret) = self.listener_subscriptions.remove(&listener_id) {
                    //TODO: Determine if we want to return the address or use the right side and return an error?
                    let _ = ret.send(Either::Left(address));
                }
            }
            SwarmEvent::ListenerClosed {
                listener_id,
                reason,
                addresses,
            } => {
                self.listeners.remove(&listener_id);
                for address in addresses {
                    self.listening_addresses.remove(&address);
                }
                if let Some(ret) = self.listener_subscriptions.remove(&listener_id) {
                    let _ = ret.send(Either::Right(reason));
                }
            }
            SwarmEvent::ListenerError { listener_id, error } => {
                self.listeners.remove(&listener_id);
                if let Some(ret) = self.listener_subscriptions.remove(&listener_id) {
                    let _ = ret.send(Either::Right(Err(error)));
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Mdns(event)) => match event {
                MdnsEvent::Discovered(list) => {
                    for (peer, addr) in list {
                        self.swarm.behaviour_mut().add_peer(peer, addr.clone());
                        if !self.swarm.is_connected(&peer) {
                            if let Err(e) = self.swarm.dial(peer) {
                                warn!("Unable to dial {peer}: {e}");
                            }
                        }
                    }
                }
                MdnsEvent::Expired(list) => {
                    for (peer, _) in list {
                        if let Some(mdns) = self.swarm.behaviour().mdns.as_ref() {
                            if !mdns.has_node(&peer) {
                                trace!("mdns: Expired peer {}", peer.to_base58());
                            }
                        }
                    }
                }
            },
            SwarmEvent::Behaviour(BehaviourEvent::Kad(event)) => {
                match event {
                    InboundRequest { request } => {
                        trace!("kad: inbound {:?} request handled", request);
                    }
                    OutboundQueryProgressed {
                        result, id, step, ..
                    } => {
                        // make sure the query is exhausted

                        if self
                            .swarm
                            .behaviour()
                            .kademlia
                            .as_ref()
                            .and_then(|kad| kad.query(&id))
                            .is_none()
                        {
                            match result {
                                // these subscriptions return actual values
                                GetClosestPeers(_) | GetProviders(_) | GetRecord(_) => {}
                                // we want to return specific errors for the following
                                Bootstrap(Err(_)) | StartProviding(Err(_)) | PutRecord(Err(_)) => {}
                                // and the rest can just return a general KadResult::Complete
                                _ => {
                                    if let Some(ret) = self.kad_subscriptions.remove(&id) {
                                        let _ = ret.send(Ok(KadResult::Complete));
                                    }
                                }
                            }
                        }

                        match result {
                            Bootstrap(Ok(BootstrapOk {
                                peer,
                                num_remaining,
                            })) => {
                                debug!(
                                    "kad: bootstrapped with {}, {} peers remain",
                                    peer, num_remaining
                                );
                            }
                            Bootstrap(Err(BootstrapError::Timeout { .. })) => {
                                warn!("kad: timed out while trying to bootstrap");

                                if self
                                    .swarm
                                    .behaviour()
                                    .kademlia
                                    .as_ref()
                                    .and_then(|kad| kad.query(&id))
                                    .is_none()
                                {
                                    if let Some(ret) = self.kad_subscriptions.remove(&id) {
                                        let _ = ret.send(Err(anyhow::anyhow!(
                                            "kad: timed out while trying to bootstrap"
                                        )));
                                    }
                                }
                            }
                            GetClosestPeers(Ok(GetClosestPeersOk { key, peers })) => {
                                if self
                                    .swarm
                                    .behaviour()
                                    .kademlia
                                    .as_ref()
                                    .and_then(|kad| kad.query(&id))
                                    .is_none()
                                {
                                    if let Some(ret) = self.kad_subscriptions.remove(&id) {
                                        let _ = ret.send(Ok(KadResult::Peers(peers.clone())));
                                    }
                                    if let Ok(peer_id) = PeerId::from_bytes(&key) {
                                        if let Some(rets) = self.dht_peer_lookup.remove(&peer_id) {
                                            if !peers.contains(&peer_id) {
                                                for ret in rets {
                                                    let _ = ret.send(Err(anyhow::anyhow!(
                                                        "Could not locate peer"
                                                    )));
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            GetClosestPeers(Err(GetClosestPeersError::Timeout {
                                key,
                                peers: _,
                            })) => {
                                // don't mention the key here, as this is just the id of our node
                                warn!("kad: timed out while trying to find all closest peers");

                                if self
                                    .swarm
                                    .behaviour()
                                    .kademlia
                                    .as_ref()
                                    .and_then(|kad| kad.query(&id))
                                    .is_none()
                                {
                                    if let Some(ret) = self.kad_subscriptions.remove(&id) {
                                        let _ = ret.send(Err(anyhow::anyhow!(
                                            "timed out while trying to find all closest peers"
                                        )));
                                    }
                                    if let Ok(peer_id) = PeerId::from_bytes(&key) {
                                        if let Some(rets) = self.dht_peer_lookup.remove(&peer_id) {
                                            for ret in rets {
                                                let _ = ret.send(Err(anyhow::anyhow!(
                                                    "timed out while trying to find all closest peers"
                                                )));
                                            }
                                        }
                                    }
                                }
                            }
                            GetProviders(Ok(GetProvidersOk::FoundProviders {
                                key: _,
                                providers,
                            })) => {
                                if !providers.is_empty() {
                                    if let Entry::Occupied(entry) =
                                        self.bitswap_provider_stream.entry(id)
                                    {
                                        let providers = providers.clone();
                                        let tx = entry.get().clone();
                                        tokio::spawn(async move {
                                            let _ = tx.send(Ok(providers)).await;
                                        });
                                    }
                                }
                                if let Entry::Occupied(entry) = self.provider_stream.entry(id) {
                                    if !providers.is_empty() {
                                        tokio::spawn({
                                            let mut tx = entry.get().clone();
                                            async move {
                                                for provider in providers {
                                                    let _ = tx.send(provider).await;
                                                }
                                            }
                                        });
                                    }
                                }
                            }
                            GetProviders(Ok(GetProvidersOk::FinishedWithNoAdditionalRecord {
                                ..
                            })) => {
                                if step.last {
                                    if let Some(tx) = self.provider_stream.remove(&id) {
                                        tx.close_channel();
                                    }
                                    if let Some(tx) = self.bitswap_provider_stream.remove(&id) {
                                        drop(tx);
                                    }
                                }
                            }
                            GetProviders(Err(GetProvidersError::Timeout { key, .. })) => {
                                let key = multibase::encode(Base::Base32Lower, key);
                                warn!("kad: timed out while trying to get providers for {}", key);

                                if self
                                    .swarm
                                    .behaviour()
                                    .kademlia
                                    .as_ref()
                                    .and_then(|kad| kad.query(&id))
                                    .is_none()
                                {
                                    if let Some(ret) = self.kad_subscriptions.remove(&id) {
                                        let _ = ret.send(Err(anyhow::anyhow!("timed out while trying to get providers for the given key")));
                                    }
                                }
                            }
                            StartProviding(Ok(AddProviderOk { key })) => {
                                let key = multibase::encode(Base::Base32Lower, key);
                                debug!("kad: providing {}", key);
                            }
                            StartProviding(Err(AddProviderError::Timeout { key })) => {
                                let key = multibase::encode(Base::Base32Lower, key);
                                warn!("kad: timed out while trying to provide {}", key);

                                if self
                                    .swarm
                                    .behaviour()
                                    .kademlia
                                    .as_ref()
                                    .and_then(|kad| kad.query(&id))
                                    .is_none()
                                {
                                    if let Some(ret) = self.kad_subscriptions.remove(&id) {
                                        let _ = ret.send(Err(anyhow::anyhow!(
                                            "kad: timed out while trying to provide the record"
                                        )));
                                    }
                                }
                            }
                            RepublishProvider(Ok(AddProviderOk { key })) => {
                                let key = multibase::encode(Base::Base32Lower, key);
                                debug!("kad: republished provider {}", key);
                            }
                            RepublishProvider(Err(AddProviderError::Timeout { key })) => {
                                let key = multibase::encode(Base::Base32Lower, key);
                                warn!("kad: timed out while trying to republish provider {}", key);
                            }
                            GetRecord(Ok(GetRecordOk::FoundRecord(record))) => {
                                if let Entry::Occupied(entry) = self.record_stream.entry(id) {
                                    tokio::spawn({
                                        let mut tx = entry.get().clone();
                                        async move {
                                            let _ = tx.send(record.record).await;
                                        }
                                    });
                                }
                            }
                            GetRecord(Ok(GetRecordOk::FinishedWithNoAdditionalRecord {
                                ..
                            })) => {
                                if step.last {
                                    if let Some(tx) = self.record_stream.remove(&id) {
                                        tx.close_channel();
                                    }
                                }
                            }
                            GetRecord(Err(GetRecordError::NotFound {
                                key,
                                closest_peers: _,
                            })) => {
                                let key = multibase::encode(Base::Base32Lower, key);
                                warn!("kad: couldn't find record {}", key);

                                if self
                                    .swarm
                                    .behaviour()
                                    .kademlia
                                    .as_ref()
                                    .and_then(|kad| kad.query(&id))
                                    .is_none()
                                {
                                    if let Some(tx) = self.record_stream.remove(&id) {
                                        tx.close_channel();
                                    }
                                }
                            }
                            GetRecord(Err(GetRecordError::QuorumFailed {
                                key,
                                records: _,
                                quorum,
                            })) => {
                                let key = multibase::encode(Base::Base32Lower, key);
                                warn!(
                                    "kad: quorum failed {} when trying to get key {}",
                                    quorum, key
                                );

                                if self
                                    .swarm
                                    .behaviour()
                                    .kademlia
                                    .as_ref()
                                    .and_then(|kad| kad.query(&id))
                                    .is_none()
                                {
                                    if let Some(tx) = self.record_stream.remove(&id) {
                                        tx.close_channel();
                                    }
                                }
                            }
                            GetRecord(Err(GetRecordError::Timeout { key })) => {
                                let key = multibase::encode(Base::Base32Lower, key);
                                warn!("kad: timed out while trying to get key {}", key);

                                if self
                                    .swarm
                                    .behaviour()
                                    .kademlia
                                    .as_ref()
                                    .and_then(|kad| kad.query(&id))
                                    .is_none()
                                {
                                    if let Some(tx) = self.record_stream.remove(&id) {
                                        tx.close_channel();
                                    }
                                }
                            }
                            PutRecord(Ok(PutRecordOk { key }))
                            | RepublishRecord(Ok(PutRecordOk { key })) => {
                                let key = multibase::encode(Base::Base32Lower, key);
                                debug!("kad: successfully put record {}", key);
                            }
                            PutRecord(Err(PutRecordError::QuorumFailed {
                                key,
                                success: _,
                                quorum,
                            }))
                            | RepublishRecord(Err(PutRecordError::QuorumFailed {
                                key,
                                success: _,
                                quorum,
                            })) => {
                                let key = multibase::encode(Base::Base32Lower, key);
                                warn!(
                                    "kad: quorum failed ({}) when trying to put record {}",
                                    quorum, key
                                );

                                if self
                                    .swarm
                                    .behaviour()
                                    .kademlia
                                    .as_ref()
                                    .and_then(|kad| kad.query(&id))
                                    .is_none()
                                {
                                    if let Some(ret) = self.kad_subscriptions.remove(&id) {
                                        let _ = ret.send(Err(anyhow::anyhow!(
                                            "kad: quorum failed when trying to put the record"
                                        )));
                                    }
                                }
                            }
                            PutRecord(Err(PutRecordError::Timeout {
                                key,
                                success: _,
                                quorum: _,
                            })) => {
                                let key = multibase::encode(Base::Base32Lower, key);
                                warn!("kad: timed out while trying to put record {}", key);

                                if self
                                    .swarm
                                    .behaviour()
                                    .kademlia
                                    .as_ref()
                                    .and_then(|kad| kad.query(&id))
                                    .is_none()
                                {
                                    if let Some(ret) = self.kad_subscriptions.remove(&id) {
                                        let _ = ret.send(Err(anyhow::anyhow!(
                                            "kad: timed out while trying to put record {}",
                                            key
                                        )));
                                    }
                                }
                            }
                            RepublishRecord(Err(PutRecordError::Timeout {
                                key,
                                success: _,
                                quorum: _,
                            })) => {
                                let key = multibase::encode(Base::Base32Lower, key);
                                warn!("kad: timed out while trying to republish record {}", key);
                            }
                        }
                    }
                    RoutingUpdated {
                        peer,
                        is_new_peer: _,
                        addresses,
                        bucket_range: _,
                        old_peer: _,
                    } => {
                        trace!("kad: routing updated; {}: {:?}", peer, addresses);
                    }
                    UnroutablePeer { peer } => {
                        trace!("kad: peer {} is unroutable", peer);
                    }
                    RoutablePeer { peer, address } => {
                        trace!("kad: peer {} ({}) is routable", peer, address);
                    }
                    PendingRoutablePeer { peer, address } => {
                        trace!("kad: pending routable peer {} ({})", peer, address);
                    }
                }
            }
            SwarmEvent::Behaviour(BehaviourEvent::Bitswap(event)) => match event {
                BitswapEvent::Provide { key } => {
                    if let Some(kad) = self.swarm.behaviour_mut().kademlia.as_mut() {
                        let key = key.hash().to_bytes();
                        let _id = kad.start_providing(key.into()).ok();
                    }
                }
                BitswapEvent::FindProviders { key, response, .. } => {
                    if let Some(kad) = self.swarm.behaviour_mut().kademlia.as_mut() {
                        info!("Looking for providers for {key}");
                        let key = key.hash().to_bytes();
                        let id = kad.get_providers(key.into());
                        self.bitswap_provider_stream.insert(id, response);
                    }
                }
                BitswapEvent::Ping { peer, response } => {
                    let duration = self.swarm.behaviour().peerbook.get_peer_latest_rtt(peer);
                    let _ = response.send(duration).ok();
                }
            },
            SwarmEvent::Behaviour(BehaviourEvent::Ping(event)) => match event {
                libp2p::ping::Event {
                    peer,
                    result: Result::Ok(PingSuccess::Ping { rtt }),
                } => {
                    trace!(
                        "ping: rtt to {} is {} ms",
                        peer.to_base58(),
                        rtt.as_millis()
                    );
                    self.swarm.behaviour_mut().peerbook.set_peer_rtt(peer, rtt);
                }
                libp2p::ping::Event {
                    peer,
                    result: Result::Ok(PingSuccess::Pong),
                } => {
                    trace!("ping: pong from {}", peer);
                }
                libp2p::ping::Event {
                    peer,
                    result: Result::Err(libp2p::ping::Failure::Timeout),
                } => {
                    trace!("ping: timeout to {}", peer);
                    self.swarm.behaviour_mut().remove_peer(&peer);
                }
                libp2p::ping::Event {
                    peer,
                    result: Result::Err(libp2p::ping::Failure::Other { error }),
                } => {
                    error!("ping: failure with {}: {}", peer.to_base58(), error);
                }
                libp2p::ping::Event {
                    peer,
                    result: Result::Err(libp2p::ping::Failure::Unsupported),
                } => {
                    error!("ping: failure with {}: unsupported", peer.to_base58());
                }
            },
            SwarmEvent::Behaviour(BehaviourEvent::Identify(event)) => match event {
                IdentifyEvent::Received { peer_id, info } => {
                    self.swarm
                        .behaviour_mut()
                        .peerbook
                        .inject_peer_info(info.clone());

                    if let Some(rets) = self.dht_peer_lookup.remove(&peer_id) {
                        for ret in rets {
                            let _ = ret.send(Ok(info.clone().into()));
                        }
                    }

                    let IdentifyInfo {
                        listen_addrs,
                        protocols,
                        ..
                    } = info;

                    if let Some(bitswap) = self.swarm.behaviour_mut().bitswap() {
                        bitswap.on_identify(&peer_id, &protocols)
                    }

                    if let Some(kad) = self.swarm.behaviour_mut().kademlia.as_mut() {
                        if protocols
                            .iter()
                            .any(|p| p.as_bytes() == libp2p::kad::protocol::DEFAULT_PROTO_NAME)
                        {
                            for addr in &listen_addrs {
                                kad.add_address(&peer_id, addr.clone());
                            }
                        }
                    }

                    if protocols
                        .iter()
                        .any(|p| p.as_bytes() == libp2p::autonat::DEFAULT_PROTOCOL_NAME)
                    {
                        for addr in listen_addrs {
                            self.swarm
                                .behaviour_mut()
                                .autonat
                                .add_server(peer_id, Some(addr));
                        }
                    }
                }
                event => trace!("identify: {:?}", event),
            },
            SwarmEvent::Behaviour(BehaviourEvent::Autonat(autonat::Event::StatusChanged {
                old,
                new,
            })) => {
                //TODO: Use status to indicate if we should use a relay or not
                debug!("Old Nat Status: {:?}", old);
                debug!("New Nat Status: {:?}", new);
            }
            _ => trace!("Swarm event: {:?}", swarm_event),
        }
    }

    #[allow(deprecated)]
    //TODO: Replace addresses_of_peer
    fn handle_event(&mut self, event: IpfsEvent) {
        match event {
            IpfsEvent::Connect(target, ret) => {
                let rx = self.swarm.behaviour_mut().peerbook.connect(target);
                let _ = ret.send(rx);
            }
            IpfsEvent::Protocol(ret) => {
                let info = self.swarm.behaviour().supported_protocols();
                let _ = ret.send(info);
            }
            IpfsEvent::Addresses(ret) => {
                let addrs = self.swarm.behaviour_mut().addrs();
                ret.send(Ok(addrs)).ok();
            }
            IpfsEvent::Listeners(ret) => {
                let listeners = self.swarm.listeners().cloned().collect::<Vec<Multiaddr>>();
                ret.send(Ok(listeners)).ok();
            }
            IpfsEvent::IsConnected(peer_id, ret) => {
                let connected = self.swarm.is_connected(&peer_id);
                ret.send(Ok(connected)).ok();
            }
            IpfsEvent::Connected(ret) => {
                let connections = self.swarm.connected_peers().copied();
                ret.send(Ok(connections.collect())).ok();
            }
            IpfsEvent::Disconnect(peer, ret) => {
                let _ = ret.send(self.swarm.behaviour_mut().peerbook.disconnect(peer));
            }
            IpfsEvent::Ban(peer, ret) => {
                self.swarm.ban_peer_id(peer);
                let _ = ret.send(Ok(()));
            }
            IpfsEvent::Unban(peer, ret) => {
                self.swarm.unban_peer_id(peer);
                let _ = ret.send(Ok(()));
            }
            IpfsEvent::GetAddresses(ret) => {
                // perhaps this could be moved under `IpfsEvent` or free functions?
                let mut addresses = Vec::new();
                addresses.extend(self.swarm.listeners().map(|a| a.to_owned()));
                addresses.extend(self.swarm.external_addresses().map(|ar| ar.addr.to_owned()));
                let _ = ret.send(addresses);
            }
            IpfsEvent::PubsubSubscribe(topic, ret) => {
                let _ = ret.send(self.swarm.behaviour_mut().pubsub().subscribe(topic).ok());
            }
            IpfsEvent::PubsubUnsubscribe(topic, ret) => {
                let _ = ret.send(self.swarm.behaviour_mut().pubsub().unsubscribe(topic));
            }
            IpfsEvent::PubsubPublish(topic, data, ret) => {
                let _ = ret.send(self.swarm.behaviour_mut().pubsub().publish(topic, data));
            }
            IpfsEvent::PubsubPeers(Some(topic), ret) => {
                let _ = ret.send(self.swarm.behaviour_mut().pubsub().subscribed_peers(&topic));
            }
            IpfsEvent::PubsubPeers(None, ret) => {
                let _ = ret.send(self.swarm.behaviour_mut().pubsub().known_peers());
            }
            IpfsEvent::PubsubSubscribed(ret) => {
                let _ = ret.send(self.swarm.behaviour_mut().pubsub().subscribed_topics());
            }
            // IpfsEvent::WantList(peer, ret) => {
            //     let list = if let Some(peer) = peer {
            //         self.swarm
            //             .behaviour_mut()
            //             .bitswap()
            //             .peer_wantlist(&peer)
            //             .unwrap_or_default()
            //     } else {
            //         self.swarm.behaviour_mut().bitswap().local_wantlist()
            //     };
            //     let _ = ret.send(list);
            // }
            // IpfsEvent::BitswapStats(ret) => {
            //     let stats = self.swarm.behaviour_mut().bitswap().stats();
            //     let peers = self.swarm.behaviour_mut().bitswap().peers();
            //     let wantlist = self.swarm.behaviour_mut().bitswap().local_wantlist();
            //     let _ = ret.send((stats, peers, wantlist).into());
            // }
            IpfsEvent::PubsubEventStream(topic, ret) => {
                let receiver = self.swarm.behaviour().pubsub.event_stream(topic);
                let _ = ret.send(receiver);
            }
            IpfsEvent::AddListeningAddress(addr, ret) => match self.swarm.listen_on(addr) {
                Ok(id) => {
                    self.listeners.insert(id);
                    let (tx, rx) = oneshot::channel();
                    self.listener_subscriptions.insert(id, tx);
                    let _ = ret.send(Ok(rx));
                }
                Err(e) => {
                    let _ = ret.send(Err(anyhow::anyhow!(e)));
                }
            },
            IpfsEvent::RemoveListeningAddress(addr, ret) => {
                if let Some(id) = self.listening_addresses.remove(&addr) {
                    match self.swarm.remove_listener(id) {
                        true => {
                            self.listeners.remove(&id);
                            let (tx, rx) = oneshot::channel();
                            self.listener_subscriptions.insert(id, tx);
                            let _ = ret.send(Ok(rx));
                        }
                        false => {
                            let _ = ret.send(Err(anyhow::anyhow!(
                                "Failed to remove previously added listening address: {}",
                                addr
                            )));
                        }
                    }
                } else {
                    let _ = ret.send(Err(format_err!(
                        "Address was not listened to before: {}",
                        addr
                    )));
                }
            }
            IpfsEvent::Bootstrap(ret) => {
                let future = match self
                    .swarm
                    .behaviour_mut()
                    .kademlia
                    .as_mut()
                    .map(|kad| kad.bootstrap())
                {
                    Some(Ok(id)) => {
                        let (tx, rx) = oneshot::channel();
                        self.kad_subscriptions.insert(id, tx);
                        Ok(rx)
                    }
                    Some(Err(e)) => {
                        error!("kad: can't bootstrap the node: {:?}", e);
                        Err(anyhow!("kad: can't bootstrap the node: {:?}", e))
                    }
                    None => Err(anyhow!("kad protocol is disabled")),
                };
                let _ = ret.send(future);
            }
            IpfsEvent::AddPeer(peer_id, addr, ret) => {
                let result = match self
                    .swarm
                    .behaviour_mut()
                    .addressbook
                    .add_address(peer_id, addr.clone())
                {
                    true => Ok(()),
                    false => Err(anyhow::anyhow!(
                        "Unable to add {addr}. It either contains a `PeerId` or already exist."
                    )),
                };

                let _ = ret.send(result);
            }
            IpfsEvent::RemovePeer(peer_id, addr, ret) => {
                let result = match addr {
                    Some(addr) => Ok(self
                        .swarm
                        .behaviour_mut()
                        .addressbook
                        .remove_address(&peer_id, &addr)),
                    None => Ok(self.swarm.behaviour_mut().addressbook.remove_peer(&peer_id)),
                };

                let _ = ret.send(result);
            }
            IpfsEvent::GetClosestPeers(peer_id, ret) => {
                let id = self
                    .swarm
                    .behaviour_mut()
                    .kademlia
                    .as_mut()
                    .map(|kad| kad.get_closest_peers(peer_id));

                let (tx, rx) = oneshot::channel();
                let _ = ret.send(rx);
                match id {
                    Some(id) => {
                        self.kad_subscriptions.insert(id, tx);
                    }
                    None => {
                        let _ = tx.send(Err(anyhow::anyhow!("kad protocol is disabled")));
                    }
                };
            }
            IpfsEvent::WantList(peer, ret) => {
                if let Some(bitswap) = self.swarm.behaviour().bitswap.as_ref() {
                    let client = bitswap.client().clone();
                    let server = bitswap.server().cloned();

                    let _ = ret.send(
                        async move {
                            if let Some(peer) = peer {
                                if let Some(server) = server {
                                    server.wantlist_for_peer(&peer).await
                                } else {
                                    Vec::new()
                                }
                            } else {
                                Vec::from_iter(client.get_wantlist().await)
                            }
                        }
                        .boxed(),
                    );
                } else {
                    let _ = ret.send(futures::future::ready(vec![]).boxed());
                }
            }
            IpfsEvent::GetBitswapPeers(ret) => {
                if let Some(bitswap) = self.swarm.behaviour().bitswap.as_ref() {
                    let client = bitswap.client().clone();
                    let _ = ret.send(async move { client.get_peers().await }.boxed());
                } else {
                    let _ = ret.send(futures::future::ready(vec![]).boxed());
                }
            }
            IpfsEvent::FindPeerIdentity(peer_id, ret) => {
                let locally_known = self.swarm.behaviour().peerbook.get_peer_info(peer_id);

                let (tx, rx) = oneshot::channel();

                match locally_known {
                    Some(info) => {
                        let _ = tx.send(Ok(info.clone()));
                    }
                    None => {
                        self.swarm
                            .behaviour_mut()
                            .kademlia
                            .as_mut()
                            .map(|kad| kad.get_closest_peers(peer_id));

                        self.dht_peer_lookup.entry(peer_id).or_default().push(tx);
                    }
                }

                let _ = ret.send(rx);
            }
            IpfsEvent::FindPeer(peer_id, local_only, ret) => {
                let listener_addrs = self
                    .swarm
                    .behaviour_mut()
                    .peerbook
                    .peer_connections(peer_id)
                    .unwrap_or_default()
                    .iter()
                    .map(|addr| extract_peer_id_from_multiaddr(addr.clone()))
                    .map(|(_, addr)| addr)
                    .collect::<Vec<_>>();

                let locally_known_addrs = if !listener_addrs.is_empty() {
                    listener_addrs
                } else {
                    self.swarm
                        .behaviour_mut()
                        .kademlia
                        .addresses_of_peer(&peer_id)
                };

                let addrs = if !locally_known_addrs.is_empty() || local_only {
                    Either::Left(locally_known_addrs)
                } else {
                    Either::Right({
                        let id = self
                            .swarm
                            .behaviour_mut()
                            .kademlia
                            .as_mut()
                            .map(|kad| kad.get_closest_peers(peer_id));

                        let (tx, rx) = oneshot::channel();
                        if let Some(id) = id {
                            self.kad_subscriptions.insert(id, tx);
                        }
                        rx
                    })
                };
                let _ = ret.send(addrs);
            }
            IpfsEvent::WhitelistPeer(peer_id, ret) => {
                self.swarm.behaviour_mut().peerbook.add(peer_id);
                let _ = ret.send(Ok(()));
            }
            IpfsEvent::RemoveWhitelistPeer(peer_id, ret) => {
                self.swarm.behaviour_mut().peerbook.remove(peer_id);
                let _ = ret.send(Ok(()));
            }
            IpfsEvent::GetProviders(cid, ret) => {
                let key = Key::from(cid.hash().to_bytes());
                let id = self
                    .swarm
                    .behaviour_mut()
                    .kademlia
                    .as_mut()
                    .map(|kad| kad.get_providers(key));

                let mut provider_stream = None;

                let (tx, mut rx) = futures::channel::mpsc::unbounded();
                if let Some(id) = id {
                    let stream = async_stream::stream! {
                        let mut current_providers: HashSet<PeerId> = Default::default();
                        while let Some(provider) = rx.next().await {
                            if current_providers.insert(provider) {
                                yield provider;
                            }
                        }
                    };
                    self.provider_stream.insert(id, tx);
                    provider_stream = Some(ProviderStream(stream.boxed()));
                }

                let _ = ret.send(provider_stream);
            }
            IpfsEvent::Provide(cid, ret) => {
                let key = Key::from(cid.hash().to_bytes());
                let future = match self
                    .swarm
                    .behaviour_mut()
                    .kademlia
                    .as_mut()
                    .map(|kad| kad.start_providing(key))
                {
                    Some(Ok(id)) => {
                        let (tx, rx) = oneshot::channel();
                        self.kad_subscriptions.insert(id, tx);
                        Ok(rx)
                    }
                    Some(Err(e)) => {
                        error!("kad: can't provide a key: {:?}", e);
                        Err(anyhow!("kad: can't provide the key: {:?}", e))
                    }
                    None => Err(anyhow!("kad protocol is disabled")),
                };
                let _ = ret.send(future);
            }
            IpfsEvent::DhtGet(key, ret) => {
                let id = self
                    .swarm
                    .behaviour_mut()
                    .kademlia
                    .as_mut()
                    .map(|kad| kad.get_record(key));

                let (tx, mut rx) = futures::channel::mpsc::unbounded();
                let stream = async_stream::stream! {
                    while let Some(record) = rx.next().await {
                            yield record;
                    }
                };
                if let Some(id) = id {
                    self.record_stream.insert(id, tx);
                }

                let _ = ret.send(RecordStream(stream.boxed()));
            }
            IpfsEvent::DhtPut(key, value, quorum, ret) => {
                let record = Record {
                    key,
                    value,
                    publisher: None,
                    expires: None,
                };
                let future = match self
                    .swarm
                    .behaviour_mut()
                    .kademlia
                    .as_mut()
                    .map(|kad| kad.put_record(record, quorum))
                {
                    Some(Ok(id)) => {
                        let (tx, rx) = oneshot::channel();
                        self.kad_subscriptions.insert(id, tx);
                        Ok(rx)
                    }
                    Some(Err(e)) => {
                        error!("kad: can't put a record: {:?}", e);
                        Err(anyhow!("kad: can't provide the record: {:?}", e))
                    }
                    None => Err(anyhow!("kad protocol is not enabled")),
                };
                let _ = ret.send(future);
            }
            IpfsEvent::GetBootstrappers(ret) => {
                let list = Vec::from_iter(self.bootstraps.iter().cloned());
                let _ = ret.send(list);
            }
            IpfsEvent::AddBootstrapper(mut addr, ret) => {
                let ret_addr = addr.clone();
                if !self.swarm.behaviour().kademlia.is_enabled() {
                    let _ = ret.send(Err(anyhow::anyhow!("kad protocol is disabled")));
                } else {
                    if self.bootstraps.insert(addr.clone()) {
                        if let Some(peer_id) = addr.extract_peer_id() {
                            self.swarm
                                .behaviour_mut()
                                .kademlia
                                .as_mut()
                                .map(|kad| kad.add_address(&peer_id, addr));
                            self.swarm.behaviour_mut().peerbook.add(peer_id);
                            // the return value of add_address doesn't implement Debug
                            trace!(peer_id=%peer_id, "tried to add a bootstrapper");
                        }
                    }
                    let _ = ret.send(Ok(ret_addr));
                }
            }
            IpfsEvent::RemoveBootstrapper(mut addr, ret) => {
                let result = addr.clone();
                if !self.swarm.behaviour().kademlia.is_enabled() {
                    let _ = ret.send(Err(anyhow::anyhow!("kad protocol is disabled")));
                } else {
                    if self.bootstraps.remove(&addr) {
                        if let Some(peer_id) = addr.extract_peer_id() {
                            let prefix: Multiaddr = addr;

                            if let Some(Some(e)) = self
                                .swarm
                                .behaviour_mut()
                                .kademlia
                                .as_mut()
                                .map(|kad| kad.remove_address(&peer_id, &prefix))
                            {
                                info!(peer_id=%peer_id, status=?e.status, "removed bootstrapper");
                            } else {
                                warn!(peer_id=%peer_id, "attempted to remove an unknown bootstrapper");
                            }
                            self.swarm.behaviour_mut().peerbook.remove(peer_id);
                        }
                    }
                    let _ = ret.send(Ok(result));
                }
            }
            IpfsEvent::ClearBootstrappers(ret) => {
                let removed = self.bootstraps.drain().collect::<Vec<_>>();
                let mut list = Vec::with_capacity(removed.len());
                if self.swarm.behaviour().kademlia.is_enabled() {
                    for mut addr_with_peer_id in removed {
                        let priginal = addr_with_peer_id.clone();
                        let Some(peer_id) = addr_with_peer_id.extract_peer_id() else {
                            continue;
                        };
                        let prefix: Multiaddr = addr_with_peer_id;

                        if let Some(Some(e)) = self
                            .swarm
                            .behaviour_mut()
                            .kademlia
                            .as_mut()
                            .map(|kad| kad.remove_address(&peer_id, &prefix))
                        {
                            info!(peer_id=%peer_id, status=?e.status, "cleared bootstrapper");
                            list.push(priginal);
                        } else {
                            error!(peer_id=%peer_id, "attempted to clear an unknown bootstrapper");
                        }
                        self.swarm.behaviour_mut().peerbook.remove(peer_id);
                    }
                }
                let _ = ret.send(list);
            }
            IpfsEvent::DefaultBootstrap(ret) => {
                let mut rets = Vec::new();
                if self.swarm.behaviour().kademlia.is_enabled() {
                    for addr in BOOTSTRAP_NODES {
                        let mut addr = addr
                            .parse::<Multiaddr>()
                            .expect("see test bootstrap_nodes_are_multiaddr_with_peerid");
                        let original: Multiaddr = addr.clone();
                        if self.bootstraps.insert(addr.clone()) {
                            let Some(peer_id) = addr.extract_peer_id() else {
                                continue;
                            };

                            self.swarm
                                .behaviour_mut()
                                .kademlia
                                .as_mut()
                                .map(|kad| kad.add_address(&peer_id, addr.clone()));
                            trace!(peer_id=%peer_id, "tried to restore a bootstrapper");
                            self.swarm.behaviour_mut().peerbook.add(peer_id);
                            // report with the peerid
                            rets.push(original);
                        }
                    }
                }

                let _ = ret.send(Ok(rets));
            }
            IpfsEvent::Exit => {
                // FIXME: we could do a proper teardown
            }
        }
    }

    fn handle_repo_event(&mut self, event: RepoEvent) {
        match event {
            RepoEvent::WantBlock(session, cid, peers) => {
                if let Some(bitswap) = self.swarm.behaviour().bitswap.as_ref() {
                    let client = bitswap.client().clone();
                    let repo = self.repo.clone();
                    let (closer_s, closer_r) = oneshot::channel();
                    //If there is no session context defined, we will use 0 as its root context
                    let ctx = session.unwrap_or(0);
                    let entry = self.bitswap_sessions.entry(ctx).or_default();

                    let worker = tokio::task::spawn(async move {
                        tokio::select! {
                            _ = closer_r => {
                                // Explicit sesssion stop.
                                debug!("session {}: stopped: closed", ctx);
                            }
                            block = client.get_block_with_session_id(ctx, &cid, &peers) => match block {
                                Ok(block) => {
                                    info!("Found {cid}");
                                    let block = libipld::Block::new_unchecked(block.cid, block.data.to_vec());
                                    let res = repo.put_block(block).await;
                                    if let Err(e) = res {
                                        error!("Got block {} but failed to store it: {}", cid, e);
                                    }

                                }
                                Err(err) => {
                                    error!("Failed to get {}: {}", cid, err);
                                }
                            },
                        }
                    });
                    entry.push((closer_s, worker));
                }
            }
            RepoEvent::UnwantBlock(_cid) => {}
            RepoEvent::NewBlock(block, ret) => {
                if let Some(bitswap) = self.swarm.behaviour().bitswap.as_ref() {
                    let client = bitswap.client().clone();
                    let server = bitswap.server().cloned();
                    tokio::task::spawn(async move {
                        let block = beetle_bitswap_next::Block::new(
                            bytes::Bytes::copy_from_slice(block.data()),
                            *block.cid(),
                        );
                        if let Err(err) = client.notify_new_blocks(&[block.clone()]).await {
                            warn!("failed to notify bitswap about blocks: {:?}", err);
                        }
                        if let Some(server) = server {
                            if let Err(err) = server.notify_new_blocks(&[block]).await {
                                warn!("failed to notify bitswap about blocks: {:?}", err);
                            }
                        }
                    });
                }
                let _ = ret.send(Err(anyhow!("not actively providing blocks yet")));
            }
            RepoEvent::RemovedBlock(cid) => self.swarm.behaviour_mut().stop_providing_block(&cid),
        }
    }
}
