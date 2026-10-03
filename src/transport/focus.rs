//! Playback focus, as a transport plugin sees it.
//!
//! meta-share's hull owns the focus *policy*: `POST/DELETE /api/playback/:cid`,
//! the leases, the TTL reaper, which sibling CIDs make up a title. A plugin only
//! needs to *obey* it: pause a torrent that isn't focused, park a background
//! fill, floor a non-focused ingress. So the hull pushes a [`FocusSnapshot`]
//! (`PUT /focus`) on every transition, plus periodically so a restarted plugin
//! recovers, and the plugin answers its gates from a local [`FocusView`].
//!
//! The read side mirrors meta-share's `focus::FocusGate` method for method, so
//! code moved out of meta-share keeps its call sites. Two things are
//! deliberately different from the in-process gate:
//!
//! - **No leases.** A view never expires an entry on its own; only the next
//!   snapshot changes it. The hull's reaper is the single source of truth.
//! - **Per-process buckets.** `throttle_in`/`throttle_out` floor non-focused
//!   traffic at `bg_rate_bytes` *per process*, where the monolith shared one
//!   bucket across tiers. HTTP response bodies are still metered once, by the
//!   hull, when it relays a plugin's `/raw` — plugins must not meter their own
//!   `/raw` egress, or a peer pulling through the hull pays twice.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::watch;

/// How often a parked background task re-checks whether the focus cleared.
/// Same value as meta-share's `focus::PARK_POLL`.
pub const PARK_POLL: Duration = Duration::from_millis(750);

/// Burst allowance as a multiple of the per-second rate, floored at
/// [`MIN_BURST_BYTES`]. Same values as meta-share's `focus.rs`.
const BURST_SECS: u64 = 2;
const MIN_BURST_BYTES: u64 = 512 * 1024;

/// Which bandwidth lane a byte flow belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lane {
    /// Belongs to a title a viewer is playing right now — never gated.
    Focused,
    /// Everything else. Parked or throttled while any title is focused;
    /// unrestricted otherwise.
    Background,
}

impl Lane {
    pub fn is_focused(self) -> bool {
        matches!(self, Lane::Focused)
    }

    /// Wire value of the `X-MetaMesh-Lane` header.
    pub fn as_header(self) -> &'static str {
        match self {
            Lane::Focused => "focused",
            Lane::Background => "background",
        }
    }

    /// Parse the `X-MetaMesh-Lane` header. An absent or unknown value is
    /// `Focused`: a request the hull didn't classify must never be starved.
    pub fn from_header(v: Option<&str>) -> Lane {
        match v {
            Some("background") => Lane::Background,
            _ => Lane::Focused,
        }
    }
}

/// One focused title: its group key plus every identity it is reachable under.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusTitle {
    /// The record's canonical cid — the stable per-title key.
    pub group: String,
    /// Every sibling cid of the title.
    #[serde(default)]
    pub cids: Vec<String>,
    /// The torrent infohashes among those siblings, lowercase hex, pre-decoded
    /// by the hull so a torrent plugin never re-parses cids on a transition.
    #[serde(default)]
    pub infohashes: Vec<String>,
}

/// The whole focus state, pushed by the hull on `PUT /focus`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusSnapshot {
    /// Master switch (`META_SHARE_FOCUS_ENABLED`). Off → every gate is a no-op.
    pub enabled: bool,
    /// Bytes/s a non-focused flow is floored at. `0` → hard park.
    pub bg_rate_bytes: u64,
    #[serde(default)]
    pub titles: Vec<FocusTitle>,
}

impl Default for FocusSnapshot {
    fn default() -> Self {
        Self {
            enabled: true,
            bg_rate_bytes: 10 * 1024,
            titles: Vec::new(),
        }
    }
}

struct Inner {
    titles: Vec<FocusTitle>,
    cids: HashSet<String>,
    infohashes: HashSet<[u8; 20]>,
}

/// A plugin's local replica of the hull's focus gate.
pub struct FocusView {
    inner: Mutex<Inner>,
    /// `titles.len()` as an atomic, so the resting state is one relaxed load.
    n: AtomicUsize,
    enabled: std::sync::atomic::AtomicBool,
    rate: AtomicU64,
    out_bucket: TokenBucket,
    in_bucket: TokenBucket,
    /// Bumped on every applied snapshot that changed something. A torrent
    /// plugin watches it to re-run its pause/unpause pass.
    changed: watch::Sender<u64>,
}

impl FocusView {
    pub fn new() -> Arc<Self> {
        let d = FocusSnapshot::default();
        let (changed, _) = watch::channel(0);
        let burst = (d.bg_rate_bytes * BURST_SECS).max(MIN_BURST_BYTES);
        Arc::new(Self {
            inner: Mutex::new(Inner {
                titles: Vec::new(),
                cids: HashSet::new(),
                infohashes: HashSet::new(),
            }),
            n: AtomicUsize::new(0),
            enabled: std::sync::atomic::AtomicBool::new(d.enabled),
            rate: AtomicU64::new(d.bg_rate_bytes),
            out_bucket: TokenBucket::new(d.bg_rate_bytes, burst),
            in_bucket: TokenBucket::new(d.bg_rate_bytes, burst),
            changed,
        })
    }

