//! Conformance checks any transport plugin must pass — ours or a competing
//! implementation (a qBittorrent-backed torrent plugin, say). Run them against a
//! live plugin URL, or in a unit test via [`spawn_local`].
//!
//! The checks cover the *common* contract only. A plugin's byte behaviour is
//! verified by its own tests and by meta-share's contract suite against the
//! hull, which is where "identical to the monolith" is decided.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use super::client::RemoteTransport;
use super::dto::{Deleted, CONTRACT_VERSION};
use super::focus::{FocusSnapshot, FocusTitle};
use super::plugin::TransportPlugin;

/// A well-formed cid no plugin holds a job for: the raw/sha2-256 CID of the
/// empty block.
pub const UNKNOWN_CID: &str = "bafkreihdwdcefgh4dqkjv67uzcmw7ojee6xedzdetojuzjevtenxquvyku";

/// Serve `plugin` on an ephemeral localhost port. Returns its address; the
/// server lives until the runtime shuts down.
pub async fn spawn_local<P: TransportPlugin>(plugin: Arc<P>) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = super::serve::router(plugin);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

/// Run every check against the plugin at `base`. `Ok(())` or the list of
/// failures, each a one-line explanation.
pub async fn conformance(base: &str) -> Result<(), Vec<String>> {
    let http = reqwest::Client::new();
    let t = RemoteTransport::new("under-test", base, http.clone())
        .with_control_timeout(Duration::from_secs(10));
    let mut fails = Vec::new();

    match t.manifest().await {
        Ok(m) => {
            if m.id.is_empty() {
                fails.push("manifest.id is empty".into());
            }
            if m.contract != CONTRACT_VERSION {
                fails.push(format!("manifest.contract {} != {CONTRACT_VERSION}", m.contract));
            }
            if !m.capabilities.fetch {
                fails.push("manifest.capabilities.fetch is false — a transport must fetch".into());
            }
        }
        Err(e) => fails.push(format!("GET /manifest: {e}")),
    }

    match http.get(t.url("/health")).send().await {
        Ok(r) if r.status().is_success() || r.status().as_u16() == 503 => {
            if r.json::<super::dto::Health>().await.is_err() {
                fails.push("GET /health body is not a Health".into());
            }
        }
        Ok(r) => fails.push(format!("GET /health: status {}", r.status())),
        Err(e) => fails.push(format!("GET /health: {e}")),
    }

    if let Err(e) = t.jobs().await {
        fails.push(format!("GET /jobs: {e}"));
    }

    let snap = FocusSnapshot {
        enabled: true,
        bg_rate_bytes: 4321,
        titles: vec![FocusTitle {
            group: "conformance-group".into(),
            cids: vec!["conformance-cid".into()],
            infohashes: vec![],
        }],
    };
    match t.push_focus(&snap).await {
        Ok(()) => match t.get_json::<FocusSnapshot>("/focus").await {
            Ok(back) if back == snap => {}
            Ok(back) => fails.push(format!("GET /focus returned {back:?}, pushed {snap:?}")),
            Err(e) => fails.push(format!("GET /focus: {e}")),
        },
        Err(e) => fails.push(format!("PUT /focus: {e}")),
    }
    // Leave the plugin unfocused.
    let _ = t.push_focus(&FocusSnapshot::default()).await;

    match http.delete(t.url(&format!("/jobs/{UNKNOWN_CID}"))).send().await {
        Ok(r) if r.status().as_u16() == 404 => {}
        Ok(r) if r.status().is_success() => match r.json::<Deleted>().await {
            Ok(d) if d.bytes_freed == 0 => {}
            Ok(d) => fails.push(format!("DELETE unknown job freed {} bytes", d.bytes_freed)),
            Err(e) => fails.push(format!("DELETE unknown job: body: {e}")),
        },
        Ok(r) => fails.push(format!("DELETE unknown job: status {}", r.status())),
        Err(e) => fails.push(format!("DELETE unknown job: {e}")),
    }

    match http.get(t.url("/raw/not-a-cid")).send().await {
        Ok(r) if r.status().is_client_error() => {
            let v: Option<serde_json::Value> = r.json().await.ok();
            if !v.as_ref().is_some_and(|v| v.get("error").is_some()) {
                fails.push("GET /raw/not-a-cid: 4xx without an {\"error\"} body".into());
            }
        }
        Ok(r) => fails.push(format!("GET /raw/not-a-cid: expected 4xx, got {}", r.status())),
        Err(e) => fails.push(format!("GET /raw/not-a-cid: {e}")),
    }

    if fails.is_empty() {
        Ok(())
    } else {
        Err(fails)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::dto::{Capabilities, Job, Manifest, ReconcileReport, ReconcileRequest};
    use crate::transport::error::ApiError;
    use crate::transport::focus::FocusView;
    use async_trait::async_trait;
    use axum::http::HeaderMap;
    use axum::response::{IntoResponse, Response};

    /// The smallest plugin that should pass: holds nothing, fetches nothing.
    struct Empty {
        focus: Arc<FocusView>,
    }

    #[async_trait]
    impl TransportPlugin for Empty {
        fn manifest(&self) -> Manifest {
            Manifest {
                id: "empty".into(),
                implementation: "testkit-stub".into(),
                version: "0".into(),
                contract: CONTRACT_VERSION,
                capabilities: Capabilities { fetch: true, share: false },
            }
        }
        async fn raw(&self, _cid: String, _h: HeaderMap) -> Response {
            ApiError::BadRequest("unsupported cid".into()).into_response()
        }
        async fn jobs(&self) -> Result<Vec<Job>, ApiError> {
            Ok(vec![])
        }
        async fn delete(&self, _cid: String, _keep: Vec<String>) -> Result<Deleted, ApiError> {
            Ok(Deleted::default())
        }
        async fn reconcile(&self, _r: ReconcileRequest) -> Result<ReconcileReport, ApiError> {
            Ok(ReconcileReport::default())
        }
        fn focus(&self) -> &Arc<FocusView> {
            &self.focus
        }
    }

    #[tokio::test]
    async fn the_empty_plugin_conforms() {
        let addr = spawn_local(Arc::new(Empty { focus: FocusView::new() })).await;
        conformance(&format!("http://{addr}")).await.unwrap();
    }
}
