/*
* Copyright (C) 2019-2021 TON Labs. All Rights Reserved.
*
* Licensed under the SOFTWARE EVALUATION License (the "License"); you may not use
* this file except in compliance with the License.
*
* Unless required by applicable law or agreed to in writing, software
* distributed under the License is distributed on an "AS IS" BASIS,
* WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
* See the License for the specific TON DEV software governing permissions and
* limitations under the License.
*/

use adnl::{
    declare_counted,
    common::{
        add_counted_object_to_map_with_update, add_unbound_object_to_map, AdnlPeers, CountedObject, 
        Counter, hash, hash_boxed, Query, QueryResult, Subscriber, TaggedTlObject, Version, Wait
    }, 
    node::{AddressCache, AddressCacheIterator, AdnlNode, IpAddress}
};
#[cfg(feature = "telemetry")]
use adnl::telemetry::Metric;
use overlay::{OverlayId, OverlayShortId, OverlayUtils};
use rand::Rng;
use std::{
    collections::VecDeque, convert::TryInto, fmt::{self, Display, Formatter}, 
    sync::{Arc, atomic::{AtomicU8, AtomicU64, Ordering}}
};
#[cfg(feature = "telemetry")]
use std::time::Instant;
use ton_api::{
    deserialize_boxed, IntoBoxed, serialize_boxed, serialize_boxed_inplace, Signing,
    ton::{
        PublicKey, TLObject, 
        adnl::AddressList as AddressListBoxed, 
        dht::{
            Node as NodeBoxed, Nodes as NodesBoxed, Pong as DhtPongBoxed, Stored, UpdateRule,
            ValueResult as DhtValueResult,
            key::Key as DhtKey, keydescription::KeyDescription as DhtKeyDescription, 
            node::Node, nodes::Nodes, pong::Pong as DhtPong, value::Value as DhtValue,
            valueresult::{ValueFound, ValueNotFound}
        },
        overlay::{
            Nodes as OverlayNodesBoxed, node::Node as OverlayNode, nodes::Nodes as OverlayNodes
        }, 
        pub_::publickey::Overlay,
        rpc::dht::{
            FindNode, FindValue, GetSignedAddressList, Ping as DhtPing, Query as DhtQuery, 
            Store
        }
    }
};
#[cfg(feature = "telemetry")]
use ton_api::tag_from_boxed_type;
use ton_types::{error, fail, base64_encode, KeyId, KeyOption, Result, UInt256};

include!("../common/src/info.rs");

pub const TARGET: &str = "dht";

pub struct DhtIterator {
    iter: Option<AddressCacheIterator>, 
    key_id: Arc<DhtKeyId>,
    order: Vec<(u8, Arc<KeyId>)>
}

impl DhtIterator {

    fn with_key_id(dht: &DhtNode, key_id: Arc<DhtKeyId>) -> Self {
        let mut ret = Self {
            iter: None,
            key_id,
            order: Vec::new() 
        };
        ret.update(dht);
        ret
    }

    fn update(&mut self, dht: &DhtNode) {
        let mut next = if let Some(iter) = &self.iter {
            dht.known_peers.given(iter)
        } else {
            dht.get_known_peer(&mut self.iter)
        };
        while let Some(peer) = next {
            let mut affinity = DhtNode::get_affinity(peer.data(), &self.key_id);
            if let Some(score) = dht.bad_peers.get(&peer) {
                let score = score.val().load(Ordering::Relaxed);
                let new_affinity = affinity.saturating_sub(score);
                log::debug!(
                    target: TARGET, 
                    "Bad DHT peer {}, score {} affinity {} -> {}", 
                    peer, score, affinity, new_affinity
                );
                affinity = new_affinity;
            }
            let add = if let Some((top_affinity, _)) = self.order.last() {
                (*top_affinity <= affinity) || (self.order.len() < DhtNode::MAX_TASKS as usize)
            } else {
                true
            };
            if add {
                self.order.push((affinity, peer))
            }
            next = dht.get_known_peer(&mut self.iter)
        }
        self.order.sort_unstable_by_key(|(affinity, _)| *affinity);
        if let Some((top_affinity, _)) = self.order.last() {
            let mut drop_to = 0;
            while self.order.len() - drop_to > DhtNode::MAX_TASKS as usize {
                let (affinity, _) = self.order[drop_to];
                if affinity < *top_affinity {
                    drop_to += 1
                } else {
                    break
                }
            }
            self.order.drain(0..drop_to);
        }
        if log::log_enabled!(log::Level::Debug) {
            let mut out = format!("DHT search list for {}:\n", base64_encode(&self.key_id[..]));
            for (affinity, key_id) in self.order.iter().rev() {
                out.push_str(format!("order {} - {}\n", affinity, key_id).as_str())
            }
            log::debug!(target: TARGET, "{}", out);
        }
    }

}

impl Display for DhtIterator {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        if let Some(iter) = &self.iter {
            write!(f, "{} DHT peer(s) selected of {:?}", self.order.len(), iter)
        } else {
            write!(f, "no DHT peers yet")
        }
    }
}

type DhtKeyId = [u8; 32];

struct DhtKeyIdDumper {
    dump: Option<String>
}

impl DhtKeyIdDumper {
    fn with_params(level: log::Level, src: &DhtKeyId) -> Self {
        let dump = if log::log_enabled!(level) {
            Some(base64_encode(src))
        } else {
            None
        };
        Self {
            dump
        }
    }
}

impl Display for DhtKeyIdDumper {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        if let Some(dump) = &self.dump {
            write!(f, "{}", dump)
        } else {
            fmt::Result::Ok(())
        }
    }
}	

declare_counted!(
    struct NodeObject {
        object: Node
    }
);

declare_counted!(
    struct ValueObject {
        object: DhtValue
    }
);

#[derive(Clone)]
pub enum DhtSearchPolicy {
    FastSearch(u8),    // Parameter: concurrency level 
    FullSearch(u8)     // Parameter: concurrency level
}

#[cfg(feature = "telemetry")]
struct DhtTelemetry {
    peers: Arc<Metric>,
    values: Arc<Metric>
}

