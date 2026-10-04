//! `BeaconNode` — advertise this process's resources and keep a live view of
//! everyone else's.
//!
//! Ported from meta-discovery v1's `meshdisco.rs`, keeping the two details that
//! fail *silently* when wrong:
//!
//! 1. **Per-interface fan-out.** A socket that joins with `INADDR_ANY` and sends
//!    via the default route covers exactly one interface. `metashare-app` sits
//!    on `pcs` *and* `metamesh-mesh`; `metawatch-*` on `metawatch` *and*
//!    `metamesh-mesh`. We enumerate interfaces and join/send on each, using
//!    `ip_mreqn`'s `imr_ifindex`.
//! 2. **Expiry at read time.** No reaper owns correctness; the announce loop's
//!    prune only exists to fire change notifications for nodes that went quiet.
//!
//! Raw `libc` rather than `socket2` / `if-addrs`: every consumer already links
//! libc, and this is the whole of what we need from either crate.

use std::collections::HashMap;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tracing::{debug, info, warn};

use super::proto::{
    Message, NodeInfo, Resource, DEFAULT_GROUP, DEFAULT_INTERVAL, DEFAULT_PORT, LIVENESS_FACTOR,
    MAX_DATAGRAM, READ_BUFFER, TYPE_ADVERTISE, TYPE_BYE, TYPE_PROBE,
};

/// How a node is set up. Build with [`Config::new`] / [`Config::from_env`].
#[derive(Clone, Debug)]
pub struct Config {
    pub node: NodeInfo,
    pub resources: Vec<Resource>,
    pub group: String,
    pub port: u16,
    pub interval: Duration,
}

impl Config {
    /// Defaults: this host's name as the instance, the standard group/port,
    /// a 10 s interval, no resources.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            node: NodeInfo {
                name: name.into(),
                instance: hostname(),
                version: None,
                status: None,
            },
            resources: Vec::new(),
            group: DEFAULT_GROUP.into(),
            port: DEFAULT_PORT,
            interval: DEFAULT_INTERVAL,
        }
    }

    /// [`Config::new`] with `BEACON_GROUP` / `BEACON_PORT` / `BEACON_INTERVAL_MS`
    /// applied.
    pub fn from_env(name: impl Into<String>) -> Self {
        let mut c = Self::new(name);
        if let Some(g) = env_nonempty("BEACON_GROUP") {
            c.group = g;
        }
        if let Some(p) = env_nonempty("BEACON_PORT").and_then(|v| v.parse().ok()) {
            c.port = p;
        }
        if let Some(ms) = env_nonempty("BEACON_INTERVAL_MS").and_then(|v| v.parse::<u64>().ok()) {
            if ms > 0 {
                c.interval = Duration::from_millis(ms);
            }
        }
        c
    }

    pub fn version(mut self, v: impl Into<String>) -> Self {
        self.node.version = Some(v.into());
        self
    }

    pub fn instance(mut self, i: impl Into<String>) -> Self {
        self.node.instance = i.into();
        self
    }

    pub fn resource(mut self, r: Resource) -> Self {
        self.resources.push(r);
        self
    }
}

/// A node heard on the wire (or this node itself, via [`BeaconNode::self_node`]).
#[derive(Clone, Debug, Serialize)]
pub struct SeenNode {
    #[serde(flatten)]
    pub node: NodeInfo,
    pub resources: Vec<Resource>,
    /// Source address taken from the packet, never from the payload.
    pub addr: String,
    /// Unix seconds.
    #[serde(rename = "lastSeen")]
    pub last_seen: f64,
}

impl SeenNode {
    /// Every cap of every resource, in order, deduplicated.
    pub fn caps(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for c in self.resources.iter().flat_map(|r| r.caps.iter()) {
            if !out.contains(c) {
                out.push(c.clone());
            }
        }
        out
    }

    /// True if any resource matches `pattern`.
    pub fn matches(&self, pattern: &str) -> bool {
        self.resources.iter().any(|r| r.matches(pattern))
    }

    /// The first resource matching `pattern`.
    pub fn resource(&self, pattern: &str) -> Option<&Resource> {
        self.resources.iter().find(|r| r.matches(pattern))
    }
}

/// One resource together with the node advertising it.
#[derive(Clone, Debug, Serialize)]
pub struct SeenResource {
    pub node: NodeInfo,
    pub resource: Resource,
    pub addr: String,
    #[serde(rename = "lastSeen")]
    pub last_seen: f64,
}