    /// Replace the state with `snap`. Returns `true` when anything changed (and
    /// notifies [`subscribe`](Self::subscribe)rs).
    pub fn apply(&self, snap: FocusSnapshot) -> bool {
        let mut cids = HashSet::new();
        let mut infohashes = HashSet::new();
        for t in &snap.titles {
            cids.extend(t.cids.iter().cloned());
            cids.insert(t.group.clone());
            infohashes.extend(t.infohashes.iter().filter_map(|h| decode_hex20(h)));
        }
        let changed = {
            let mut inner = self.inner.lock().unwrap();
            let changed = inner.titles != snap.titles
                || self.enabled.load(Ordering::Relaxed) != snap.enabled
                || self.rate.load(Ordering::Relaxed) != snap.bg_rate_bytes;
            inner.titles = snap.titles;
            inner.cids = cids;
            inner.infohashes = infohashes;
            self.n.store(inner.titles.len(), Ordering::Relaxed);
            changed
        };
        self.enabled.store(snap.enabled, Ordering::Relaxed);
        if self.rate.swap(snap.bg_rate_bytes, Ordering::Relaxed) != snap.bg_rate_bytes {
            let burst = (snap.bg_rate_bytes * BURST_SECS).max(MIN_BURST_BYTES);
            self.out_bucket.reconfigure(snap.bg_rate_bytes, burst);
            self.in_bucket.reconfigure(snap.bg_rate_bytes, burst);
        }
        if changed {
            self.changed.send_modify(|g| *g += 1);
        }
        changed
    }

    /// The current state, as the hull last pushed it.
    pub fn snapshot(&self) -> FocusSnapshot {
        FocusSnapshot {
            enabled: self.enabled.load(Ordering::Relaxed),
            bg_rate_bytes: self.rate.load(Ordering::Relaxed),
            titles: self.inner.lock().unwrap().titles.clone(),
        }
    }

    /// Fires after every snapshot that changed something.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.changed.subscribe()
    }

    pub fn bg_rate_bytes(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// Is *any* title focused?
    pub fn any(&self) -> bool {
        self.enabled.load(Ordering::Relaxed) && self.n.load(Ordering::Relaxed) > 0
    }

    pub fn count(&self) -> usize {
        self.n.load(Ordering::Relaxed)
    }

    /// Which lane does a byte flow for `cid` belong to? Same rule as the hull:
    /// nothing focused → everything is `Focused`.
    pub fn lane(&self, cid: &str) -> Lane {
        if !self.any() {
            return Lane::Focused;
        }
        if self.inner.lock().unwrap().cids.contains(cid) {
            Lane::Focused
        } else {
            Lane::Background
        }
    }

    pub fn is_cid_focused(&self, cid: &str) -> bool {
        self.inner.lock().unwrap().cids.contains(cid)
    }

    pub fn focused_infohashes(&self) -> HashSet<[u8; 20]> {
        self.inner.lock().unwrap().infohashes.clone()
    }

    pub fn is_infohash_focused(&self, infohash: &[u8; 20]) -> bool {
        self.inner.lock().unwrap().infohashes.contains(infohash)
    }

    /// Park a background task while any title is focused.
    pub async fn wait_turn(&self, lane: Lane) {
        if lane.is_focused() {
            return;
        }
        while self.any() {
            tokio::time::sleep(PARK_POLL).await;
        }
    }

    /// [`wait_turn`](Self::wait_turn) with a deadline; `false` if the focus
    /// outlasted `budget`.
    pub async fn wait_turn_timeout(&self, lane: Lane, budget: Duration) -> bool {
        if lane.is_focused() {
            return true;
        }
        let deadline = Instant::now() + budget;
        while self.any() {
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(PARK_POLL).await;
        }
        true
    }

    /// Floor a non-focused egress of `n` bytes (bitswap block service).
    pub async fn throttle_out(&self, lane: Lane, n: u64) {
        if lane.is_focused() || !self.any() {
            return;
        }
        if self.bg_rate_bytes() == 0 {
            self.wait_turn(lane).await;
            return;
        }
        self.out_bucket.consume(n).await;
    }

    /// Floor a non-focused ingress of `n` bytes that a caller is waiting on.
    pub async fn throttle_in(&self, lane: Lane, n: u64) {
        if lane.is_focused() || !self.any() {
            return;
        }
        if self.bg_rate_bytes() == 0 {
            self.wait_turn(lane).await;
            return;
        }
        self.in_bucket.consume(n).await;
    }
}