struct DhtAlloc {
    peers: Arc<AtomicU64>,
    values: Arc<AtomicU64>
}

struct OverlayNodeResolveContext {
    node: OverlayNode,
    key: Arc<dyn KeyOption>,
    search: Option<AddressSearchContext>
}

pub struct AddressSearchContext {
    iter: Option<DhtIterator>,
    key_id: Arc<DhtKeyId>,
}

pub struct OverlayNodesSearchContext {
    key_id: Arc<DhtKeyId>,
    search: VecDeque<OverlayNodeResolveContext>,
    stored: AddressCache
}

/// DHT Node
pub struct DhtNode {
    adnl: Arc<AdnlNode>,
    buckets: lockfree::map::Map<u8, lockfree::map::Map<Arc<KeyId>, NodeObject>>,
    bad_peers: lockfree::map::Map<Arc<KeyId>, AtomicU8>,
    known_peers: AddressCache,
    node_key: Arc<dyn KeyOption>,
    query_prefix: Vec<u8>,
    storage: lockfree::map::Map<DhtKeyId, ValueObject>,
    #[cfg(feature = "telemetry")]
    tag_dht_ping: u32,
    #[cfg(feature = "telemetry")]
    tag_get_signed_address_list: u32,
    #[cfg(feature = "telemetry")]
    tag_find_node: u32,
    #[cfg(feature = "telemetry")]
    tag_find_value: u32,
    #[cfg(feature = "telemetry")]
    tag_store: u32,
    #[cfg(feature = "telemetry")]
    telemetry: DhtTelemetry,
    allocated: DhtAlloc
}

impl DhtNode {

    const BITS: [u8; 16] = [
        4, 3, 2, 2, 1, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0
    ];

    const MAX_FAIL_COUNT: u8 = 5;
    const MAX_PEERS: u32 = 65536;
    const MAX_TASKS: u8 = 5;
    const TIMEOUT_VALUE: i32 = 3600; // Seconds

    /// Constructor 
    pub fn with_adnl_node(adnl: Arc<AdnlNode>, key_tag: usize) -> Result<Arc<Self>> {
        let node_key = adnl.key_by_tag(key_tag)?;
        #[cfg(feature = "telemetry")]
        let telemetry = DhtTelemetry {
            peers: adnl.add_metric("Alloc DHT peers"),
            values: adnl.add_metric("Alloc DHT values")
        };
        let allocated = DhtAlloc {
            peers: Arc::new(AtomicU64::new(0)),
            values: Arc::new(AtomicU64::new(0))
        };
        let mut ret = Self {
            adnl,
            buckets: lockfree::map::Map::new(),
            bad_peers: lockfree::map::Map::new(), 
            known_peers: AddressCache::with_limit(Self::MAX_PEERS),
            node_key,
            query_prefix: Vec::new(),
            storage: lockfree::map::Map::new(),
            #[cfg(feature = "telemetry")]
            tag_dht_ping: tag_from_boxed_type::<DhtPing>(),
            #[cfg(feature = "telemetry")]
            tag_find_node: tag_from_boxed_type::<FindNode>(),
            #[cfg(feature = "telemetry")]
            tag_find_value: tag_from_boxed_type::<FindValue>(),
            #[cfg(feature = "telemetry")]
            tag_get_signed_address_list: tag_from_boxed_type::<GetSignedAddressList>(),
            #[cfg(feature = "telemetry")]
            tag_store: tag_from_boxed_type::<Store>(),
            #[cfg(feature = "telemetry")]
            telemetry,
            allocated
        };
        let query = DhtQuery { 
            node: ret.sign_local_node()?
        };
        serialize_boxed_inplace(&mut ret.query_prefix, &query)?;
        Ok(Arc::new(ret))
    }

    /// Add DHT peer 
    pub fn add_peer(&self, peer: &Node) -> Result<Option<Arc<KeyId>>> {
        if let Err(e) = self.verify_other_node(peer) {
            log::warn!(target: TARGET, "Error when verifying DHT peer: {}", e);
            return Ok(None)
        }
        let addr = if let Some(addr) = AdnlNode::parse_address_list(&peer.addr_list)? {
            addr
        } else {
            log::warn!(target: TARGET, "Wrong DHT peer address {:?}", peer.addr_list);
            return Ok(None)
        };
        let ret = self.adnl.add_peer(
            self.node_key.id(), 
            &addr, 
            &((&peer.id).try_into()?)
        )?;
        let ret = if let Some(ret) = ret {
            ret
        } else {
            return Ok(None)
        };
        if self.known_peers.put(ret.clone())? {
            let key1 = self.node_key.id().data();
            let key2 = ret.data();
            let affinity = Self::get_affinity(key1, key2);
            add_unbound_object_to_map(
                &self.buckets, 
                affinity, 
                || Ok(lockfree::map::Map::new())
            )?;
            if let Some(bucket) = self.buckets.get(&affinity) {
                add_counted_object_to_map_with_update(
                    bucket.val(),
                    ret.clone(), 
                    |old_node| {
                        if let Some(old_node) = old_node {
                            if old_node.object.version >= peer.version {
                                return Ok(None)
                            }
                        }
                        let ret = NodeObject {
                            object: peer.clone(),
                            counter: self.allocated.peers.clone().into()
                        };
                        #[cfg(feature = "telemetry")]
                        self.telemetry.peers.update(
                            self.allocated.peers.load(Ordering::Relaxed)
                        );
                        Ok(Some(ret))
                    }
                )?;
            }
        } else {
            self.set_good_peer(&ret)
        }
        Ok(Some(ret))
    }

    /// Find DHT nodes
    pub async fn find_dht_nodes(&self, dst: &Arc<KeyId>) -> Result<bool> {
        let query = TaggedTlObject {
            object: TLObject::new(
                FindNode {
                    key: UInt256::with_array(*self.node_key.id().data()),
                    k: 10
                }
            ),
            #[cfg(feature = "telemetry")]
            tag: self.tag_find_node
        };
        let answer = self.query_with_prefix(dst, &query).await?;
        let answer: NodesBoxed = if let Some(answer) = answer {
            Query::parse(answer, &query.object)?
        } else {
            return Ok(false)
        };        
        let src = answer.only().nodes;
        log::debug!(target: TARGET, "-------- Found DHT nodes:");
        for node in src.iter() {
            log::debug!(target: TARGET, "{:?}", node);
            self.add_peer(node)?; 
        }
        Ok(true)
    }

