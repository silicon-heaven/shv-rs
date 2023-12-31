use async_std::{channel, task};
use async_std::net::TcpListener;
use crate::broker::config::{AccessControl, BrokerConfig, Password};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use async_std::channel::unbounded;
use glob::{Pattern};
use log::{debug, error, info, Level, log, warn};
use crate::{List, RpcMessage, RpcMessageMetaTags, RpcValue, rpcvalue, util};
use crate::rpc::{Subscription, SubscriptionPattern};
use crate::rpcframe::RpcFrame;
use crate::rpcmessage::{CliId, RpcError, RpcErrorCode, RqId};
use crate::shvnode::{find_longest_prefix, METH_DIR, process_local_dir_ls, RequestCommand, ShvNode, SIG_CHNG};
use crate::util::{sha1_hash, split_glob_on_match};
use crate::broker::node::{BrokerNodeCommand, METH_SUBSCRIBE};
use async_std::stream::StreamExt;
use futures::select;
use crate::broker::call::{BrokerCommand};
use futures::FutureExt;

pub mod config;
pub mod peer;
pub mod node;
mod call;
#[cfg(test)]
mod test;

type Sender<T> = async_std::channel::Sender<T>;
type Receiver<T> = async_std::channel::Receiver<T>;

pub(crate) enum LoginResult {
    Ok,
    ClientSocketClosed,
}

#[derive(Debug)]
pub(crate) enum PeerToBrokerMessage {
    GetPassword {
        client_id: CliId,
        user: String,
    },
    NewPeer {
        client_id: CliId,
        sender: Sender<BrokerToPeerMessage>,
        peer_kind: PeerKind,
    },
    RegisterDevice {
        client_id: CliId,
        device_id: Option<String>,
        mount_point: Option<String>,
    },
    #[allow(dead_code)]
    SetSubscribePath {
        // used for testing, to bypass device discovery RPC Calls
        client_id: CliId,
        subscribe_path: SubscribePath,
    },
    FrameReceived {
        client_id: CliId,
        frame: RpcFrame,
    },
    PeerGone {
        client_id: CliId,
    },
}

#[derive(Debug)]
pub(crate) enum BrokerToPeerMessage {
    PasswordSha1(Option<Vec<u8>>),
    SendFrame(RpcFrame),
    SendMessage(RpcMessage),
    DisconnectByBroker,
}

#[derive(Debug)]
pub enum PeerKind {
    Client,
    ParentBroker,
}

#[derive(Debug)]
struct Peer {
    sender: Sender<BrokerToPeerMessage>,
    user: String,
    mount_point: Option<String>,
    subscriptions: Vec<SubscriptionPattern>,
    peer_kind: PeerKind,
    subscribe_path: Option<SubscribePath>,
}

