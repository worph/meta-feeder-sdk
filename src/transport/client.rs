//! The hull side of the contract: an HTTP client for one transport plugin.
//!
//! [`RemoteTransport`] covers the common routes and gives typed helpers for a
//! plugin's extra ones (`get_json`, `post_json`, …). Byte responses come back as
//! an axum [`Response`] whose body streams straight from the plugin, so the hull
//! relays a multi-GB range without buffering it.

use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::de::DeserializeOwned;
use serde::Serialize;

use super::dto::{
    Deleted, Health, Job, Jobs, Manifest, ReconcileReport, ReconcileRequest,
};
use super::error::ApiError;
use super::focus::FocusSnapshot;

/// Request headers never forwarded to a plugin (hop-by-hop, or describing the
/// client's connection to the hull rather than the hull's to the plugin).
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
    "content-length",
];

/// Client for one plugin at `base` (e.g. `http://metashare-torrent:3000`).
#[derive(Clone, Debug)]
pub struct RemoteTransport {
    id: String,
    base: String,
    http: reqwest::Client,
    /// For control calls (jobs, focus, reconcile). `/raw` has no overall
    /// timeout — a body can legitimately stream for the length of a film.
    control_timeout: Duration,
}

impl RemoteTransport {
    /// `id` names the tier in error messages (`torrent`, `nzb`, `ipfs`).
    pub fn new(id: impl Into<String>, base: impl Into<String>, http: reqwest::Client) -> Self {
        Self {
            id: id.into(),
            base: base.into().trim_end_matches('/').to_string(),
            http,
            control_timeout: Duration::from_secs(30),
        }
    }

