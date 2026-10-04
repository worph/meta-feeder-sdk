//! `serve_transport` — the axum harness every transport plugin binary runs.
//!
//! # Common routes
//!
//! - `GET    /manifest`        → [`Manifest`](super::dto::Manifest)
//! - `GET    /health`          → [`Health`](super::dto::Health)
//! - `GET    /raw/:cid`        → the tier's byte response, verbatim
//! - `GET    /jobs`            → [`Jobs`](super::dto::Jobs)
//! - `DELETE /jobs/:cid?keep=` → [`Deleted`](super::dto::Deleted)
//! - `POST   /reconcile`       → [`ReconcileReport`](super::dto::ReconcileReport)
//! - `PUT    /focus`           ← [`FocusSnapshot`](super::focus::FocusSnapshot), `204`
//! - `GET    /focus`           → the snapshot the plugin currently obeys
//!
//! plus the `/config*` routes when [`TransportPlugin::config`] is `Some` (see
//! [`ConfigPlane`](super::config::ConfigPlane)), plus whatever
//! [`TransportPlugin::extra_routes`] adds.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use tracing::info;

use super::dto::{DeleteQuery, Jobs, ReconcileRequest};
use super::focus::FocusSnapshot;
use super::plugin::TransportPlugin;

type Shared = Arc<dyn TransportPlugin>;

/// The common router for `plugin`, merged with its extra routes.
pub fn router<P: TransportPlugin>(plugin: Arc<P>) -> Router {
    let extra = Arc::clone(&plugin).extra_routes();
    let config = plugin.config().map(|c| c.routes());
    let shared: Shared = plugin;
    let r = Router::new()
        .route("/manifest", get(manifest))
        .route("/health", get(health))
        .route("/raw/:cid", get(raw))
        .route("/jobs", get(jobs))
        .route("/jobs/:cid", delete(delete_job))
        .route("/reconcile", post(reconcile))
        .route("/focus", get(get_focus).put(put_focus))
        .with_state(shared)
        .merge(extra);
    match config {
        Some(c) => r.merge(c),
        None => r,
    }
}

/// Bind `listen` and serve `plugin` until SIGTERM / Ctrl-C.
pub async fn serve_transport<P: TransportPlugin>(
    plugin: Arc<P>,
    listen: SocketAddr,
) -> anyhow::Result<()> {
    let manifest = plugin.manifest();
    let id = manifest.id.clone();
    let advertised = Arc::clone(&plugin);
    let app = router(plugin);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    info!(%listen, plugin = %id, "transport plugin listening");
    // Beacon v2: advertise once bound; say `bye` on clean shutdown so the hull
    // drops us at once instead of after the liveness window.
    let port = listen.port();
    // `node.name` is a short machine name; the human description stays in the
    // manifest (`implementation`), which a consumer fetches over HTTP.
    let beacon_name = std::env::var("SERVICE_NAME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("meta-transport-{}", manifest.id));
    let beacon = crate::beacon::advertise_plugin(&beacon_name, &manifest.version, move || {
        beacon_resource(advertised.as_ref(), port)
    });
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    if let Some(b) = beacon {
        b.bye().await;
    }
    Ok(())
}

/// The transport's beacon v2 resource: id = manifest id, cap
/// `metamesh.transport/<id>@<contract>`, `rev` = hash of the served manifest.
fn beacon_resource<P: TransportPlugin + ?Sized>(plugin: &P, listen_port: u16) -> crate::beacon::Resource {
    use crate::beacon::{advertise_url, binds_from_env, caps, rev_of, Resource};
    let mut m = plugin.manifest();
    m.config = plugin.config().is_some();
    let mut r = Resource::new(m.id.clone(), [caps::transport(&m.id, m.contract)])
        .with_endpoint("http", advertise_url(listen_port))
        .with_endpoint("manifest", "/manifest");
    r.rev = Some(rev_of(&m));
    r.binds = binds_from_env();
    r
}

/// Resolves on SIGTERM (docker stop) or Ctrl-C.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}

async fn manifest(State(p): State<Shared>) -> Response {
    let mut m = p.manifest();
    m.config = p.config().is_some();
    Json(m).into_response()
}

async fn health(State(p): State<Shared>) -> Response {
    let h = p.health().await;
    let code = if h.ok { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (code, Json(h)).into_response()
}

async fn raw(State(p): State<Shared>, Path(cid): Path<String>, headers: HeaderMap) -> Response {
    p.raw(cid, headers).await
}

async fn jobs(State(p): State<Shared>) -> Response {
    match p.jobs().await {
        Ok(jobs) => Json(Jobs { jobs }).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn delete_job(
    State(p): State<Shared>,
    Path(cid): Path<String>,
    Query(q): Query<DeleteQuery>,
) -> Response {
    match p.delete(cid, q.keep_list()).await {
        Ok(d) => Json(d).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn reconcile(State(p): State<Shared>, Json(req): Json<ReconcileRequest>) -> Response {
    match p.reconcile(req).await {
        Ok(r) => Json(r).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn get_focus(State(p): State<Shared>) -> Response {
    Json(p.focus().snapshot()).into_response()
}

async fn put_focus(State(p): State<Shared>, Json(snap): Json<FocusSnapshot>) -> StatusCode {
    p.focus().apply(snap);
    StatusCode::NO_CONTENT
}