    /// Fetch address of node (locally) with given key ID 
    pub async fn fetch_address(
        &self,
        key_id: &Arc<KeyId>
    ) -> Result<Option<(IpAddress, Arc<dyn KeyOption>)>> {
        let key = Self::dht_key_from_key_id(key_id, "address");
        let value = self.search_dht_key(&hash(key)?);
        if let Some(value) = value {
            let object = deserialize_boxed(&value.value)?;
            Ok(Some(Self::parse_value_as_address(value.key, object)?))
        } else {
            Ok(None)
        }
    }

    /// Find address of node with given key ID 
    pub async fn find_address(
        dht: &Arc<Self>, 
        key_id: &Arc<KeyId>
    ) -> Result<Option<(IpAddress, Arc<dyn KeyOption>)>> {
        DhtNode::find_address_with_context(
            dht, 
            key_id, 
            &mut None, 
            DhtSearchPolicy::FullSearch(Self::MAX_TASKS)
        ).await
    }

    /// Find address of node with given key ID 
    pub async fn find_address_with_context(
        dht: &Arc<Self>, 
        key_id: &Arc<KeyId>,
        ctx_opt: &mut Option<AddressSearchContext>,
        policy: DhtSearchPolicy
    ) -> Result<Option<(IpAddress, Arc<dyn KeyOption>)>> {
        if ctx_opt.is_none() {
            let key_id = Arc::new(hash(Self::dht_key_from_key_id(key_id, "address"))?);
            ctx_opt.replace(
                AddressSearchContext {
                    iter: None,
                    key_id
                }
            );
        }
        let Some(ctx) = ctx_opt else {
            fail!("INTERNAL ERROR: cannot make address search context")
        };
        let mut addr_list = DhtNode::find_value(
            dht,
            &ctx.key_id,
            |object| object.is::<AddressListBoxed>(),
            &policy,
            false, 
            &mut ctx.iter
        ).await?;
        if let Some((key, addr_list)) = addr_list.pop() {
            Ok(Some(Self::parse_value_as_address(key, addr_list)?))
        } else {
            Ok(None)
        }
    }

    /// Get nodes of overlay with given ID
    pub async fn find_overlay_nodes(
        dht: &Arc<Self>, 
        overlay_id: &Arc<OverlayShortId>,
        iter: &mut Option<DhtIterator>
    ) -> Result<Vec<(IpAddress, OverlayNode)>> {
        DhtNode::find_overlay_nodes_with_context(
            dht, 
            overlay_id, 
            &mut None,
            DhtSearchPolicy::FullSearch(Self::MAX_TASKS), 
            iter
        ).await
    }

    /// Get nodes of overlay with given ID, keeping search context
    pub async fn find_overlay_nodes_with_context(
        dht: &Arc<Self>, 
        overlay_id: &Arc<OverlayShortId>,
        ctx_search_opt: &mut Option<OverlayNodesSearchContext>,
        policy: DhtSearchPolicy,
        iter: &mut Option<DhtIterator>
    ) -> Result<Vec<(IpAddress, OverlayNode)>> {
        let mut ret = Vec::new();
        if ctx_search_opt.is_none() {
            let key_id = Arc::new(hash(Self::dht_key_from_key_id(overlay_id, "nodes"))?);
            ctx_search_opt.replace(
                OverlayNodesSearchContext {
                    key_id,
                    search: VecDeque::new(),
                    stored: AddressCache::with_limit(Self::MAX_PEERS)
                }
            );
        }
        let Some(ctx_search) = ctx_search_opt else {
            fail!("INTERNAL ERROR: cannot make overlay search context")
        };
        log::debug!(
            target: TARGET, 
            "-------- Overlay nodes search, {}", 
            if let Some(iter) = iter {
                iter.to_string()
            } else {
                format!("{} DHT peer(s) to query", dht.known_peers.count())
            }
        );
        let mut postponed = VecDeque::new();
        loop {
            if ctx_search.search.is_empty() {
                let mut nodes_lists = DhtNode::find_value(
                    dht,
                    &ctx_search.key_id,
                    |object| object.is::<OverlayNodesBoxed>(),
                    &policy,
                    true, 
                    iter
                ).await?;
                if nodes_lists.is_empty() {
                    // No more results
                    break
                }
                while let Some((_, nodes_list)) = nodes_lists.pop() {
                    if let Ok(nodes_list) = nodes_list.downcast::<OverlayNodesBoxed>() {
                        for node in nodes_list.only().nodes.0 {
                            let key: Arc<dyn KeyOption> = (&node.id).try_into()?;
                            ctx_search.search.push_back(
                                OverlayNodeResolveContext {
                                    node,
                                    key,
                                    search: None
                                }
                            )
                        }
                    } else {
                        fail!("INTERNAL ERROR: overlay nodes list type mismatch in search")
                    } 
                }
                ctx_search.search.append(&mut postponed);
            }
            let (wait, mut queue_reader) = Wait::new();
            log::debug!(
                target: TARGET, 
                "-------- Overlay nodes search, {} ({} suspicious) nodes to resolve", 
                ctx_search.search.len() + postponed.len(), 
                postponed.len()
            );
            let limit = match &policy {
                DhtSearchPolicy::FastSearch(_) => 1,
                DhtSearchPolicy::FullSearch(limit) => *limit
            };
            while let Some(mut ctx_resolve) = ctx_search.search.pop_front() {
                if ctx_search.stored.contains(ctx_resolve.key.id()) {
                    log::trace!(
                        target: TARGET, 
                        "-------- Overlay nodes search, node {} already stored", 
                        ctx_resolve.key.id()
                    );
                    continue
                }
                let dht = dht.clone();
                let policy = policy.clone();
                let wait = wait.clone();
                let reqs = wait.request_immediate();
                tokio::spawn(
                    async move {
                        log::trace!(
                            target: TARGET, 
                            "-------- Overlay nodes search, try resolve node {}", 
                            ctx_resolve.key.id()
                        );
                        match DhtNode::find_address_with_context(
                            &dht, 
                            ctx_resolve.key.id(),
                            &mut ctx_resolve.search,
                            policy
                        ).await {
                            Ok(Some((ip, _))) => {
                                log::debug!(
                                    target: TARGET, 
                                    "-------- Overlay nodes search, resolved {} IP: {}, key: {}",
                                    ctx_resolve.key.id(), ip, 
                                    base64_encode(ctx_resolve.key.pub_key().unwrap_or(&[0u8; 32]))
                                );
                                wait.respond(Some((Some(ip), ctx_resolve)))
                            },
                            Ok(None) => {
                                log::trace!(
                                    target: TARGET, 
                                    "-------- Overlay nodes search, {} not resolved", 
                                    ctx_resolve.key.id()
                                );
                                wait.respond(Some((None, ctx_resolve))) 
                            },
                            Err(e) => {
                                log::debug!(
                                    target: TARGET, 
                                    "-------- Overlay nodes search, cannot resolve {}: {}", 
                                    ctx_resolve.key.id(), e
                                );
                                wait.respond(Some((None, ctx_resolve))) 
                            }
                        }
                    }
                );
                if reqs >= limit as usize {
                    break
                }
            }
            loop {  
                match wait.wait(&mut queue_reader, false).await { 
                    Some(Some((None, ctx_resolve))) => match &policy {
                        DhtSearchPolicy::FastSearch(_) => (), 
                        DhtSearchPolicy::FullSearch(_) => postponed.push_back(ctx_resolve),
                    },
                    Some(Some((Some(ip), ctx_resolve))) => {
                        if ctx_search.stored.put(ctx_resolve.key.id().clone())? {
                            ret.push((ip, ctx_resolve.node));
                        }
                    },
                    _ => break
                }
            }
            log::debug!(
                target: TARGET, 
                "-------- Overlay nodes search, so far resolved {} nodes", 
                ret.len()
            );
            if !ret.is_empty() {
                // Found some
                break
            }
            if iter.is_none() {
                // Search is over
                break
            }
        }
        ctx_search.search.append(&mut postponed);
        log::debug!(
            target: TARGET, 
            "-------- Overlay nodes search, {} nodes yet to resolve", 
            ctx_search.search.len()
        );
        Ok(ret)
    }

