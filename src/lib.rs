//! `meta-feeder-sdk` — the source-agnostic foundation shared by the gateway
//! core and every feeder sidecar binary.
//!
//! A *feeder* implements [`FeederPlugin`] (find records, fetch bytes) for one
//! or more upstreams, and [`serve_feeders`] exposes them over the feeder HTTP
//! contract. The gateway core consumes that contract via its
//! `RemoteFeederPlugin` and keeps the libp2p wire, the bitswap blockstore, the
//! hashing-into-the-blockstore, and the meta-core store-back to itself.
//!
//! Deliberately libp2p-free and blockstore-free — see this crate's `Cargo.toml`.
//!
//! With the `transport` feature it also carries the **transport-plugin**
//! contract ([`transport`]): the same split, applied to meta-share — a
//! protocol-agnostic hull and one sidecar per byte-moving protocol.
//!
//! [`beacon`] is beacon v2 local discovery. Both harnesses advertise through it;
//! with `default-features = false, features = ["beacon"]` it is all a meta-*
//! service pulls in.

// `redb::Error` is a large enum; the cache wrappers (`cache.rs`) return it
// directly rather than boxing on every embedded-DB call. Boxing each would be
// pure noise for a single-process store, so silence the lint crate-wide.
#![allow(clippy::result_large_err)]

/// pcs-convention default meta-core root a self-publishing feeder targets when
/// neither its dashboard config nor the `META_CORE_URL` env seed sets one. Keeps
/// the modular storage seam out of compose: a standard install just works; a
/// non-standard meta-core is set in the feeder's config UI. Matches the
/// `metacore-app` service the AppStore feeder apps ship on the shared network.
pub const DEFAULT_META_CORE_URL: &str = "http://metacore-app:9000";

#[cfg(feature = "beacon")]
pub mod beacon;

#[cfg(feature = "feeder")]
pub mod budget;
#[cfg(feature = "feeder")]
pub mod cache;
#[cfg(feature = "feeder")]
pub mod common;
#[cfg(feature = "feeder")]
pub mod config;
#[cfg(feature = "feeder")]
pub mod domain;
#[cfg(feature = "feeder")]
pub mod enrich;
#[cfg(feature = "filename")]
pub mod filename_meta;
#[cfg(feature = "feeder")]
pub mod hash;
#[cfg(feature = "feeder")]
pub mod lang;
#[cfg(feature = "feeder")]
pub mod licence;
#[cfg(feature = "feeder")]
pub mod meta_core;
#[cfg(feature = "feeder")]
pub mod plugin;
#[cfg(feature = "feeder")]
pub mod query;
#[cfg(feature = "feeder")]
pub mod query_eval;
#[cfg(feature = "feeder")]
pub mod serve;
#[cfg(feature = "transport")]
pub mod transport;
#[cfg(feature = "feeder")]
pub mod types;

#[cfg(feature = "feeder")]
pub use budget::RateBudget;
#[cfg(feature = "feeder")]
pub use config::{ConfigField, ConfigSchema, FieldKind};
#[cfg(feature = "feeder")]
pub use licence::licence_from_url;
#[cfg(feature = "feeder")]
pub use enrich::{EnrichTarget, EnrichmentConfig, Enricher};
#[cfg(feature = "feeder")]
pub use meta_core::FeederStore;
#[cfg(feature = "feeder")]
pub use plugin::{
    upstream_id_field, ConfigError, FeederPlugin, HashKind, HashOutcome, PluginRegistry,
    RedeemClaim,
};
#[cfg(feature = "feeder")]
pub use query::{GatewayQuery, GatewaySearchEvent, GatewayWireError, Negation, RangeFilter};
#[cfg(feature = "feeder")]
pub use serve::{
    configure_plugins, router, serve_feeders, ComputeRequest, ComputeResponse, HashKindDto,
    HealthResponse, ManifestResponse, OutcomeDto, PluginManifest, QueryRequest, QueryResponse,
    RedeemsResponse,
};
#[cfg(feature = "feeder")]
pub use types::{ByteStream, DiscoveryId, DiscoveryRecord, GatewayError, Hash, PluginHealth};