/// Meter a byte stream against a view's egress floor. Same shape as
/// meta-share's `focus::gated_stream`.
pub fn gated_stream<S, B, E>(
    stream: S,
    focus: Arc<FocusView>,
    lane: Lane,
) -> impl futures::Stream<Item = Result<B, E>>
where
    S: futures::Stream<Item = Result<B, E>>,
    B: AsRef<[u8]>,
{
    use futures::StreamExt;
    stream.then(move |item| {
        let focus = Arc::clone(&focus);
        async move {
            if let Ok(bytes) = &item {
                focus.throttle_out(lane, bytes.as_ref().len() as u64).await;
            }
            item
        }
    })
}

/// Lowercase hex of a 20-byte infohash — the wire form in [`FocusTitle`].
pub fn infohash_hex(ih: &[u8; 20]) -> String {
    let mut s = String::with_capacity(40);
    for b in ih {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

fn decode_hex20(s: &str) -> Option<[u8; 20]> {
    if s.len() != 40 {
        return None;
    }
    let mut out = [0u8; 20];
    for (i, pair) in s.as_bytes().chunks(2).enumerate() {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// Leaky bucket that always waits (never errors on `n > burst`). Copied from
/// meta-share's `focus.rs`, plus `reconfigure` for a rate pushed at runtime.
struct TokenBucket {
    state: Mutex<BucketState>,
}

struct BucketState {
    tokens: f64,
    last: Instant,
    rate: u64,
    burst: u64,
}

impl TokenBucket {
    fn new(rate: u64, burst: u64) -> Self {
        Self {
            state: Mutex::new(BucketState {
                tokens: burst as f64,
                last: Instant::now(),
                rate,
                burst,
            }),
        }
    }

    fn reconfigure(&self, rate: u64, burst: u64) {
        let mut s = self.state.lock().unwrap();
        s.rate = rate;
        s.burst = burst;
        s.tokens = s.tokens.min(burst as f64);
    }

    async fn consume(&self, n: u64) {
        let wait = {
            let mut s = self.state.lock().unwrap();
            if s.rate == 0 || n == 0 {
                return;
            }
            let now = Instant::now();
            let elapsed = now.duration_since(s.last).as_secs_f64();
            s.last = now;
            s.tokens = (s.tokens + elapsed * s.rate as f64).min(s.burst as f64);
            s.tokens -= n as f64;
            if s.tokens >= 0.0 {
                return;
            }
            Duration::from_secs_f64(-s.tokens / s.rate as f64)
        };
        tokio::time::sleep(wait).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(titles: Vec<FocusTitle>) -> FocusSnapshot {
        FocusSnapshot {
            titles,
            ..Default::default()
        }
    }

    #[test]
    fn nothing_focused_means_every_flow_is_focused() {
        let v = FocusView::new();
        assert!(!v.any());
        assert_eq!(v.lane("bafyanything"), Lane::Focused);
    }

    #[test]
    fn siblings_and_group_are_focused_others_background() {
        let v = FocusView::new();
        v.apply(snap(vec![FocusTitle {
            group: "g".into(),
            cids: vec!["a".into(), "b".into()],
            infohashes: vec![infohash_hex(&[7u8; 20])],
        }]));
        assert!(v.any());
        assert_eq!(v.lane("a"), Lane::Focused);
        assert_eq!(v.lane("g"), Lane::Focused);
        assert_eq!(v.lane("z"), Lane::Background);
        assert!(v.is_infohash_focused(&[7u8; 20]));
        assert!(!v.is_infohash_focused(&[8u8; 20]));
    }

    #[test]
    fn disabled_gate_focuses_nothing() {
        let v = FocusView::new();
        v.apply(FocusSnapshot {
            enabled: false,
            bg_rate_bytes: 1,
            titles: vec![FocusTitle {
                group: "g".into(),
                ..Default::default()
            }],
        });
        assert!(!v.any());
        assert_eq!(v.lane("z"), Lane::Focused);
    }

    #[test]
    fn apply_reports_change_only_on_change() {
        let v = FocusView::new();
        let rx = v.subscribe();
        let s = snap(vec![FocusTitle {
            group: "g".into(),
            ..Default::default()
        }]);
        assert!(v.apply(s.clone()));
        assert!(!v.apply(s));
        assert_eq!(*rx.borrow(), 1);
        assert!(v.apply(snap(vec![])));
        assert_eq!(*rx.borrow(), 2);
    }

    #[test]
    fn lane_header_round_trip_defaults_to_focused() {
        assert_eq!(Lane::from_header(Some("background")), Lane::Background);
        assert_eq!(Lane::from_header(Some("focused")), Lane::Focused);
        assert_eq!(Lane::from_header(None), Lane::Focused);
        assert_eq!(Lane::from_header(Some("junk")), Lane::Focused);
        assert_eq!(Lane::from_header(Some(Lane::Background.as_header())), Lane::Background);
    }

    #[test]
    fn snapshot_json_round_trips() {
        let s = snap(vec![FocusTitle {
            group: "g".into(),
            cids: vec!["a".into()],
            infohashes: vec![infohash_hex(&[1u8; 20])],
        }]);
        let back: FocusSnapshot = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }
}