    /// Get DHT peer via iterator
    pub fn get_known_peer(&self, iter: &mut Option<AddressCacheIterator>) -> Option<Arc<KeyId>> {
        loop {
            let ret = if let Some(iter) = iter {
                self.known_peers.next(iter)
            } else {
                let (new_iter, first) = self.known_peers.first();
                iter.replace(new_iter);
                first
            };
            if let Some(peer) = &ret {
                if let Some(count) = self.bad_peers.get(peer) {
                    if count.val().load(Ordering::Relaxed) >= Self::MAX_FAIL_COUNT {
                        continue
                    }
                }
            }
            break ret
        }
    }

    /// Get known DHT nodes
    pub fn get_known_nodes(&self, limit: usize) -> Result<Vec<Node>> {
        if limit == 0 {
            fail!("It is useless to ask for zero known nodes")
        }
        let mut ret = Vec::new();
        for i in 0..=255 {
            if let Some(bucket) = self.buckets.get(&i) {
                for node in bucket.val().iter() {         
                    ret.push(node.val().object.clone());
                    if ret.len() == limit {
                        return Ok(ret)
                    }
                }
            }
        }
        Ok(ret)
    }
                    
    /// Get signed address list 
    pub async fn get_signed_address_list(&self, dst: &Arc<KeyId>) -> Result<bool> {
        let query = TaggedTlObject {
            object: TLObject::new(GetSignedAddressList),
            #[cfg(feature = "telemetry")]
            tag: self.tag_get_signed_address_list
        };
        let answer = self.query_with_prefix(dst, &query).await?;
        let answer: NodeBoxed = if let Some(answer) = answer {
            Query::parse(answer, &query.object)?
        } else {
            return Ok(false)
        };
        self.add_peer(&answer.only())?;
        Ok(true)
    }

    /// Get signed node
    pub fn get_signed_node(&self) -> Result<Node> {
        self.sign_local_node()
    }

    /// Node IP address
    pub fn ip_address(&self) -> &IpAddress {
        self.adnl.ip_address()
    }

    /// Node key
    pub fn key(&self) -> &Arc<dyn KeyOption> {
        &self.node_key
    }

    /// Ping 
    pub async fn ping(&self, dst: &Arc<KeyId>) -> Result<bool> {
        let random_id = rand::thread_rng().gen();
        let query = TaggedTlObject {
            object: TLObject::new(
                DhtPing { 
                    random_id 
                }
            ),
            #[cfg(feature = "telemetry")]
            tag: self.tag_dht_ping
        };
        let answer = self.query(dst, &query).await?;
        let answer: DhtPongBoxed = if let Some(answer) = answer {
            Query::parse(answer, &query.object)?
        } else {
            return Ok(false)
        };
        Ok(answer.random_id() == &random_id)
    }