impl Peer {
    fn new(peer_kind: PeerKind, sender: Sender<BrokerToPeerMessage>) -> Self {
        Self {
            sender,
            user: "".to_string(),
            mount_point: None,
            subscriptions: vec![],
            peer_kind,
            subscribe_path: None,
        }
    }
    fn is_signal_subscribed(&self, path: &str, method: &str) -> bool {
        for subs in self.subscriptions.iter() {
            if subs.match_shv_method(path, method) {
                return true;
            }
        }
        false
    }
    pub fn is_broker(&self) -> crate::Result<bool> {
        match &self.subscribe_path {
            None => { Err(format!("Device mounted on: {:?} - not checked for broker capability yet.", self.mount_point).into()) }
            Some(path) => {
                match path {
                    SubscribePath::NotBroker => { Ok(false) }
                    SubscribePath::CanSubscribe(_) => { Ok(true) }
                }
            }
        }
    }
    pub fn broker_subscribe_path(&self) -> crate::Result<String> {
        match &self.subscribe_path {
            None => { Err(format!("Device mounted on: {:?} - not checked for broker capability yet.", self.mount_point).into()) }
            Some(path) => {
                match path {
                    SubscribePath::NotBroker => { Err("Not broker".into()) }
                    SubscribePath::CanSubscribe(path) => { Ok(path.to_string()) }
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum SubscribePath {
    NotBroker,
    CanSubscribe(String),
}

struct Device {
    client_id: CliId,
}

impl Device {
}

type Node<K> = Box<dyn ShvNode<K> + Send + Sync>;

enum Mount<K> {
    Peer(Device),
    Node(Node<K>),
}

struct ParsedAccessRule {
    path_method: SubscriptionPattern,
    grant: String,
}

impl ParsedAccessRule {
    pub fn new(path: &str, method: &str, grant: &str) -> crate::Result<Self> {
        let method = if method.is_empty() { "?*" } else { method };
        let path = if path.is_empty() { "**" } else { path };
        match Pattern::new(method) {
            Ok(method) => {
                match Pattern::new(path) {
                    Ok(path) => {
                        Ok(Self {
                            path_method: SubscriptionPattern { paths: path, methods: method },
                            grant: grant.to_string(),
                        })
                    }
                    Err(err) => { Err(format!("{}", &err).into()) }
                }
            }
            Err(err) => { Err(format!("{}", &err).into()) }
        }
    }
}

fn find_mount<'a, 'b, K>(mounts: &'a mut BTreeMap<String, Mount<K>>, shv_path: &'b str) -> Option<(&'a mut Mount<K>, &'b str)> {
    if let Some((mount_dir, node_dir)) = find_longest_prefix(mounts, shv_path) {
        Some((mounts.get_mut(mount_dir).unwrap(), node_dir))
    } else {
        None
    }
}
struct PendingRpcCall {
    client_id: CliId,
    request_id: RqId,
    response_sender: Sender<RpcFrame>,
}
struct Broker {
    peers: HashMap<CliId, Peer>,
    mounts: BTreeMap<String, Mount<crate::broker::node::BrokerNodeCommand>>,
    access: AccessControl,
    role_access: HashMap<String, Vec<ParsedAccessRule>>,
    pending_rpc_calls: Vec<PendingRpcCall>,
    broker_command_sender: Sender<BrokerCommand>,
}

impl Broker {
    fn new(access: AccessControl, broker_command_sender: Sender<BrokerCommand>) -> Self {
        let mut role_access: HashMap<String, Vec<ParsedAccessRule>> = Default::default();
        for (name, role) in &access.roles {
            let mut list: Vec<ParsedAccessRule> = Default::default();
            for rule in &role.access {
                match ParsedAccessRule::new(&rule.paths, &rule.methods, &rule.grant) {
                    Ok(rule) => {
                        list.push(rule);
                    }
                    Err(err) => {
                        error!("Parse access rule error: {}", err);
                    }
                }
            }
            if !list.is_empty() {
                role_access.insert(name.clone(), list);
            }
        }
        Self {
            peers: Default::default(),
            mounts: Default::default(),
            access: access,
            role_access,
            pending_rpc_calls: vec![],
            broker_command_sender,
        }
    }
    //async fn send_message_to_peer(&self, client_id: CliId, message: RpcMessage) -> crate::Result<()> {
    //    let peer = self.peers.get(&client_id).ok_or_else(|| "Invalid client ID")?;
    //    peer.sender.send(BrokerToPeerMessage::SendMessage(message)).await?;
    //    Ok(())
    //}
    async fn start_broker_rpc_call(&mut self, request: RpcMessage, pending_call: PendingRpcCall) -> crate::Result<()> {
        //self.pending_calls.retain(|r| !r.sender.is_closed());
        let peer = self.peers.get(&pending_call.client_id).ok_or(format!("Invalid client ID: {}", pending_call.client_id))?;
        // let rqid = data.request.request_id().ok_or("Missing request ID")?;
        peer.sender.send(BrokerToPeerMessage::SendMessage(request)).await?;
        self.pending_rpc_calls.push(pending_call);
        Ok(())
    }
    async fn process_pending_broker_rpc_call(&mut self, client_id: CliId, response_frame: RpcFrame) -> crate::Result<()> {
        assert!(response_frame.is_response());
        assert!(response_frame.caller_ids().is_empty());
        let rqid = response_frame.request_id().ok_or_else(|| "Request ID must be set.")?;
        let mut pending_call_ix = None;
        for (ix, pc) in self.pending_rpc_calls.iter().enumerate() {
            if pc.request_id == rqid && pc.client_id == client_id {
                pending_call_ix = Some(ix);
                break;
            }
        }
        if let Some(ix) = pending_call_ix {
            let pending_call = self.pending_rpc_calls.remove(ix);
            pending_call.response_sender.send(response_frame).await?;
        }
        Ok(())
    }
    pub async fn process_broker_command(&mut self, broker_command: BrokerCommand) -> crate::Result<()> {
        match broker_command {
            BrokerCommand::RpcCall{ client_id, request, response_sender } => {
                let rq_id = request.request_id().unwrap_or_default();
                let mut rq2 = request;
                // broker calls can have any access level, set 'su' to bypass client access control
                rq2.set_access("su");
                self.start_broker_rpc_call(rq2, PendingRpcCall {
                    client_id,
                    request_id: rq_id,
                    response_sender,
                }).await
            }
            BrokerCommand::SetSubscribePath{ client_id, subscribe_path } => { self.set_subscribe_path(client_id, subscribe_path) }
            BrokerCommand::PropagateSubscriptions{ client_id } => { self.propagate_subscriptions_to_mounted_device(client_id).await }
            //BrokerCommand::CallSubscribe(data) => { self.call_subscribe_async(data.client_id, data.subscriptions) }
        }
    }
    fn set_subscribe_path(&mut self, client_id: CliId, subscribe_path: SubscribePath) -> crate::Result<()> {
        let peer = self.peers.get_mut(&client_id).ok_or_else(|| format!("Invalid client ID: {client_id}"))?;
        log!(target: "Subscr", Level::Debug, "Device mounted on: {:?}, client ID: {client_id} has subscribe method path set to: {:?}", peer.mount_point, subscribe_path);
        peer.subscribe_path = Some(subscribe_path);
        Ok(())
    }
    pub fn sha_password(&self, user: &str) -> Option<Vec<u8>> {
        match self.access.users.get(user) {
            None => None,
            Some(user) => {
                match &user.password {
                    Password::Plain(password) => {
                        Some(sha1_hash(password.as_bytes()))
                    }
                    Password::Sha1(password) => {
                        Some(password.as_bytes().into())
                    }
                }
            }
        }
    }
    async fn mount_device(&mut self, client_id: i32, device_id: Option<String>, mount_point: Option<String>) -> crate::Result<()> {
        let client_path = format!(".app/broker/client/{}", client_id);
        self.mounts.insert(client_path, Mount::Peer(Device { client_id }));
        let mount_point = loop {
            if let Some(ref mount_point) = mount_point {
                if mount_point.starts_with("test/") {
                    info!("Client id: {} mounted on path: '{}'", client_id, &mount_point);
                    break Some(mount_point.clone());
                }
            }
            if let Some(device_id) = device_id {
                break match self.access.mounts.get(&device_id) {
                    None => {
                        warn!("Cannot find mount-point for device ID: {device_id}");
                        None
                    }
                    Some(mount) => {
                        let mount_point = mount.mount_point.clone();
                        info!("Client id: {}, device id: {} mounted on path: '{}'", client_id, device_id, &mount_point);
                        Some(mount_point)
                    }
                };
            }
            break None;
        };
        if let Some(mount_point) = &mount_point {
            if let Some(peer) = self.peers.get_mut(&client_id) {
                peer.mount_point = Some(mount_point.clone());
                self.mounts.insert(mount_point.clone(), Mount::Peer(Device { client_id }));
                self.device_capabilities_discovery_async(client_id)?;
            } else {
                warn!("Cannot mount device with invalid client ID: {client_id}");
            }
        } else {
            self.set_subscribe_path(client_id, SubscribePath::NotBroker)?;
        };
        Ok(())
    }
    fn propagate_subscription_to_matching_devices(&mut self, new_subscription: &Subscription) -> crate::Result<()> {
        // compare new_subscription with all mount-points to check
        // whether it should be propagated also to mounted device
        let mut to_subscribe = vec![];
        for (mount_point, mount) in self.mounts.iter() {
            if let Mount::Peer(device) = mount {
                if let Some(peer) = self.peers.get(&device.client_id) {
                    if peer.is_broker()? {
                        if let Ok(Some((_local, remote))) = split_glob_on_match(&new_subscription.paths, mount_point) {
                            to_subscribe.push((device.client_id, Subscription::new(remote, &new_subscription.methods)));
                        }
                    }
                }
            }
        }
        for (client_id, subscription) in to_subscribe {
            let to_subscribe = vec![subscription];
            self.call_subscribe_async(client_id, to_subscribe)?;
        }
        Ok(())
    }
    fn device_capabilities_discovery_async(&mut self, client_id: i32) -> crate::Result<()> {
        // let peer = self.peers.get(&client_id).ok_or_else(|| format!("Invalid client ID: {client_id}"))?;
        // let mount_point = peer.mount_point.clone().ok_or_else(|| format!("Mount point is missing, client ID: {client_id}"))?;
        let broker_command_sender = self.broker_command_sender.clone();
        task::spawn(async move {
            async fn check_path(client_id: CliId, path: &str, broker_command_sender: &Sender<BrokerCommand>) -> crate::Result<bool> {
                let (response_sender, response_receiver) = unbounded();
                let request = RpcMessage::new_request(path, METH_DIR, Some(METH_SUBSCRIBE.into()));
                let cmd = BrokerCommand::RpcCall {
                    client_id,
                    request,
                    response_sender,
                };
                broker_command_sender.send(cmd).await?;
                let resp = response_receiver.recv().await?.to_rpcmesage()?;
                if let Ok(val) = resp.result() {
                    if !val.is_null() {
                        let cmd = BrokerCommand::SetSubscribePath {
                            client_id,
                            subscribe_path: SubscribePath::CanSubscribe(path.to_string()),
                        };
                        broker_command_sender.send(cmd).await?;
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            let found_cmd = BrokerCommand::PropagateSubscriptions { client_id };
            if let Some(true) = check_path(client_id, "/.app/broker/currentClient", &broker_command_sender).await.ok() {
                let _ = broker_command_sender.send(found_cmd).await;
            } else if let Some(true) = check_path(client_id, ".app/broker/currentClient", &broker_command_sender).await.ok() {
                let _ = broker_command_sender.send(found_cmd).await;
            } else if let Some(true) = check_path(client_id,".broker/app", &broker_command_sender).await.ok() {
                let _ = broker_command_sender.send(found_cmd).await;
            } else {
                let cmd = BrokerCommand::SetSubscribePath {
                    client_id,
                    subscribe_path: SubscribePath::NotBroker,
                };
                let _ = broker_command_sender.send(cmd).await;
            }
        });
        Ok(())
    }
    async fn propagate_subscriptions_to_mounted_device(&mut self, client_id: CliId) -> crate::Result<()> {
        let peer = self.peers.get_mut(&client_id).ok_or_else(|| format!("Invalid client ID: {client_id}"))?;
        //let subscribe_path = peer.broker_subscribe_path()?;
        let mount_point = peer.mount_point.clone().ok_or_else(|| format!("Mount point is missing, client ID: {client_id}"))?;
        let mut subscribed = HashMap::new();
        for (_id, peer) in &self.peers {
            //if id == &client_id { continue }
            for subscr in &peer.subscriptions {
                subscribed.insert(subscr.to_string(), (subscr.paths.as_str(), subscr.methods.as_str()));
            }
        }
        let mut to_subscribe = HashSet::new();
        for (_sig, (paths, methods)) in subscribed {
            if let Ok(Some((_local, remote))) = split_glob_on_match(paths, &mount_point) {
                to_subscribe.insert(Subscription::new(remote, methods).to_string());
            } else {
                error!("Invalid pattern '{paths}'.")
            }
        }
        let to_subscribe = to_subscribe.iter().map(|s| Subscription::from_str(s)).collect();
        self.call_subscribe_async(client_id, to_subscribe)?;
        Ok(())
    }
    fn call_subscribe_async(&mut self, client_id: CliId, subscriptions: Vec<Subscription>) -> crate::Result<()> {
        let peer = self.peers.get_mut(&client_id).ok_or_else(|| format!("Invalid client ID: {client_id}"))?;
        let mount_point = peer.mount_point.clone();
        let broker_command_sender = self.broker_command_sender.clone();
        //info!("call_subscribe_async mount: {:?} client_id {:?} subscriptions {:?}", mount_point, client_id, subscriptions);
        let subscribe_path = peer.broker_subscribe_path()?;
        task::spawn(async move {
            async fn call_subscribe(client_id: CliId, subscribe_path: &str, subscription: Subscription, broker_command_sender: &Sender<BrokerCommand>) -> crate::Result<()> {
                let (response_sender, response_receiver) = unbounded();
                let cmd = BrokerCommand::RpcCall {
                    client_id,
                    request: RpcMessage::new_request(subscribe_path, METH_SUBSCRIBE, Some(subscription.to_rpcvalue())),
                    response_sender,
                };
                broker_command_sender.send(cmd).await?;
                response_receiver.recv().await?.to_rpcmesage()?;
                Ok(())
            }
            for subscription in subscriptions {
                log!(target: "Subscr", Level::Debug, "Propagating subscription {} to client ID: {client_id} mounted on {:?}", subscription, mount_point);
                let _ = call_subscribe(client_id, &subscribe_path, subscription, &broker_command_sender).await;
            }
        });
        Ok(())
    }
    pub fn client_id_to_mount_point(&self, client_id: CliId) -> Option<String> {
        match self.peers.get(&client_id) {
            None => None,
            Some(peer) => peer.mount_point.clone()
        }
    }
    fn flatten_roles(&self, user: &str) -> Option<Vec<String>> {
        if let Some(user) = self.access.users.get(user) {
            let mut queue: VecDeque<String> = VecDeque::new();
            fn enqueue(queue: &mut VecDeque<String>, role: &str) {
                let role = role.to_string();
                if !queue.contains(&role) {
                    queue.push_back(role);
                }
            }
            for role in user.roles.iter() { enqueue(&mut queue, role); }
            let mut flatten_roles = Vec::new();
            while !queue.is_empty() {
                let role_name = queue.pop_front().unwrap();
                if let Some(role) = self.access.roles.get(&role_name) {
                    for role in role.roles.iter() { enqueue(&mut queue, role); }
                }
                flatten_roles.push(role_name);
            }
            Some(flatten_roles)
        } else {
            None
        }
    }
    fn grant_for_request(&self, client_id: CliId, frame: &RpcFrame) -> Result<String, String> {
        log!(target: "Acc", Level::Debug, "======================= grant_for_request {}", &frame);
        let shv_path = frame.shv_path().unwrap_or_default();
        let method = frame.method().unwrap_or("");
        if method.is_empty() {
            Err("Method is empty".into())
        } else {
            let peer = self.peers.get(&client_id).ok_or("Peer not found")?;
            match peer.peer_kind {
                PeerKind::Client => {
                    if let Some(flatten_roles) = self.flatten_roles(&peer.user) {
                        log!(target: "Acc", Level::Debug, "user: {}, flatten roles: {:?}", &peer.user, flatten_roles);
                        for role_name in flatten_roles {
                            if let Some(rules) = self.role_access.get(&role_name) {
                                log!(target: "Acc", Level::Debug, "----------- access for role: {}", role_name);
                                for rule in rules {
                                    log!(target: "Acc", Level::Debug, "\trule: {}", rule.path_method);
                                    if rule.path_method.match_shv_method(shv_path, method) {
                                        log!(target: "Acc", Level::Debug, "\t\t HIT");
                                        return Ok(rule.grant.clone());
                                    }
                                }
                            }
                        }
                    }
                    return Err(format!("Access denied for user: {}", peer.user));
                }
                PeerKind::ParentBroker => {
                    match frame.access() {
                        None => { return Err("Access denied, no access grant set by broker".into()); }
                        Some(access) => { return Ok(access.to_owned()); }
                    };
                }
            }
        }
    }
    fn peer_to_info(client_id: CliId, peer: &Peer) -> rpcvalue::Map {
        let subs: List = peer.subscriptions.iter().map(|subs| subs.to_rpcvalue()).collect();
        rpcvalue::Map::from([
            ("clientId".to_string(), client_id.into()),
            ("userName".to_string(), RpcValue::from(&peer.user)),
            ("mountPoint".to_string(), RpcValue::from(peer.mount_point.clone().unwrap_or_default())),
            ("subscriptions".to_string(), subs.into()),
        ]
        )
    }
    fn client_info(&mut self, client_id: CliId) -> Option<rpcvalue::Map> {
        match self.peers.get(&client_id) {
            None => { None }
            Some(peer) => {
                Some(Broker::peer_to_info(client_id, peer))
            }
        }
    }
    async fn disconnect_client(&mut self, client_id: CliId) -> crate::Result<()> {
        let peer = self.peers.get(&client_id).ok_or_else(|| "Invalid client ID")?;
        peer.sender.send(BrokerToPeerMessage::DisconnectByBroker).await?;
        Ok(())
    }
    fn mounted_client_info(&mut self, mount_point: &str) -> Option<rpcvalue::Map> {
        for (client_id, peer) in &self.peers {
            if let Some(mount_point1) = &peer.mount_point {
                if mount_point1 == mount_point {
                    return Some(Broker::peer_to_info(*client_id, peer));
                }
            }
        }
        None
    }
    fn clients(&self) -> rpcvalue::List {
        let ids: rpcvalue::List = self.peers.iter().map(|(id, _)| RpcValue::from(*id)).collect();
        ids
    }
    fn mounts(&self) -> rpcvalue::List {
        self.peers.iter().filter(|(_id, peer)| peer.mount_point.is_some()).map(|(_id, peer)| {
            RpcValue::from(peer.mount_point.clone().unwrap())
        }).collect()
    }
    async fn subscribe(&mut self, client_id: CliId, subscription: &Subscription) -> crate::Result<()> {
        log!(target: "Subscr", Level::Debug, "New subscription for client id: {} - {}", client_id, subscription);
        let peer = self.peers.get_mut(&client_id).ok_or_else(|| format!("Invalid client ID: {client_id}"))?;
        peer.subscriptions.push(SubscriptionPattern::from_subscription(subscription)?);
        self.propagate_subscription_to_matching_devices(&subscription)?;
        Ok(())
    }
    fn unsubscribe(&mut self, client_id: CliId, subscription: &Subscription) -> crate::Result<bool> {
        log!(target: "Subscr", Level::Debug, "Remove subscription for client id: {} - {}", client_id, subscription);
        let peer = self.peers.get_mut(&client_id).ok_or_else(|| format!("Invalid client ID: {client_id}"))?;
        let cnt = peer.subscriptions.len();
        let subscr_pattern = SubscriptionPattern::from_subscription(subscription)?;
        peer.subscriptions.retain(|subscr| subscr != &subscr_pattern);
        Ok(cnt != peer.subscriptions.len())
    }
    fn subscriptions(&self, client_id: CliId) -> crate::Result<List> {
        let peer = self.peers.get(&client_id).ok_or_else(|| format!("Invalid client ID: {client_id}"))?;
        let subs: List = peer.subscriptions.iter().map(|subs| subs.to_rpcvalue()).collect();
        Ok(subs)
    }
}

pub(crate) async fn broker_loop(peers_messages: Receiver<PeerToBrokerMessage>, access: AccessControl) {
    let (broker_command_sender, broker_command_receiver) = unbounded();
    let mut broker = Broker::new(access, broker_command_sender);

    broker.mounts.insert(".app".into(), Mount::Node(Box::new(crate::shvnode::AppNode { app_name: "shvbroker", ..Default::default() })));
    broker.mounts.insert(".app/broker".into(), Mount::Node(Box::new(node::AppBrokerNode {})));
    broker.mounts.insert(".app/broker/currentClient".into(), Mount::Node(Box::new(node::AppBrokerCurrentClientNode {})));
    loop {
        select! {
            command = broker_command_receiver.recv().fuse() => match command {
                Ok(command) => {
                    if let Err(err) = broker.process_broker_command(command).await {
                        warn!("Process broker command error: {}", err);
                    }
                }
                Err(err) => {
                    warn!("Reseive broker command error: {}", err);
                }
            },
            event = peers_messages.recv().fuse() => match event {
                Err(e) => {
                    info!("Peers channel closed, accept loop exited: {}", &e);
                    break;
                }
                Ok(message) => {
                    match message {
                        PeerToBrokerMessage::FrameReceived { client_id, mut frame} => {
                            if frame.is_request() {
                                type Command = RequestCommand<BrokerNodeCommand>;
                                let shv_path = frame.shv_path().unwrap_or_default().to_string();
                                let response_meta= RpcFrame::prepare_response_meta(&frame.meta);

                                let grant_for_request = broker.grant_for_request(client_id, &frame);
                                let command = match grant_for_request {
                                    Err(err) => {
                                        Command::Error(RpcError::new(RpcErrorCode::PermissionDenied, &err))
                                    }
                                    Ok(grant) => {
                                        if let Some(local_result) = process_local_dir_ls(&broker.mounts, &frame) {
                                            local_result
                                        } else {
                                            if let Some((mount, node_path)) = find_mount(&mut broker.mounts, &shv_path) {
                                                match mount {
                                                    Mount::Peer(device) => {
                                                        let device_client_id = device.client_id;
                                                        frame.push_caller_id(client_id);
                                                        frame.set_shvpath(node_path);
                                                        frame.set_access(&grant);
                                                        let _ = broker.peers.get(&device_client_id).expect("client ID must exist").sender.send(BrokerToPeerMessage::SendFrame(frame)).await;
                                                        Command::Noop
                                                    }
                                                    Mount::Node(node) => {
                                                        if let Ok(rpcmsg) = frame.to_rpcmesage() {
                                                            let mut rpcmsg2 = rpcmsg;
                                                            rpcmsg2.set_shvpath(node_path);
                                                            rpcmsg2.set_access(&grant);
                                                            if node.is_request_granted(&rpcmsg2) {
                                                                node.process_request(&rpcmsg2)
                                                            } else {
                                                                let err = RpcError::new(RpcErrorCode::PermissionDenied, &format!("Method doesn't exist or request to call {}:{} is not granted.", shv_path, rpcmsg2.method().unwrap_or_default()));
                                                                Command::Error(err)
                                                            }
                                                        } else {
                                                            Command::Error(RpcError::new(RpcErrorCode::InvalidRequest, &format!("Cannot convert RPC frame to Rpc message")))
                                                        }
                                                    }
                                                }
                                            } else {
                                                let method = frame.method().unwrap_or_default();
                                                Command::Error(RpcError::new(RpcErrorCode::MethodNotFound, &format!("Invalid shv path {}:{}()", shv_path, method)))
                                            }
                                        }
                                    }
                                };
                                let command = if let Command::Custom(cc) = command {
                                    let result = match cc {
                                        BrokerNodeCommand::ClientInfo(client_id) => {
                                            match broker.client_info(client_id) {
                                                None => { Ok(RpcValue::null()) }
                                                Some(info) => { Ok(RpcValue::from(info)) }
                                            }
                                        }
                                        BrokerNodeCommand::MountedClientInfo(mount_point) => {
                                            match broker.mounted_client_info(&mount_point) {
                                                None => { Ok(RpcValue::from(())) }
                                                Some(info) => { Ok(RpcValue::from(info)) }
                                            }
                                        }
                                        BrokerNodeCommand::Clients => {
                                            Ok(RpcValue::from(broker.clients()))
                                        }
                                        BrokerNodeCommand::Mounts => {
                                            Ok(RpcValue::from(broker.mounts()))
                                        }
                                        BrokerNodeCommand::DisconnectClient(client_id) => {
                                            broker.disconnect_client(client_id).await.map(|_| RpcValue::null())
                                        }
                                        BrokerNodeCommand::CurrentClientInfo => {
                                            match broker.client_info(client_id) {
                                                None => { Ok(RpcValue::from(())) }
                                                Some(info) => { Ok(RpcValue::from(info)) }
                                            }
                                        }
                                        BrokerNodeCommand::Subscribe(subscr) => {
                                            let result = broker.subscribe(client_id, &subscr).await.map(|_| RpcValue::null());
                                            result
                                        }
                                        BrokerNodeCommand::Unsubscribe(subscr) => {
                                            broker.unsubscribe(client_id, &subscr).map(|r| RpcValue::from(r))
                                        }
                                        BrokerNodeCommand::Subscriptions => {
                                            let result = broker.subscriptions(client_id);
                                            result.map(|r| RpcValue::from(r))
                                        }
                                    };
                                    match result {
                                        Ok(val) => { Command::Result(val) }
                                        Err(err) => { Command::Error(RpcError::new(RpcErrorCode::MethodCallException, &err.to_string())) }
                                    }
                                } else {
                                    command
                                };
                                match command {
                                    RequestCommand::Noop => {}
                                    command => {
                                        if let Ok(meta) = response_meta {
                                            let peer = broker.peers.get(&client_id).unwrap();
                                            let command = if let Command::PropertyChanged(value) = command {
                                                let sig = RpcMessage::new_signal(&shv_path, SIG_CHNG, Some(value));
                                                peer.sender.send(BrokerToPeerMessage::SendMessage(sig)).await.unwrap();
                                                Command::Result(().into())
                                            } else {
                                                command
                                            };
                                            let mut resp = RpcMessage::from_meta(meta);
                                            match command {
                                                RequestCommand::Result(value) => {
                                                    resp.set_result(value);
                                                }
                                                RequestCommand::Error(error) => {
                                                    resp.set_error(error);
                                                }
                                                _ => {
                                                }
                                            }
                                            if resp.is_success() || resp.is_error() {
                                                if let Err(error) = peer.sender.send(BrokerToPeerMessage::SendMessage(resp)).await {
                                                    error!("Error writing to peer channel: {error}");
                                                }
                                            }
                                        } else {
                                            warn!("Invalid request frame received.");
                                        }
                                    }
                                }
                            } else if frame.is_response() {
                                if let Some(client_id) = frame.pop_caller_id() {
                                    let peer = broker.peers.get(&client_id).unwrap();
                                    peer.sender.send(BrokerToPeerMessage::SendFrame(frame)).await.unwrap();
                                } else {
                                    if let Err(err) = broker.process_pending_broker_rpc_call(client_id, frame).await {
                                        warn!("Process broker request error: {err}.");
                                    }
                                }
                            } else if frame.is_signal() {
                                if let Some(mount_point) = broker.client_id_to_mount_point(client_id) {
                                    let new_path = util::join_path(&mount_point, frame.shv_path().unwrap_or_default());
                                    for (cli_id, peer) in broker.peers.iter() {
                                        if &client_id != cli_id {
                                            if peer.is_signal_subscribed(&new_path, frame.method().unwrap_or_default()) {
                                                let mut frame = frame.clone();
                                                frame.set_shvpath(&new_path);
                                                peer.sender.send(BrokerToPeerMessage::SendFrame(frame)).await.unwrap();
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        PeerToBrokerMessage::NewPeer { client_id, sender, peer_kind } => {
                            if broker.peers.insert(client_id, Peer::new(peer_kind, sender)).is_some() {
                                // this might happen when connection to parent broker is restored
                                // after parent broker reset
                                // note that parent broker connection has always ID == 1
                                debug!("Client ID: {client_id} exists already!");
                            }
                        },
                        PeerToBrokerMessage::RegisterDevice { client_id, device_id, mount_point } => {
                            let _ = broker.mount_device(client_id, device_id, mount_point).await;
                        },
                        PeerToBrokerMessage::SetSubscribePath {client_id, subscribe_path} => {
                            let _ = broker.set_subscribe_path(client_id, subscribe_path);
                        }
                        PeerToBrokerMessage::PeerGone { client_id } => {
                            broker.peers.remove(&client_id);
                            info!("Client id: {} disconnected.", client_id);
                            if let Some(path) = broker.client_id_to_mount_point(client_id) {
                                info!("Unmounting path: '{}'", path);
                                broker.mounts.remove(&path);
                            }
                            let client_path = format!(".app/broker/client/{}", client_id);
                            broker.mounts.remove(&client_path);
                            broker.pending_rpc_calls.retain(|c| c.client_id != client_id);
                        }
                        PeerToBrokerMessage::GetPassword { client_id, user } => {
                            let shapwd = broker.sha_password(&user);
                            let peer = broker.peers.get_mut(&client_id).unwrap();
                            peer.user = user.clone();
                            peer.sender.send(BrokerToPeerMessage::PasswordSha1(shapwd)).await.unwrap();
                        }
                    }
                }
            }
        }
    }
}

pub async fn accept_loop(config: BrokerConfig, access: AccessControl) -> crate::Result<()> {
    if let Some(address) = config.listen.tcp.clone() {
        let (broker_sender, broker_receiver) = channel::unbounded();
        let parent_broker_peer_config = config.parent_broker.clone();
        let broker_task = task::spawn(crate::broker::broker_loop(broker_receiver, access));
        if parent_broker_peer_config.enabled {
            crate::spawn_and_log_error(peer::parent_broker_peer_loop_with_reconnect(1, parent_broker_peer_config, broker_sender.clone()));
        }
        info!("Listening on TCP: {}", address);
        let listener = TcpListener::bind(address).await?;
        info!("bind OK");
        let mut client_id = 2; // parent broker has client_id == 1
        let mut incoming = listener.incoming();
        while let Some(stream) = incoming.next().await {
            let stream = stream?;
            debug!("Accepting from: {}", stream.peer_addr()?);
            crate::spawn_and_log_error(peer::peer_loop(client_id, broker_sender.clone(), stream));
            client_id += 1;
        }
        drop(broker_sender);
        broker_task.await;
    } else {
        return Err("No port to listen on specified".into());
    }
    Ok(())
}