struct Inner {
    group: Ipv4Addr,
    port: u16,
    interval: Duration,
    node: RwLock<NodeInfo>,
    resources: RwLock<Vec<Resource>>,
    socket: UdpSocket,
    ifindexes: Vec<(String, u32)>,
    seen: Mutex<HashMap<String, SeenNode>>,
    changes: watch::Sender<u64>,
    stopped: AtomicBool,
}

/// A running beacon v2 participant. Cheap to clone.
#[derive(Clone)]
pub struct BeaconNode {
    inner: Arc<Inner>,
}

impl BeaconNode {
    /// Bind, join the group on every eligible interface, and start the read and
    /// advertise loops. Must be called inside a tokio runtime.
    ///
    /// A join failure on one interface is logged and skipped rather than fatal —
    /// a host that blocks multicast must degrade, not fail to boot.
    pub fn spawn(cfg: Config) -> io::Result<BeaconNode> {
        let group: Ipv4Addr = cfg.group.parse().map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("bad group {}", cfg.group))
        })?;

        let std_sock = bind_reuse(cfg.port)?;
        set_multicast_ttl(&std_sock, 1)?;

        let ifindexes = eligible_interfaces();
        let mut joined = 0usize;
        for (name, idx) in &ifindexes {
            match join_group(&std_sock, group, *idx) {
                Ok(()) => joined += 1,
                Err(e) => warn!(iface = %name, error = %e, "beacon: join failed"),
            }
        }
        info!(
            group = %cfg.group, port = cfg.port, name = %cfg.node.name,
            instance = %cfg.node.instance, resources = cfg.resources.len(),
            joined, total = ifindexes.len(),
            "beacon: listening"
        );

        std_sock.set_nonblocking(true)?;
        let socket = UdpSocket::from_std(std_sock)?;
        let (changes, _) = watch::channel(0u64);

        let node = BeaconNode {
            inner: Arc::new(Inner {
                group,
                port: cfg.port,
                interval: cfg.interval,
                node: RwLock::new(cfg.node),
                resources: RwLock::new(cfg.resources),
                socket,
                ifindexes,
                seen: Mutex::new(HashMap::new()),
                changes,
                stopped: AtomicBool::new(false),
            }),
        };

        let reader = node.clone();
        tokio::spawn(async move { reader.read_loop().await });

        let advertiser = node.clone();
        tokio::spawn(async move {
            advertiser.advertise().await;
            advertiser.probe(&[]).await;
            let mut tick = tokio::time::interval(advertiser.inner.interval);
            tick.tick().await; // the immediate first tick
            loop {
                tick.tick().await;
                if advertiser.inner.stopped.load(Ordering::Relaxed) {
                    break;
                }
                advertiser.advertise().await;
                advertiser.prune();
            }
        });

        Ok(node)
    }

    /// The instance name this node advertises as.
    pub fn instance(&self) -> String {
        self.inner.node.read().unwrap().instance.clone()
    }

    /// Replace this node's resources and re-advertise at once (e.g. a manifest
    /// `rev` changed). A no-op if nothing changed.
    pub async fn set_resources(&self, resources: Vec<Resource>) {
        {
            let mut cur = self.inner.resources.write().unwrap();
            if *cur == resources {
                return;
            }
            *cur = resources;
        }
        self.advertise().await;
    }

    /// Set `node.status` (`starting` / `running`) and re-advertise.
    pub async fn set_status(&self, status: &str) {
        {
            let mut n = self.inner.node.write().unwrap();
            if n.status.as_deref() == Some(status) {
                return;
            }
            n.status = Some(status.to_string());
        }
        self.advertise().await;
    }

    /// Multicast a probe. Every node owning a resource matching one of `want`
    /// (every node, when `want` is empty) replies at once — what makes the
    /// protocol usable on a boot path instead of waiting out an interval.
    pub async fn probe(&self, want: &[String]) {
        let msg = Message::probe(&self.instance(), want);
        self.multicast(&msg).await;
    }

    /// Say goodbye: multicast `bye` and stop advertising. Call on clean
    /// shutdown so neighbours drop us at once instead of after the TTL.
    pub async fn bye(&self) {
        if self.inner.stopped.swap(true, Ordering::Relaxed) {
            return;
        }
        let node = self.inner.node.read().unwrap().clone();
        self.multicast(&Message::bye(node)).await;
    }

    /// Fires (the counter increments) whenever the set of live nodes or any
    /// node's resources change — new node, changed advertise, bye, expiry.
    /// Consumers re-read [`BeaconNode::resources`] on each change.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.inner.changes.subscribe()
    }

    /// Every live node, sorted by (name, instance).
    pub fn nodes(&self) -> Vec<SeenNode> {
        let cutoff = self.cutoff();
        let guard = self.inner.seen.lock().unwrap();
        let mut out: Vec<SeenNode> = guard.values().filter(|n| n.last_seen >= cutoff).cloned().collect();
        drop(guard);
        out.sort_by(|a, b| a.node.name.cmp(&b.node.name).then(a.node.instance.cmp(&b.node.instance)));
        out
    }

    /// One row per node name, most recently seen instance wins — the nav
    /// menu's shape.
    pub fn nodes_by_name(&self) -> Vec<SeenNode> {
        let mut best: HashMap<String, SeenNode> = HashMap::new();
        for n in self.nodes() {
            match best.get(&n.node.name) {
                Some(cur) if cur.last_seen >= n.last_seen => {}
                _ => {
                    best.insert(n.node.name.clone(), n);
                }
            }
        }
        let mut out: Vec<SeenNode> = best.into_values().collect();
        out.sort_by(|a, b| a.node.name.cmp(&b.node.name));
        out
    }

    /// Every live resource matching `pattern`, with its node.
    pub fn resources(&self, pattern: &str) -> Vec<SeenResource> {
        self.nodes()
            .into_iter()
            .flat_map(|n| {
                let SeenNode { node, resources, addr, last_seen } = n;
                resources.into_iter().filter(|r| r.matches(pattern)).map(move |resource| SeenResource {
                    node: node.clone(),
                    resource,
                    addr: addr.clone(),
                    last_seen,
                })
            })
            .collect()
    }

    /// This node as a row, so a UI can render itself without waiting for its
    /// own echo.
    pub fn self_node(&self) -> SeenNode {
        SeenNode {
            node: self.inner.node.read().unwrap().clone(),
            resources: self.inner.resources.read().unwrap().clone(),
            addr: String::new(),
            last_seen: now_secs(),
        }
    }

    // -- internals ----------------------------------------------------------

    fn cutoff(&self) -> f64 {
        now_secs() - self.inner.interval.as_secs_f64() * LIVENESS_FACTOR as f64
    }

    fn bump(&self) {
        self.inner.changes.send_modify(|g| *g = g.wrapping_add(1));
    }

    /// Drop expired nodes; notify if any went.
    fn prune(&self) {
        let cutoff = self.cutoff();
        let removed = {
            let mut guard = self.inner.seen.lock().unwrap();
            let before = guard.len();
            guard.retain(|_, n| n.last_seen >= cutoff);
            before != guard.len()
        };
        if removed {
            self.bump();
        }
    }

    fn advertise_msg(&self) -> Message {
        Message::advertise(
            self.inner.node.read().unwrap().clone(),
            self.inner.resources.read().unwrap().clone(),
        )
    }

    async fn advertise(&self) {
        if self.inner.stopped.load(Ordering::Relaxed) {
            return;
        }
        let msg = self.advertise_msg();
        self.multicast(&msg).await;
    }

    fn encode(msg: &Message) -> Option<Vec<u8>> {
        match serde_json::to_vec(msg) {
            Ok(b) => {
                if b.len() > MAX_DATAGRAM {
                    warn!(bytes = b.len(), max = MAX_DATAGRAM, "beacon: datagram over the fragmentation-safe size");
                }
                Some(b)
            }
            Err(e) => {
                warn!(error = %e, "beacon: encode failed");
                None
            }
        }
    }

    /// Writes once per interface. Letting the routing table choose would reach
    /// exactly one network — the failure this protocol exists to avoid.
    async fn multicast(&self, msg: &Message) {
        let Some(body) = Self::encode(msg) else { return };
        let dst = SocketAddr::V4(SocketAddrV4::new(self.inner.group, self.inner.port));
        for (name, idx) in &self.inner.ifindexes {
            if let Err(e) = set_multicast_if(&self.inner.socket, *idx) {
                warn!(iface = %name, error = %e, "beacon: select interface failed");
                continue;
            }
            if let Err(e) = self.inner.socket.send_to(&body, dst).await {
                warn!(iface = %name, error = %e, "beacon: send failed");
            }
        }
    }

    async fn reply_to(&self, dst: SocketAddr) {
        let Some(body) = Self::encode(&self.advertise_msg()) else { return };
        if let Err(e) = self.inner.socket.send_to(&body, dst).await {
            debug!(error = %e, "beacon: reply failed");
        }
    }

    async fn read_loop(self) {
        let mut buf = vec![0u8; READ_BUFFER];
        loop {
            let (n, src) = match self.inner.socket.recv_from(&mut buf).await {
                Ok(v) => v,
                Err(e) => {
                    debug!(error = %e, "beacon: read error");
                    continue;
                }
            };
            let Some(msg) = Message::parse(&buf[..n]) else { continue };
            let me = self.instance();

            match msg.msg_type.as_str() {
                TYPE_PROBE => {
                    if msg.from.as_deref() == Some(me.as_str()) || self.inner.stopped.load(Ordering::Relaxed) {
                        continue;
                    }
                    let wanted = msg.want.is_empty()
                        || self.inner.resources.read().unwrap().iter().any(|r| r.matches_any(&msg.want));
                    if wanted {
                        self.reply_to(src).await;
                    }
                }
                TYPE_ADVERTISE | TYPE_BYE => {
                    let Some(node) = msg.node else { continue };
                    if node.instance == me {
                        continue; // our own multicast echo
                    }
                    let changed = if msg.msg_type == TYPE_BYE {
                        self.inner.seen.lock().unwrap().remove(&node.instance).is_some()
                    } else {
                        let cutoff = self.cutoff();
                        let mut guard = self.inner.seen.lock().unwrap();
                        let changed = match guard.get(&node.instance) {
                            Some(prev) => {
                                prev.last_seen < cutoff || prev.node != node || prev.resources != msg.resources
                            }
                            None => true,
                        };
                        guard.insert(
                            node.instance.clone(),
                            SeenNode {
                                node,
                                resources: msg.resources,
                                addr: src.ip().to_string(),
                                last_seen: now_secs(),
                            },
                        );
                        changed
                    };
                    if changed {
                        self.bump();
                    }
                }
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Socket plumbing.
// ---------------------------------------------------------------------------

fn bind_reuse(port: u16) -> io::Result<std::net::UdpSocket> {
    unsafe {
        let fd: RawFd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // Must be set before bind. Lets several listeners share the port on one
        // host (a test, a sidecar, a beacon v1 responder in the same netns).
        let one: libc::c_int = 1;
        for opt in [libc::SO_REUSEADDR, libc::SO_REUSEPORT] {
            libc::setsockopt(
                fd,
                libc::SOL_SOCKET,
                opt,
                &one as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
        let addr = libc::sockaddr_in {
            sin_family: libc::AF_INET as libc::sa_family_t,
            sin_port: port.to_be(),
            sin_addr: libc::in_addr { s_addr: libc::INADDR_ANY.to_be() },
            sin_zero: [0; 8],
        };
        if libc::bind(
            fd,
            &addr as *const _ as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        ) < 0
        {
            let err = io::Error::last_os_error();
            libc::close(fd);
            return Err(err);
        }
        Ok(std::net::UdpSocket::from_raw_fd(fd))
    }
}

fn set_multicast_ttl(sock: &std::net::UdpSocket, ttl: libc::c_int) -> io::Result<()> {
    // TTL 1: link-local only, never routed off the segment.
    let rc = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_TTL,
            &ttl as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `ip_mreqn` — `imr_ifindex` is what makes this a per-interface join instead
/// of "whatever the routing table picks".
fn mreqn(group: Ipv4Addr, ifindex: u32) -> libc::ip_mreqn {
    libc::ip_mreqn {
        imr_multiaddr: libc::in_addr { s_addr: u32::from(group).to_be() },
        imr_address: libc::in_addr { s_addr: libc::INADDR_ANY.to_be() },
        imr_ifindex: ifindex as libc::c_int,
    }
}

fn join_group(sock: &std::net::UdpSocket, group: Ipv4Addr, ifindex: u32) -> io::Result<()> {
    let req = mreqn(group, ifindex);
    let rc = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_ADD_MEMBERSHIP,
            &req as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::ip_mreqn>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_multicast_if(sock: &UdpSocket, ifindex: u32) -> io::Result<()> {
    let req = mreqn(Ipv4Addr::UNSPECIFIED, ifindex);
    let rc = unsafe {
        libc::setsockopt(
            sock.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_IF,
            &req as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::ip_mreqn>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Every interface except loopback, as (name, ifindex).
fn eligible_interfaces() -> Vec<(String, u32)> {
    let mut out = Vec::new();
    unsafe {
        let list = libc::if_nameindex();
        if list.is_null() {
            warn!("beacon: could not enumerate interfaces");
            return out;
        }
        let mut p = list;
        while (*p).if_index != 0 && !(*p).if_name.is_null() {
            let name = std::ffi::CStr::from_ptr((*p).if_name).to_string_lossy().into_owned();
            if name != "lo" {
                out.push((name, (*p).if_index));
            }
            p = p.add(1);
        }
        libc::if_freenameindex(list);
    }
    out
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// `$HOSTNAME`, else `/etc/hostname`, else `unknown`.
pub fn hostname() -> String {
    env_nonempty("HOSTNAME")
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| "unknown".into())
}

pub(crate) fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `ENABLE_UDP_DISCOVERY`, permissive: only `false`/`0`/`no`/`off` disable.
pub fn enabled_from_env() -> bool {
    match std::env::var("ENABLE_UDP_DISCOVERY") {
        Ok(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "false" | "0" | "no" | "off"),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two nodes on loopback-only hosts can't multicast to each other, so
    /// exercise the receive path directly by unicasting into a node's port.
    async fn node_on(port: u16, instance: &str, resources: Vec<Resource>) -> BeaconNode {
        let mut cfg = Config::new("test").instance(instance);
        cfg.port = port;
        cfg.interval = Duration::from_millis(200);
        cfg.resources = resources;
        BeaconNode::spawn(cfg).unwrap()
    }

    async fn send(port: u16, msg: &Message) {
        let s = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        s.send_to(&serde_json::to_vec(msg).unwrap(), ("127.0.0.1", port)).await.unwrap();
    }

    fn free_port() -> u16 {
        std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    fn info(instance: &str) -> NodeInfo {
        NodeInfo { name: "peer".into(), instance: instance.into(), ..Default::default() }
    }

    #[tokio::test]
    async fn advertise_bye_and_expiry() {
        let port = free_port();
        let node = node_on(port, "me", vec![]).await;
        let mut rx = node.subscribe();

        let r = Resource::new("nzb", ["metamesh.transport/nzb@1"]);
        send(port, &Message::advertise(info("peer-1"), vec![r])).await;
        tokio::time::timeout(Duration::from_secs(2), rx.changed()).await.unwrap().unwrap();
        assert_eq!(node.resources("metamesh.transport/*").len(), 1);
        assert!(node.resources("metamesh.core").is_empty());

        send(port, &Message::bye(info("peer-1"))).await;
        tokio::time::timeout(Duration::from_secs(2), rx.changed()).await.unwrap().unwrap();
        assert!(node.nodes().is_empty());

        send(port, &Message::advertise(info("peer-2"), vec![])).await;
        tokio::time::timeout(Duration::from_secs(2), rx.changed()).await.unwrap().unwrap();
        assert_eq!(node.nodes().len(), 1);
        // 3 × 200 ms liveness, then the prune on the next tick notifies.
        tokio::time::timeout(Duration::from_secs(3), rx.changed()).await.unwrap().unwrap();
        assert!(node.nodes().is_empty());
    }

    #[tokio::test]
    async fn own_echo_and_v1_ignored() {
        let port = free_port();
        let node = node_on(port, "me", vec![]).await;
        send(port, &Message::advertise(info("me"), vec![])).await;
        let s = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        s.send_to(br#"{"type":"announce","name":"x","tools":[]}"#, ("127.0.0.1", port)).await.unwrap();
        s.send_to(br#"{"v":1,"type":"announce","name":"meta-sort","instance":"a"}"#, ("127.0.0.1", port))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(node.nodes().is_empty());
    }

    #[tokio::test]
    async fn probe_reply_honours_want() {
        let port = free_port();
        let _node = node_on(port, "me", vec![Resource::new("service", ["metamesh.service/meta-sort"])]).await;
        let s = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut buf = vec![0u8; READ_BUFFER];

        let miss = Message::probe("cli", &["metamesh.core".to_string()]);
        s.send_to(&serde_json::to_vec(&miss).unwrap(), ("127.0.0.1", port)).await.unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(300), s.recv_from(&mut buf)).await.is_err());

        let hit = Message::probe("cli", &["metamesh.service/*".to_string()]);
        s.send_to(&serde_json::to_vec(&hit).unwrap(), ("127.0.0.1", port)).await.unwrap();
        let (n, _) = tokio::time::timeout(Duration::from_secs(2), s.recv_from(&mut buf)).await.unwrap().unwrap();
        let m = Message::parse(&buf[..n]).unwrap();
        assert_eq!(m.msg_type, TYPE_ADVERTISE);
        assert_eq!(m.node.unwrap().instance, "me");
    }
}