    /// Store own IP address
    pub async fn store_ip_address(dht: &Arc<Self>, key: &Arc<dyn KeyOption>) -> Result<bool> {
        log::debug!(target: TARGET, "Storing key ID {}", key.id());
        let addr_list = dht.adnl.build_address_list(None)?;
        let addr = AdnlNode::parse_address_list(&addr_list)?.ok_or_else(
            || error!("INTERNAL ERROR: cannot parse generated address list")
        )?;
        let value = serialize_boxed(&addr_list.into_boxed())?;
        let value = Self::sign_value("address", value, key)?;
        let key = Self::dht_key_from_key_id(key.id(), "address");
        let key_id = hash(key.clone())?;
        log::debug!(target: TARGET, "Storing DHT key ID {}", base64_encode(&key_id[..]));
        dht.process_store_signed_value(key_id, value.clone())?;
        Self::store_value(
            dht,
            key,
            value,
            |object| object.is::<AddressListBoxed>(),
            false, 
            |mut objects| {
                while let Some((_, object)) = objects.pop() {
                    if let Ok(addr_list) = object.downcast::<AddressListBoxed>() {
                        let addr_list = addr_list.only();
                        if let Some(ip) = AdnlNode::parse_address_list(&addr_list)? {
                            if ip == addr { //dht.adnl.ip_address() {
                                log::debug!(target: TARGET, "Checked stored address {:?}", ip);
                                return Ok(true);
                            } else {
                                log::warn!(
                                    target: TARGET, 
                                    "Found another stored address {:?}, expected {:?}", 
                                    ip,
                                    dht.adnl.ip_address()
                                )
                            }
                        } else {
                            log::warn!(
                                target: TARGET, 
                                "Found some wrong address list {:?}",
                                addr_list
                            )
                        }
                    } else {
                        fail!("INTERNAL ERROR: address list type mismatch in store")
                    }
                }
                Ok(false)
            }
        ).await
    }

    /// Store own overlay node
    pub async fn store_overlay_node(
        dht: &Arc<Self>, 
        overlay_id: &OverlayId, 
        node: &OverlayNode
    ) -> Result<bool> {
        log::debug!(target: TARGET, "Storing overlay node {:?}", node);
        let overlay_id = Overlay {
            name: overlay_id.to_vec().into()
        };
        let overlay_short_id = OverlayShortId::from_data(hash(overlay_id.clone())?);
        OverlayUtils::verify_node(&overlay_short_id, node)?;
        let nodes = OverlayNodes {
            nodes: vec![node.clone()].into()
        }.into_boxed();
        let key = Self::dht_key_from_key_id(&overlay_short_id, "nodes");
        let value = DhtValue {
            key: DhtKeyDescription {
                id: overlay_id.into_boxed(),
                key: key.clone(),
                signature: Default::default(),
                update_rule: UpdateRule::Dht_UpdateRule_OverlayNodes
            },
            ttl: Version::get() + Self::TIMEOUT_VALUE,
            signature: Default::default(),
            value: serialize_boxed(&nodes)?.into()
        };
        dht.process_store_overlay_nodes(hash(key.clone())?, value.clone())?;
        Self::store_value(
            dht,
            key,
            value,
            |object| object.is::<OverlayNodesBoxed>(),
            true, 
            |mut objects| {
                while let Some((_, object)) = objects.pop() {
                    if let Ok(nodes_list) = object.downcast::<OverlayNodesBoxed>() {
                        for found_node in nodes_list.only().nodes.0 {
                            if &found_node == node {
                                log::debug!(target: TARGET, "Checked stored node {:?}", node);
                                return Ok(true);
                            }
                        }
                    } else {
                        fail!("INTERNAL ERROR: overlay nodes list type mismatch in store")
                    }
                }
                Ok(false)
            }
        ).await
    }

    fn deserialize_overlay_nodes(value: &[u8]) -> Result<Vec<OverlayNode>> {
        let nodes = deserialize_boxed(value)?
            .downcast::<OverlayNodesBoxed>()
            .map_err(|object| error!("Wrong OverlayNodes: {:?}", object))?;
        Ok(nodes.only().nodes.0)
    }

    fn dht_key_from_key_id(id: &Arc<KeyId>, name: &str) -> DhtKey {
        DhtKey {
            id: UInt256::with_array(*id.data()),
            idx: 0,
            name: name.as_bytes().to_vec().into()
        }
    }

    async fn find_value(
        dht: &Arc<Self>, 
        key_id: &Arc<DhtKeyId>, 
        check: impl Fn(&TLObject) -> bool + Copy + Send + 'static,
        policy: &DhtSearchPolicy,
        all: bool,
        iter_opt: &mut Option<DhtIterator>
    ) -> Result<Vec<(DhtKeyDescription, TLObject)>> {
        let iter = iter_opt.get_or_insert_with(||DhtIterator::with_key_id(dht, key_id.clone()));
        if &iter.key_id != key_id {
            fail!("INTERNAL ERROR: DHT key mismatch in value search")
        }
        let mut ret = Vec::new();
        let query = TaggedTlObject {
            object: TLObject::new(
                FindValue { 
                    key: UInt256::from_slice(&key_id[..]),
                    k: 6 
                }
            ),
            #[cfg(feature = "telemetry")]
            tag: dht.tag_find_value
        };
        let key_dumper = DhtKeyIdDumper::with_params(log::Level::Debug, key_id);
        let query = Arc::new(query);
        let (wait, mut queue_reader) = Wait::new();  
        let mut known_peers = dht.known_peers.count();
        log::debug!(
            target: TARGET, 
            "FindValue with DHT key ID {} query, {}", 
            key_dumper, iter
        );
        let limit = match &policy {
            DhtSearchPolicy::FastSearch(limit) => *limit,
            DhtSearchPolicy::FullSearch(limit) => *limit
        } as usize;
        loop {
            while let Some((_, peer)) = iter.order.pop() {
                let dht_cloned = dht.clone();
                let key_id = key_id.clone();
                let peer = peer.clone(); 
                let query = query.clone(); 
                let wait = wait.clone(); 
                let reqs = wait.request_immediate(); 
                tokio::spawn(
                    async move {
                        match dht_cloned.value_query(&peer, &query, &key_id, check).await {
                            Ok(found) => wait.respond(found),
                            Err(e) => {
                                log::warn!(target: TARGET, "ERROR: {}", e);
                                wait.respond(None)
                            }
                        } 
                    } 
                );
                if reqs >= limit {
                    break;
                } 
            } 
            log::debug!(
                target: TARGET, 
                "FindValue with DHT key ID {} query, {} parallel reqs, {}", 
                key_dumper, wait.count(), iter
            );
            let mut finished = match &policy {
                DhtSearchPolicy::FastSearch(_) => true,
                DhtSearchPolicy::FullSearch(_) => false
            };
            loop {
                match wait.wait(&mut queue_reader, !all).await { 
                    Some(None) => (),
                    Some(Some(val)) => ret.push(val),
                    None => finished = true
                }
                // Update iterator if required
                if all || ret.is_empty() || finished {
                    let updated_known_peers = dht.known_peers.count();
                    if updated_known_peers != known_peers {
                        iter.update(dht);
                        known_peers = updated_known_peers;
                    }
                }
                // Add more tasks if required 
                if !all || (ret.len() < limit) || finished {
                    break
                }
            }
            // Stop if possible 
            if (all && (ret.len() >= limit)) || (!all && !ret.is_empty()) || finished {
                break
            } 
        }
        if iter.order.is_empty() {
            iter_opt.take();
        }
        Ok(ret)
    }

