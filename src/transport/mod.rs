//! **Transport plugins** — the contract between meta-share's protocol-agnostic
//! hull and the processes that actually move bytes (`meta-transport-torrent`,
//! `meta-transport-nzb`, `meta-transport-ipfs`, or a competing implementation
//! of any of them).
//!
//! Same split as gateway ⇄ feeders: the plugin finds and moves bytes for one
//! protocol; the hull keeps everything that is policy and bookkeeping — the
//! public HTTP API, the material index, seed rows, eviction, focus, meta-core.
//! Plugins never call each other and never talk to meta-core's record API;
//! every cross-tier edge goes through the hull.
//!
//! - [`plugin::TransportPlugin`] + [`serve::serve_transport`]: the plugin side.
//! - [`client::RemoteTransport`]: the hull side.
//! - [`hull::HullClient`]: plugin → hull callbacks (internal port only).
//! - [`config::ConfigPlane`]: the plugin's own settings + the config page the
//!   hull's dashboard embeds.
//! - [`focus::FocusView`]: the plugin's replica of the hull's playback focus.
//! - [`dto`]: common wire types; [`error::ApiError`] / [`range`]: shared so a
//!   relayed plugin response is byte-identical to what the monolith answered.
//! - [`cid`]: CID parsing + cid-shape decoders, shared so every process
//!   classifies a cid the same way.
//! - [`torrent`]: the torrent plugin's extra routes + magnet helpers.
//! - [`ipfs`]: the ipfs plugin's extra routes (the hull's blockstore/swarm facade).
//! - [`nzb`]: Usenet cid shapes, the usenet plugin's extra routes + settings.
//! - [`magic`]: content sniffing; [`file`]: range-serving a file off `/data`.
//! - `testkit` (feature `testkit`): conformance checks for any implementation.
//!
//! Bytes on disk: every plugin mounts the same `/data` volume as the hull, so a
//! plugin-written file is opened by the hull directly (local-first reads) and
//! `rename(2)` between `tmp/` and `cache/` stays atomic.

pub mod cid;
pub mod client;
pub mod config;
pub mod dto;
pub mod error;
pub mod file;
pub mod focus;
pub mod fsutil;
pub mod hull;
pub mod ipfs;
pub mod magic;
pub mod nzb;
pub mod plugin;
pub mod range;
pub mod serve;
pub mod torrent;
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

pub use client::RemoteTransport;
pub use config::ConfigPlane;
pub use dto::{
    Capabilities, Deleted, Event, Health, Job, Jobs, Manifest, PromotedFile, ReconcileReport,
    ReconcileRequest, CONTRACT_VERSION,
};
pub use error::ApiError;
pub use focus::{FocusSnapshot, FocusTitle, FocusView, Lane};
pub use hull::{HullClient, NetworkInfo, RecordInfo};
pub use plugin::TransportPlugin;
pub use serve::serve_transport;