    pub fn with_control_timeout(mut self, t: Duration) -> Self {
        self.control_timeout = t;
        self
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn unreachable(&self, e: impl std::fmt::Display) -> ApiError {
        ApiError::Upstream(format!("{} plugin unreachable: {e}", self.id))
    }

    // ---- common routes ---------------------------------------------------

    pub async fn manifest(&self) -> Result<Manifest, ApiError> {
        self.get_json("/manifest").await
    }

    pub async fn health(&self) -> Result<Health, ApiError> {
        let r = self
            .http
            .get(self.url("/health"))
            .timeout(self.control_timeout)
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        r.json::<Health>().await.map_err(|e| self.unreachable(e))
    }

    /// `GET /raw/:cid`, forwarding the client's request headers plus `extra`.
    /// Returns the plugin's response as-is (any status), with its body streaming.
    pub async fn raw(
        &self,
        cid: &str,
        client_headers: &HeaderMap,
        extra: &[(&str, String)],
    ) -> Result<Response, ApiError> {
        let mut req = self.http.get(self.url(&format!("/raw/{cid}")));
        req = req.headers(forwardable(client_headers));
        for (k, v) in extra {
            req = req.header(*k, v);
        }
        let resp = req.send().await.map_err(|e| self.unreachable(e))?;
        Ok(relay(resp))
    }

    pub async fn jobs(&self) -> Result<Vec<Job>, ApiError> {
        Ok(self.get_json::<Jobs>("/jobs").await?.jobs)
    }

    pub async fn delete(&self, cid: &str, keep: &[String]) -> Result<Deleted, ApiError> {
        let r = self
            .http
            .delete(self.url(&format!("/jobs/{cid}")))
            .query(&[("keep", keep.join(","))])
            .timeout(self.control_timeout)
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        decode(r).await
    }

    pub async fn reconcile(&self, req: &ReconcileRequest) -> Result<ReconcileReport, ApiError> {
        // A reconcile can delete many units; give it more room than a poll.
        let r = self
            .http
            .post(self.url("/reconcile"))
            .json(req)
            .timeout(self.control_timeout * 10)
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        decode(r).await
    }

    pub async fn push_focus(&self, snap: &FocusSnapshot) -> Result<(), ApiError> {
        let r = self
            .http
            .put(self.url("/focus"))
            .json(snap)
            .timeout(self.control_timeout)
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        if r.status().is_success() {
            Ok(())
        } else {
            Err(ApiError::Upstream(format!("{} plugin /focus: {}", self.id, r.status())))
        }
    }

    /// `PUT /manifests/:cid`: hand a manifest-consuming plugin (contract 2) the
    /// manifest a pointer resolved to, after it answered `428` +
    /// [`super::dto::HDR_NEEDS_MANIFEST`].
    pub async fn push_manifest(&self, cid: &str, manifest: bytes::Bytes) -> Result<(), ApiError> {
        let r = self
            .http
            .put(self.url(&format!("/manifests/{cid}")))
            .body(manifest)
            .timeout(self.control_timeout)
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        if r.status().is_success() {
            Ok(())
        } else {
            Err(error_from(r).await)
        }
    }

    // ---- typed helpers for extra routes ------------------------------------

    pub async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, ApiError> {
        let r = self
            .http
            .get(self.url(path))
            .timeout(self.control_timeout)
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        decode(r).await
    }

    pub async fn post_json<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> Result<T, ApiError> {
        self.post_json_timeout(path, body, self.control_timeout).await
    }

    pub async fn post_json_timeout<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
        timeout: Duration,
    ) -> Result<T, ApiError> {
        let r = self
            .http
            .post(self.url(path))
            .json(body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        decode(r).await
    }

    pub async fn put_json<B: Serialize + ?Sized>(&self, path: &str, body: &B) -> Result<(), ApiError> {
        let r = self
            .http
            .put(self.url(path))
            .json(body)
            .timeout(self.control_timeout)
            .send()
            .await
            .map_err(|e| self.unreachable(e))?;
        if r.status().is_success() {
            Ok(())
        } else {
            Err(error_from(r).await)
        }
    }

    /// Forward an arbitrary request and relay the response verbatim — for the
    /// public routes a plugin now answers (`/ipfs/:cid`, `/api/peers`, …).
    pub async fn forward(
        &self,
        method: Method,
        path_and_query: &str,
        client_headers: &HeaderMap,
        body: Option<bytes::Bytes>,
    ) -> Result<Response, ApiError> {
        let mut req = self
            .http
            .request(method, self.url(path_and_query))
            .headers(forwardable(client_headers));
        if let Some(b) = body {
            req = req.body(b);
        }
        let resp = req.send().await.map_err(|e| self.unreachable(e))?;
        Ok(relay(resp))
    }
}

/// Client request headers worth forwarding to a plugin.
fn forwardable(h: &HeaderMap) -> reqwest::header::HeaderMap {
    let mut out = reqwest::header::HeaderMap::new();
    for (k, v) in h {
        if HOP_BY_HOP.contains(&k.as_str()) {
            continue;
        }
        if let (Ok(k), Ok(v)) = (
            reqwest::header::HeaderName::from_bytes(k.as_str().as_bytes()),
            reqwest::header::HeaderValue::from_bytes(v.as_bytes()),
        ) {
            out.append(k, v);
        }
    }
    out
}

/// A plugin response as an axum response: same status, same headers (minus
/// hop-by-hop), body streamed.
pub fn relay(resp: reqwest::Response) -> Response {
    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut headers = HeaderMap::new();
    for (k, v) in resp.headers() {
        let name = k.as_str();
        if matches!(name, "connection" | "keep-alive" | "transfer-encoding" | "trailer" | "upgrade") {
            continue;
        }
        if let (Ok(k), Ok(v)) = (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_bytes(v.as_bytes()),
        ) {
            headers.append(k, v);
        }
    }
    let body = Body::from_stream(resp.bytes_stream());
    let mut out = (status, body).into_response();
    *out.headers_mut() = headers;
    out
}

async fn decode<T: DeserializeOwned>(r: reqwest::Response) -> Result<T, ApiError> {
    if !r.status().is_success() {
        return Err(error_from(r).await);
    }
    r.json::<T>()
        .await
        .map_err(|e| ApiError::Upstream(format!("plugin returned malformed JSON: {e}")))
}

/// Map a plugin error response back onto [`ApiError`], keeping its message so
/// a relayed error reads the same as the in-process one did.
pub async fn error_from(r: reqwest::Response) -> ApiError {
    let status = r.status().as_u16();
    let msg = r
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_string))
        .unwrap_or_else(|| format!("plugin error {status}"));
    match status {
        400 => ApiError::BadRequest(msg),
        404 => ApiError::NotFound,
        504 => ApiError::GatewayTimeout(msg),
        503 if msg == "swarm task is gone" => ApiError::SwarmGone,
        _ => ApiError::Upstream(msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hop_by_hop_headers_are_not_forwarded() {
        let mut h = HeaderMap::new();
        h.insert("range", HeaderValue::from_static("bytes=0-1"));
        h.insert("host", HeaderValue::from_static("x"));
        h.insert("connection", HeaderValue::from_static("close"));
        let f = forwardable(&h);
        assert!(f.contains_key("range"));
        assert!(!f.contains_key("host"));
        assert!(!f.contains_key("connection"));
    }
}
