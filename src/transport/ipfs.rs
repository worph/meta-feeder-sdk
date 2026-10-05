//! IPFS-specific half of the contract: the ipfs plugin's extra routes.
//!
//! The ipfs plugin owns the libp2p swarm (identify / kad / mdns / bitswap), the
//! blockstore (`ipfs/blocks.redb`: material blocks + leaf refs into files on the
//! shared volume), bitswap ingress and the peer directory. These routes are the
//! hull's facade onto that — each mirrors one blockstore / swarm call the hull
//! made in-process, so the hull's logic (seed rows, eviction, meta-core links,
//! probe grading) did not have to change shape.
//!
//! Public passthrough (the hull relays meta-share's own route verbatim):
//!
//! - `GET /ipfs/:cid` — the read-only IPFS gateway.
//!
//! Internal (all under `/ipfs-tier/`):
//!
//! | route | in-process equivalent |
//! |---|---|
//! | `GET  peers` → [`PeersInfo`] | `swarm::Command::Peers` |
//! | `GET  directory` → [`Directory`] | `swarm::PeerDirectory` (gateway discovery) |
//! | `GET  resolve/:cid?hint=` → bytes | a [pointer](super::pointer)'s bytes: the stored copy `hint` names, else a gateway redeem (see below) |
//! | `GET  local/:cid` (Range) → bytes / `404` | `files::bitswap::try_local_block` |
//! | `GET  has/:cid` → [`Present`] | `block_store.get(cid).is_some()` |
//! | `GET  complete/:cid` → [`Complete`] | `local_dag_complete` |
//! | `GET  cat/:cid?max=` → bytes | `ipfs_walker::get_block` (+ dag-pb assemble) |
//! | `GET  fetch/:cid?timeout_ms=` → block bytes | `block_store.get` → `bitswap_get_block` |
//! | `PUT  block/:cid` ← bytes | `blockstore::put_ipfs_block` |
//! | `POST import` ← file bytes → [`Imported`] | `put_ipfs_blocks(compute_ipfs_blocks(..))` |
//! | `POST forget/:cid` | `api::seeds::remove_ipfs_blocks` |
//! | `POST forget-library` ← [`ForgetLibrary`] | `block_store.remove_library_dag` |
//! | `POST provide/:cid`, `POST unprovide/:cid` | `Command::Provide` / `StopProviding` |
//! | `POST drop-refs` ← [`DropRefs`] | `block_store.drop_refs_for_materials` |
//! | `GET  first-leaf/:cid` → [`FirstLeaf`] | `block_store.first_leaf_backing` |
//! | `GET  rel-path/:midhash` → [`RelPath`] | `resolver().rel_path_for` |
//! | `POST share/library` ← [`ShareLibrary`] → [`Shared`] | `ipfs_seed::seed_file_as_refs` |
//! | `POST share/container` ← [`ShareContainer`] → [`Shared`] | the seal / rebuild re-chunk |
//! | `GET  stats/blockstore` | `/api/stats/blockstore` body |
//! | `GET  debug/:cid` | `/api/debug/blockstore/:cid` body |
//!
//! Plugin → hull, over [`super::hull`]: [`Event::Seed`](super::Event::Seed)
//! (a bitswap fetch now seeds), [`Event::Promoted`](super::Event::Promoted) (an
//! ingress job was renamed into `cache/`), and `GET /internal/records/:cid` for
//! a display name.

//! **Resolving a pointer** (`GET resolve/:cid`) is the one route that reaches
//! outside the swarm: a pointer is answered only by a gateway holding the key,
//! over plain HTTP at the gateway's base URL. Gateways come from config (the
//! local one, pinned) and from the swarm directory; the fastest one whose redeem
//! claim covers the pointer is asked first. `2xx` carries the bytes plus
//! [`HDR_MANIFEST_SOURCE`](super::hull::HDR_MANIFEST_SOURCE) (`store` | `redeem`)
//! and [`HDR_MANIFEST_CID`](super::hull::HDR_MANIFEST_CID); a failure carries
//! [`HDR_RESOLVE_ERROR`](super::dto::HDR_RESOLVE_ERROR).

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct PeersInfo {
    pub local_peer_id: String,
    pub connected: Vec<String>,
}

/// One peer of the directory, with its freshness as *ages* (the hull rebuilds
/// its own `Instant`s from them).
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct DirectoryPeer {
    pub peer_id: String,
    #[serde(default)]
    pub base_url: Option<String>,
    /// Milliseconds since the gateway last advertised `nzbFetch`; `None` = not.
    #[serde(default)]
    pub nzb_fetch_age_ms: Option<u64>,
    /// Milliseconds since the caps were fetched; `None` = never.
    #[serde(default)]
    pub caps_age_ms: Option<u64>,
    /// The gateway's redeem claims, as the gateway serves them
    /// (`{codec, field, hosts, sources}`).
    #[serde(default)]
    pub redeems: Vec<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Directory {
    pub peers: Vec<DirectoryPeer>,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Present {
    pub present: bool,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Complete {
    pub complete: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Imported {
    pub root: String,
    pub size: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ForgetLibrary {
    pub root: String,
    pub midhash: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct DropRefs {
    /// `(container, rel)` pairs.
    pub materials: Vec<(String, String)>,
}

/// Where a dag's first leaf points (the filestore's `LeafBacking`).
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LeafBacking {
    /// A ref into a file meta-core owns, by midhash256.
    MetaCore(String),
    /// A ref into a file in a cache container.
    Cache,
    /// The leaf's bytes are stored material (swarm-fetched, no file behind it).
    Material,
    /// The root or a node on the way down is absent.
    Unknown,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct FirstLeaf {
    pub backing: LeafBacking,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct RelPath {
    pub rel: Option<String>,
}

/// Seed a meta-core library file as refs `(midhash, offset, len)`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ShareLibrary {
    /// Path relative to the files volume (what meta-core's WebDAV serves).
    pub rel: String,
    pub midhash: String,
}

/// Seed a file in a cache container as refs `(container, rel, offset, len)`.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct ShareContainer {
    pub container: String,
    pub rel: String,
    /// Absolute path on the shared volume.
    pub path: String,
    /// Verify the resulting dag is fully held before answering (the seal does;
    /// the rebuild doesn't).
    #[serde(default)]
    pub verify_complete: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Shared {
    pub root: String,
    pub size: u64,
    /// Leaf refs written.
    pub refs: u64,
    /// `verify_complete` was asked and the dag is fully held locally.
    #[serde(default)]
    pub complete: bool,
}