    fn get_affinity(key1: &DhtKeyId, key2: &DhtKeyId) -> u8 {
        let mut ret = 0;
        for i in 0..32 {
            match key1[i] ^ key2[i] {
                0 => ret += 8,
                x => {
                    if (x & 0xF0) == 0 {
                        ret += Self::BITS[(x & 0x0F) as usize] + 4
                    } else {
                        ret += Self::BITS[(x >> 4) as usize]
                    }
                    break
                }
            }
        }
        ret
    }

    fn parse_value_as_address(
        key: DhtKeyDescription, 
        value: TLObject
    ) -> Result<(IpAddress, Arc<dyn KeyOption>)> {
        if let Ok(addr_list) = value.downcast::<AddressListBoxed>() {
            let ip_address = AdnlNode::parse_address_list(&addr_list.only())?.ok_or_else(
                || error!("Wrong address list in DHT search")
            )?;
            Ok((ip_address, (&key.id).try_into()?))
        } else {
            fail!("Address list type mismatch in DHT search")
        }
    }

    fn process_find_node(&self, query: &FindNode) -> Result<Nodes> {
        log::trace!(target: TARGET, "Process FindNode query {:?}", query);
        let key1 = self.node_key.id().data();
        let key2 = query.key.as_slice();
        let mut dist = 0u8;
        let mut ret = Vec::new();
        for i in 0..32 {
            if ret.len() == query.k as usize {
                break;
            }
            let mut subdist = dist;
            let mut xor = key1[i] ^ key2[i];
            while xor != 0 {
                if (xor & 0xF0) == 0 {
                    subdist = subdist.saturating_add(4);
                    xor <<= 4;
                } else {
                    let shift = Self::BITS[(xor >> 4) as usize];
                    subdist = subdist.saturating_add(shift);
                    if let Some(bucket) = self.buckets.get(&subdist) {
                        for node in bucket.val().iter() {         
                            ret.push(node.val().object.clone());
                            if ret.len() == query.k as usize {
                                break
                            }
                        }
                    }
                    xor <<= shift + 1;
                    subdist = subdist.saturating_add(1);
                }
                if ret.len() == query.k as usize {
                    break
                }
            }
            dist = dist.saturating_add(8);
        }
        let ret = Nodes {
            nodes: ret.into()
        };
        log::trace!(target: TARGET, "FindNode result {:?}", ret);
        Ok(ret)
    }

    fn process_find_value(&self, query: &FindValue) -> Result<DhtValueResult> {
        log::trace!(target: TARGET, "Process FindValue query {:?}", query);
        let ret = if let Some(value) = self.search_dht_key(query.key.as_slice()) {
            ValueFound {
                value: value.into_boxed()
            }.into_boxed()
        } else {
            ValueNotFound {
                nodes: Nodes {
                    nodes: self.get_known_nodes(query.k as usize)?.into()
                }
            }.into_boxed()
        };
        log::trace!(target: TARGET, "FindValue result {:?}", ret);
        Ok(ret)
    }

    fn process_ping(&self, query: &DhtPing) -> Result<DhtPong> {
        Ok(DhtPong { random_id: query.random_id })
    }

    fn process_store(&self, query: Store) -> Result<Stored> {
        let dht_key_id = hash(query.value.key.key.clone())?;
        if query.value.ttl <= Version::get() {
            fail!("Ignore expired DHT value with key {}", base64_encode(&dht_key_id))
        }
        match query.value.key.update_rule {
            UpdateRule::Dht_UpdateRule_Signature => 
                self.process_store_signed_value(dht_key_id, query.value)?,
            UpdateRule::Dht_UpdateRule_OverlayNodes =>
                self.process_store_overlay_nodes(dht_key_id, query.value)?,
            _ => fail!("Unsupported store query {:?}", query)  
        };                                                                                                                         
        Ok(Stored::Dht_Stored)
    }

