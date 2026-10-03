//! The plugin side of the contract: what a transport implementation provides.

use std::sync::Arc;

use async_trait::async_trait;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Router;

use super::dto::{Deleted, Health, Job, Manifest, ReconcileReport, ReconcileRequest};
use super::error::ApiError;
use super::focus::FocusView;

/// One transport implementation (BitTorrent, Usenet, IPFS, or a competing
/// implementation of one of them). [`serve_transport`](super::serve::serve_transport)
/// wraps it in the common routes; [`extra_routes`](Self::extra_routes) adds its
/// protocol-specific ones.
#[async_trait]
pub trait TransportPlugin: Send + Sync + 'static {
    fn manifest(&self) -> Manifest;

    async fn health(&self) -> Health {
        Health::ok()
    }

    /// `GET /raw/:cid`. Must answer exactly what the in-process tier answered
    /// in the monolith (status, `Content-*`, `Retry-After`, error body) — the
    /// hull relays it to meta-share's public API untouched. Must **not** meter
    /// its own egress: the hull does, on relay.
    async fn raw(&self, cid: String, headers: HeaderMap) -> Response;

    /// `GET /jobs`.
    async fn jobs(&self) -> Result<Vec<Job>, ApiError>;

    /// `DELETE /jobs/:cid?keep=…`. `keep` lists sibling cids of the same unit
    /// the hull still wants.
    async fn delete(&self, cid: String, keep: Vec<String>) -> Result<Deleted, ApiError>;

    /// `POST /reconcile`.
    async fn reconcile(&self, req: ReconcileRequest) -> Result<ReconcileReport, ApiError>;

    /// The view `PUT /focus` writes into.
    fn focus(&self) -> &Arc<FocusView>;

    /// Protocol-specific routes, merged into the common router. Paths must not
    /// collide with the common ones.
    fn extra_routes(self: Arc<Self>) -> Router {
        Router::new()
    }
}
