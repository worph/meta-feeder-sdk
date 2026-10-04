//! Beacon v2 — local resource advertise / discover over UDP multicast
//! (`239.255.99.1:9099`).
//!
//! Every meta-* service and every SDK-hosted plugin runs one [`BeaconNode`]: it
//! advertises this process's [`Resource`]s, each tagged with capability strings
//! (`metamesh.service/meta-sort`, `metamesh.transport/nzb@1`, …), and keeps a
//! live view of everyone else's, which consumers filter by capability
//! ([`cap_matches`]).
//!
//! The normative spec is meta-root's `docs/project-architecture/beacon-v2.md`
//! (the Go / TypeScript / Python ports are written against it too). This module
//! needs only the `beacon` feature, so a service can depend on the SDK with
//! `default-features = false, features = ["beacon"]` and pull in none of the
//! feeder machinery.

mod node;
mod proto;
mod scan;

pub use node::{enabled_from_env, hostname, BeaconNode, Config, SeenNode, SeenResource};
pub use scan::{classify, scan, suggested_name, Candidate, CandidateState, ScanReport, ScanSummary};
pub use proto::{
    cap_matches, rev_of, Message, NodeInfo, Resource, DEFAULT_GROUP, DEFAULT_INTERVAL, DEFAULT_PORT,
    LIVENESS_FACTOR, MAX_DATAGRAM, PROTO, VERSION,
};

/// Capability vocabulary of the MetaMesh profile (spec Part 2).
pub mod caps {
    /// meta-core; its resource carries `data.urls`.
    pub const CORE: &str = "metamesh.core";
    /// Prefix of `metamesh.service/<name>`.
    pub const SERVICE_PREFIX: &str = "metamesh.service/";
    /// Every service.
    pub const ANY_SERVICE: &str = "metamesh.service/*";
    /// Prefix of `metamesh.transport/<id>@<contract>`.
    pub const TRANSPORT_PREFIX: &str = "metamesh.transport/";
    /// Every transport plugin.
    pub const ANY_TRANSPORT: &str = "metamesh.transport/*";
    /// Prefix of `metamesh.feeder/<upstream_id>`.
    pub const FEEDER_PREFIX: &str = "metamesh.feeder/";
    /// Every feeder upstream.
    pub const ANY_FEEDER: &str = "metamesh.feeder/*";
    /// Prefix of `metamesh.enrich/<plugin_id>` (meta-sort enrichment plugins).
    pub const ENRICH_PREFIX: &str = "metamesh.enrich/";
    /// Every meta-sort enrichment plugin.
    pub const ANY_ENRICH: &str = "metamesh.enrich/*";

    pub fn service(name: &str) -> String {
        format!("{SERVICE_PREFIX}{name}")
    }

    pub fn transport(id: &str, contract: u32) -> String {
        format!("{TRANSPORT_PREFIX}{id}@{contract}")
    }

    pub fn feeder(upstream_id: &str) -> String {
        format!("{FEEDER_PREFIX}{upstream_id}")
    }

    pub fn enrich(plugin_id: &str) -> String {
        format!("{ENRICH_PREFIX}{plugin_id}")
    }
}

/// The resource every meta-* service advertises: id `service`, cap
/// `metamesh.service/<name>`, `endpoints.ui` = its browser-facing URL.
pub fn service_resource(name: &str, ui_url: Option<&str>) -> Resource {
    let mut r = Resource::new("service", [caps::service(name)]);
    if let Some(u) = ui_url {
        r = r.with_endpoint("ui", u);
    }
    r
}

/// `BEACON_ADVERTISE_URL`, else `http://$HOSTNAME:<port>` — what a plugin
/// advertises as `endpoints.http`.
pub fn advertise_url(listen_port: u16) -> String {
    std::env::var("BEACON_ADVERTISE_URL")
        .ok()
        .map(|s| s.trim().trim_end_matches('/').to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| format!("http://{}:{}", hostname(), listen_port))
}

/// `BEACON_BINDS`, when set.
pub fn binds_from_env() -> Option<String> {
    std::env::var("BEACON_BINDS").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// Advertise a plugin process: spawn a [`BeaconNode`] named `name` whose single
/// resource is whatever `build` returns, and re-run `build` every interval so a
/// changed manifest (`rev`) is re-advertised at once. Used by both SDK
/// harnesses; returns `None` (and logs) when discovery is disabled or the
/// socket can't be set up — a plugin keeps serving either way.
pub fn advertise_plugin<F>(name: &str, version: &str, build: F) -> Option<BeaconNode>
where
    F: Fn() -> Resource + Send + Sync + 'static,
{
    if !enabled_from_env() {
        tracing::info!("beacon: disabled (ENABLE_UDP_DISCOVERY)");
        return None;
    }
    let cfg = Config::from_env(name).version(version).resource(build());
    let interval = cfg.interval;
    let node = match BeaconNode::spawn(cfg) {
        Ok(n) => n,
        Err(e) => {
            tracing::warn!(error = %e, "beacon: could not start; plugin will not be discoverable");
            return None;
        }
    };
    let refresher = node.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(interval);
        tick.tick().await;
        loop {
            tick.tick().await;
            refresher.set_resources(vec![build()]).await;
        }
    });
    Some(node)
}

/// The `/api/neighbors` JSON every service serves (spec Part 2, "HTTP
/// surface"). Each row is a [`SeenNode`] plus `caps` and a v1-compatible
/// `baseUrl` (the `metamesh.service/*` resource's `endpoints.ui`).
///
/// `all` returns every instance instead of one row per name; `cap` keeps only
/// nodes owning a resource that matches it.
pub fn neighbors_json(
    node: Option<&BeaconNode>,
    current: &str,
    all: bool,
    cap: Option<&str>,
) -> serde_json::Value {
    let Some(node) = node else {
        return serde_json::json!({
            "current": current, "enabled": false, "count": 0, "neighbors": [],
        });
    };
    let rows: Vec<serde_json::Value> = if all { node.nodes() } else { node.nodes_by_name() }
        .into_iter()
        .filter(|n| cap.map_or(true, |c| n.matches(c)))
        .map(|n| neighbor_row(&n))
        .collect();
    serde_json::json!({
        "current": current,
        "enabled": true,
        "count": rows.len(),
        "self": neighbor_row(&node.self_node()),
        "neighbors": rows,
    })
}

fn neighbor_row(n: &SeenNode) -> serde_json::Value {
    let mut v = serde_json::to_value(n).unwrap_or_default();
    if let Some(obj) = v.as_object_mut() {
        obj.insert("caps".into(), serde_json::json!(n.caps()));
        let base = n.resource(caps::ANY_SERVICE).and_then(|r| r.endpoint("ui"));
        if let Some(b) = base {
            obj.insert("baseUrl".into(), serde_json::Value::String(b));
        }
    }
    v
}