    fn process_store_overlay_nodes(&self, dht_key_id: DhtKeyId, value: DhtValue) -> Result<bool> {
        log::trace!(target: TARGET, "Process Store Overlay Nodes {:?}", value);
        if !value.signature.is_empty() {
            fail!("Wrong value signature for OverlayNodes")
        }
        if !value.key.signature.is_empty() {
            fail!("Wrong key signature for OverlayNodes")
        }
        let overlay_short_id = match value.key.id {
            PublicKey::Pub_Overlay(_) => OverlayShortId::from_data(hash_boxed(&value.key.id)?),
            _ => fail!("Wrong key description format for OverlayNodes")
        };
        if Self::dht_key_from_key_id(&overlay_short_id, "nodes") != value.key.key {
            fail!("Wrong DHT key for OverlayNodes")
        }
        let mut nodes_list = Self::deserialize_overlay_nodes(&value.value)?;
        let mut nodes = Vec::new();
        while let Some(node) = nodes_list.pop() {
            if let Err(e) = OverlayUtils::verify_node(&overlay_short_id, &node) {
                log::warn!(target: TARGET, "Bad overlay node {:?}: {}", node, e)
            } else {
                nodes.push(node)
            }
        }
        if nodes.is_empty() {
            fail!("Empty overlay nodes list")
        }
        add_counted_object_to_map_with_update(
            &self.storage,
            dht_key_id, 
            |old_value| {
                let old_value = if let Some(old_value) = old_value {
                    if old_value.object.ttl < Version::get() {
                        None
                    } else if old_value.object.ttl > value.ttl {
                        return Ok(None)
                    } else {
                        Some(&old_value.object.value)
                    }
                } else {
                    None
                };
                let mut old_nodes = if let Some(old_value) = old_value {
                    Self::deserialize_overlay_nodes(old_value)?
                } else {
                    Vec::new()
                };
                for node in nodes.iter() {
                    let mut found = false;
                    for old_node in old_nodes.iter_mut() {
                        if node.id == old_node.id {
                            if node.version > old_node.version {
                                *old_node = node.clone()
                            } else {
                                return Ok(None)
                            }
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        old_nodes.push(node.clone())
                    }
                }
                let nodes = OverlayNodes {
                    nodes: old_nodes.into()
                }.into_boxed();
                let mut ret = ValueObject {
                    object: value.clone(),
                    counter: self.allocated.values.clone().into()
                };
                #[cfg(feature = "telemetry")]
                self.telemetry.values.update(
                    self.allocated.values.load(Ordering::Relaxed)
                );
                ret.object.value = serialize_boxed(&nodes)?.into();
                log::trace!(target: TARGET, "Store Overlay Nodes result {:?}", ret.object);
                Ok(Some(ret))
            }
        )
    }

    fn process_store_signed_value(
        &self, dht_key_id: DhtKeyId, 
        mut value: DhtValue
    ) -> Result<bool> {
        self.verify_value(&mut value)?;
        add_counted_object_to_map_with_update(
            &self.storage,
            dht_key_id, 
            |old_value| {
                if let Some(old_value) = old_value {
                    if old_value.object.ttl >= value.ttl {
                        return Ok(None)
                    }
                }
                let ret = ValueObject {
                    object: value.clone(),
                    counter: self.allocated.values.clone().into()
                };
                #[cfg(feature = "telemetry")]
                self.telemetry.values.update(
                    self.allocated.values.load(Ordering::Relaxed)
                );
                Ok(Some(ret))
            }
        )
    }

    async fn query(
        &self, 
        dst: &Arc<KeyId>, 
        query: &TaggedTlObject
    ) -> Result<Option<TLObject>> {
        let peers = AdnlPeers::with_keys(self.node_key.id().clone(), dst.clone());
        let result = self.adnl.clone().query(query, &peers, None).await?;
        self.set_query_result(result, dst)
    } 

    async fn query_with_prefix(
        &self, 
        dst: &Arc<KeyId>, 
        query: &TaggedTlObject
    ) -> Result<Option<TLObject>> {
        let peers = AdnlPeers::with_keys(self.node_key.id().clone(), dst.clone());
        let result = self.adnl.clone()
            .query_with_prefix(Some(&self.query_prefix[..]), query, &peers, None)
            .await?;
        self.set_query_result(result, dst)
    } 

    fn search_dht_key(&self, key: &DhtKeyId) -> Option<DhtValue> { 
        let version = Version::get();
        if let Some(value) = self.storage.get(key) {
            if value.val().object.ttl > version {
                Some(value.val().object.clone())
            } else {
                None
            }
        } else {
            None
        }
    }

    fn set_good_peer(&self, peer: &Arc<KeyId>) {
        loop {
            if let Some(count) = self.bad_peers.get(peer) {
                let cnt = count.val().load(Ordering::Relaxed);
                if cnt >= Self::MAX_FAIL_COUNT {
                    if count.val().compare_exchange(
                        cnt, 
                        cnt - 1, 
                        Ordering::Relaxed, 
                        Ordering::Relaxed
                    ).is_err() {
                        continue
                    }
                    log::info!(target: TARGET, "Make DHT peer {} feel good {}", peer, cnt - 1);
                }
            }
            break
        }
    }

    fn set_query_result(
        &self, 
        result: Option<TLObject>, 
        peer: &Arc<KeyId>
    ) -> Result<Option<TLObject>> {
        if result.is_some() {
            self.set_good_peer(peer)
        } else {
            loop {
                if let Some(count) = self.bad_peers.get(peer) {
                    let mut cnt = count.val().load(Ordering::Relaxed);
                    if cnt <= Self::MAX_FAIL_COUNT {
                        cnt = count.val().fetch_add(2, Ordering::Relaxed) + 2;
                    }
                    log::info!(target: TARGET, "Make DHT peer {} feel bad {}", peer, cnt);
                    break
                }
                add_unbound_object_to_map(
                    &self.bad_peers,
                    peer.clone(),
                    || Ok(AtomicU8::new(0))
                )?;
            }
        }
        Ok(result)
    }
    
    fn sign_key_description(name: &str, key: &Arc<dyn KeyOption>) -> Result<DhtKeyDescription> {
        let key_description = DhtKeyDescription {
            id: key.try_into()?,
            key: Self::dht_key_from_key_id(key.id(), name),
            signature: Default::default(),
            update_rule: UpdateRule::Dht_UpdateRule_Signature
        };
        key_description.sign(key)
    }    

    fn sign_local_node(&self) -> Result<Node> {
        let local_node = Node {
            id: (&self.node_key).try_into()?,
            addr_list: self.adnl.build_address_list(None)?,
            signature: Default::default(),
            version: Version::get()
        };
        local_node.sign(&self.node_key)
    }

    fn sign_value(name: &str, value: Vec<u8>, key: &Arc<dyn KeyOption>) -> Result<DhtValue> {
        let value = DhtValue {
            key: Self::sign_key_description(name, key)?,
            ttl: Version::get() + Self::TIMEOUT_VALUE,
            signature: Default::default(),
            value: value.into()
        };
        value.sign(key)
    }

    async fn store_value(
        dht: &Arc<Self>, 
        key: DhtKey, 
        value: DhtValue,
        check_type: impl Fn(&TLObject) -> bool + Copy + Send + 'static,
        check_all: bool,
        check_vals: impl Fn(Vec<(DhtKeyDescription, TLObject)>) -> Result<bool>
    ) -> Result<bool> {
        let key_id = Arc::new(hash(key)?);
        let query = TaggedTlObject {
            object: TLObject::new(
                Store {
                    value
                }
            ),
            #[cfg(feature = "telemetry")]
            tag: dht.tag_store
        };
        let query = Arc::new(query);
        let policy = DhtSearchPolicy::FullSearch(Self::MAX_TASKS);
        let mut iter = None;
        let mut peer = dht.get_known_peer(&mut iter);
        while peer.is_some() {
            let (wait, mut queue_reader) = Wait::new();
            while let Some(next) = peer {
                peer = dht.get_known_peer(&mut iter);
                let dht = dht.clone();  
                let query = query.clone();
                let wait = wait.clone();
                wait.request();
                tokio::spawn(
                    async move {
                        let ret = match dht.query(&next, &query).await {
                            Ok(Some(answer)) => {
                                match Query::parse::<TLObject, Stored>(answer, &query.object) {
                                    Ok(_) => Some(()), // Probably stored
                                    Err(answer) => {
                                        log::debug!(
                                            target: TARGET, 
                                            "Improper store reply: {:?}", 
                                            answer
                                        );
                                        None
                                    }
                                }
                            },
                            Ok(None) => None, // No reply at all 
                            Err(e) => {
                                log::warn!(target: TARGET, "Store error: {:?}", e);
                                None
                            }
                        };
                        wait.respond(ret)
                    }
                );
            }
            while wait.wait(&mut queue_reader, false).await.is_some() { 
            }
            let vals = DhtNode::find_value(
                dht, 
                &key_id, 
                check_type,
                &policy, 
                check_all, 
                &mut None
            ).await?;
            if check_vals(vals)? {
                return Ok(true)
            }
            peer = dht.get_known_peer(&mut iter);
        }
        Ok(false)
    }

    async fn value_query(
        &self, 
        peer: &Arc<KeyId>, 
        query: &Arc<TaggedTlObject>,
        key: &Arc<DhtKeyId>,
        check: impl Fn(&TLObject) -> bool
    ) -> Result<Option<(DhtKeyDescription, TLObject)>> {
        let answer = self.query(peer, query).await?;
        if let Some(answer) = answer {
            let answer: DhtValueResult = Query::parse(answer, &query.object)?;
            match answer {
                DhtValueResult::Dht_ValueFound(value) => {
                    let value = value.value.only();
                    log::debug!(
                        target: TARGET, 
                        "Found value for DHT key ID {}: {:?}/{:?}", 
                        base64_encode(&key[..]), value.key, value.value
                    );
                    let object = deserialize_boxed(&value.value)?;
                    if check(&object) {
                        return Ok(Some((value.key, object)))
                    } 
                    log::debug!(
                        target: TARGET,
                        "Improper value found, object {:?}", 
                        object
                    );
                },
                DhtValueResult::Dht_ValueNotFound(nodes) => {
                    let nodes = nodes.nodes.nodes;
                    log::debug!(
                        target: TARGET, 
                        "Value not found on {} for DHT key ID {}, suggested {} other nodes",
                        peer, base64_encode(&key[..]), nodes.len()
                    );
                    for node in nodes.iter() {          
                        self.add_peer(node)?;
                    }
                }
            }
        } else {
            log::debug!(
                target: TARGET, 
                "No answer from {} to FindValue with DHT key ID {} query", 
                peer, base64_encode(&key[..])
            );
        }
        Ok(None) 
    }

    fn verify_other_node(&self, node: &Node) -> Result<()> {
        let other_key: Arc<dyn KeyOption> = (&node.id).try_into()?;
        let mut node = node.clone();
        node.verify(&other_key)
    }

    fn verify_value(&self, value: &mut DhtValue) -> Result<()> {
        let other_key: Arc<dyn KeyOption> = (&value.key.id).try_into()?;
        value.verify(&other_key)?;
        value.key.verify(&other_key)
    }

}

#[async_trait::async_trait]
impl Subscriber for DhtNode {

    #[cfg(feature = "telemetry")]
    async fn poll(&self, _start: &Arc<Instant>) {
        self.telemetry.peers.update(self.allocated.peers.load(Ordering::Relaxed));
        self.telemetry.values.update(self.allocated.values.load(Ordering::Relaxed));
    }

    async fn try_consume_query(
        &self, 
        object: TLObject, 
        _peers: &AdnlPeers
    ) -> Result<QueryResult> {
        let object = match object.downcast::<DhtPing>() {
            Ok(query) => return QueryResult::consume(
                self.process_ping(&query)?, 
                #[cfg(feature = "telemetry")]
                None
            ),
            Err(object) => object
        };
        let object = match object.downcast::<FindNode>() {
            Ok(query) => return QueryResult::consume(
                self.process_find_node(&query)?, 
                #[cfg(feature = "telemetry")]
                None
            ),
            Err(object) => object
        };
        let object = match object.downcast::<FindValue>() {
            Ok(query) => return QueryResult::consume_boxed(
                self.process_find_value(&query)?, 
                #[cfg(feature = "telemetry")]
                None
            ),
            Err(object) => object
        };
        let object = match object.downcast::<GetSignedAddressList>() {
            Ok(_) => return QueryResult::consume(
                self.get_signed_node()?, 
                #[cfg(feature = "telemetry")]
                None
            ),
            Err(object) => object
        };
        match object.downcast::<Store>() {
            Ok(query) => QueryResult::consume_boxed(
                self.process_store(query)?, 
                #[cfg(feature = "telemetry")]
                None
            ),
            Err(object) => {
                log::warn!(target: TARGET, "Unexpected DHT query {:?}", object);
                Ok(QueryResult::Rejected(object))
            }        
        }
    }    

    async fn try_consume_query_bundle(
        &self, 
        mut objects: Vec<TLObject>,
        peers: &AdnlPeers
    ) -> Result<QueryResult> {
        if objects.len() != 2 {
            return Ok(QueryResult::RejectedBundle(objects));
        }
        let other_node = match objects.remove(0).downcast::<DhtQuery>() {
            Ok(query) => query.node,
            Err(object) => {
                objects.insert(0, object); 
                return Ok(QueryResult::RejectedBundle(objects));
            }
        };  
        self.add_peer(&other_node)?;
        let ret = self.try_consume_query(objects.remove(0), peers).await?;
        if let QueryResult::Rejected(object) = ret {
            fail!("Unexpected DHT query {:?}", object);
        }
        Ok(ret)
    }    

}
