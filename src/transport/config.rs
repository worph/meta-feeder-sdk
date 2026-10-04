//! A transport plugin's own settings — the "config plane".
//!
//! Same model as a feeder's (`crate::config`): the plugin declares a
//! [`ConfigSchema`], keeps its values in `<state_dir>/config.json`, and serves a
//! schema-driven page that meta-share's dashboard embeds through its plugin
//! proxy (`/api/plugins/:name/ui/config`). Keys stay on the plugin; the hull
//! carries no protocol-specific settings.
//!
//! Routes (mounted by [`serve`](super::serve) when
//! [`TransportPlugin::config`](super::plugin::TransportPlugin::config) is
//! `Some`):
//!
//! - `GET /config`        → the page ([`CONFIG_PAGE_HTML`]); every call it makes
//!   is relative to its own path, so the proxy prefix is irrelevant.
//! - `GET /config/schema` → [`ConfigSchema`].
//! - `GET /config/values` → effective values, secrets redacted to `<key>_set`.
//! - `PUT /config/values` → merge (blank secret keeps the stored one), persist
//!   atomically, then exit so the container restarts on the new values
//!   (`{"saved":true,"reloading":true}`). Plugins read their config once at
//!   boot — a restart is the one apply path that is right for every one of them.
//!
//! **Effective values** = `config.json` if it exists and parses as an object,
//! else the `seed` the plugin derived from its env (its pre-config-plane
//! settings source). A plugin deserializes [`ConfigPlane::effective`] into its
//! own typed struct with serde defaults, so a missing key is never fatal.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::Value;
use tracing::{info, warn};

use crate::config::{merge, redact, ConfigSchema, CONFIG_PAGE_HTML};

/// Env var naming the plugin's own writable state dir.
pub const STATE_DIR_ENV: &str = "META_SHARE_PLUGIN_STATE_DIR";
/// Default state dir (the plugin app's own `AppData/<app>/state` mount).
pub const DEFAULT_STATE_DIR: &str = "/state";
/// File name of the stored values inside the state dir.
pub const CONFIG_FILE: &str = "config.json";

/// The plugin's state dir from [`STATE_DIR_ENV`], else [`DEFAULT_STATE_DIR`].
pub fn state_dir_from_env() -> PathBuf {
    std::env::var(STATE_DIR_ENV)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_STATE_DIR.to_string())
        .into()
}

/// One plugin's schema + stored values.
pub struct ConfigPlane {
    schema: ConfigSchema,
    path: PathBuf,
    seed: Value,
    restart_on_save: bool,
}

impl ConfigPlane {
    /// `seed` is what the plugin would run with if no `config.json` existed
    /// (normally its env-derived settings, serialized).
    pub fn new(schema: ConfigSchema, state_dir: &Path, seed: Value) -> Self {
        Self {
            schema,
            path: state_dir.join(CONFIG_FILE),
            seed,
            restart_on_save: true,
        }
    }

    /// Don't exit after a save (tests, or a plugin that hot-applies).
    pub fn without_restart(mut self) -> Self {
        self.restart_on_save = false;
        self
    }

    pub fn schema(&self) -> &ConfigSchema {
        &self.schema
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The stored values, if `config.json` exists and is a JSON object.
    pub fn stored(&self) -> Option<Value> {
        let bytes = std::fs::read(&self.path).ok()?;
        match serde_json::from_slice::<Value>(&bytes) {
            Ok(v @ Value::Object(_)) => Some(v),
            Ok(_) | Err(_) => {
                warn!(path = %self.path.display(), "plugin config is not a JSON object; using the seed");
                None
            }
        }
    }

    /// Stored values, else the seed.
    pub fn effective(&self) -> Value {
        self.stored().unwrap_or_else(|| self.seed.clone())
    }

    /// Persist `values` atomically (tmp + rename), creating the state dir.
    pub fn save(&self, values: &Value) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let body = serde_json::to_vec_pretty(values)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, &self.path)
    }

    /// Write `values` as the initial config **only** if no `config.json` exists
    /// yet — the one-shot migration hook (e.g. the nzb plugin importing the
    /// NNTP block meta-share used to own). Returns whether it wrote.
    pub fn seed_file_if_absent(&self, values: &Value) -> std::io::Result<bool> {
        if self.path.exists() {
            return Ok(false);
        }
        self.save(values)?;
        Ok(true)
    }

    /// The `/config*` routes.
    pub fn routes(self: Arc<Self>) -> Router {
        Router::new()
            .route("/config", get(page))
            .route("/config/schema", get(schema))
            .route("/config/values", get(values_get).put(values_put))
            .with_state(self)
    }
}

/// Lenient readers for a config object as the page writes it: an empty number
/// is `null`, an empty text is `""`, and a hand-edited file may hold a number as
/// a string. Each returns `None` for "not set" so the caller falls back to its
/// default.
pub mod read {
    use serde_json::Value;

    /// A non-blank, trimmed string.
    pub fn text(v: &Value, key: &str) -> Option<String> {
        match v.get(key)? {
            Value::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }

    pub fn number(v: &Value, key: &str) -> Option<f64> {
        match v.get(key)? {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    /// A non-negative integer (fractions truncate, negatives are "not set").
    pub fn uint(v: &Value, key: &str) -> Option<u64> {
        number(v, key).filter(|n| n.is_finite() && *n >= 0.0).map(|n| n as u64)
    }

    pub fn flag(v: &Value, key: &str) -> Option<bool> {
        match v.get(key)? {
            Value::Bool(b) => Some(*b),
            Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    /// A list of non-blank strings (a string is split on commas/newlines).
    pub fn list(v: &Value, key: &str) -> Option<Vec<String>> {
        let items: Vec<String> = match v.get(key)? {
            Value::Array(a) => a.iter().filter_map(|x| x.as_str()).map(str::to_string).collect(),
            Value::String(s) => s.split([',', '\n']).map(str::to_string).collect(),
            _ => return None,
        };
        Some(items.into_iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
    }
}

async fn page() -> Html<&'static str> {
    Html(CONFIG_PAGE_HTML)
}

async fn schema(State(p): State<Arc<ConfigPlane>>) -> Json<ConfigSchema> {
    Json(p.schema.clone())
}

async fn values_get(State(p): State<Arc<ConfigPlane>>) -> Json<Value> {
    Json(redact(&p.effective(), &p.schema))
}

async fn values_put(
    State(p): State<Arc<ConfigPlane>>,
    Json(incoming): Json<Value>,
) -> Result<Json<Value>, Response> {
    if !incoming.is_object() {
        return Err((StatusCode::BAD_REQUEST, "expected a JSON object").into_response());
    }
    let merged = merge(&p.effective(), &incoming, &p.schema);
    p.save(&merged).map_err(|e| {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("write config: {e}")).into_response()
    })?;
    if !p.restart_on_save {
        info!(path = %p.path.display(), "plugin config saved");
        return Ok(Json(serde_json::json!({ "saved": true, "reloading": false })));
    }
    info!(path = %p.path.display(), "plugin config saved; restarting to apply");
    // Let this response flush, then exit; `restart: unless-stopped` brings the
    // plugin back and it reads config.json at boot.
    tokio::spawn(async {
        tokio::time::sleep(Duration::from_millis(800)).await;
        std::process::exit(0);
    });
    Ok(Json(serde_json::json!({ "saved": true, "reloading": true })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigField;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn schema() -> ConfigSchema {
        ConfigSchema {
            fields: vec![
                ConfigField::text("host", "Host"),
                ConfigField::secret("pass", "Password"),
                ConfigField::number("connections", "Connections"),
            ],
        }
    }

    fn plane(dir: &Path) -> Arc<ConfigPlane> {
        Arc::new(
            ConfigPlane::new(schema(), dir, serde_json::json!({"host":"seed.example","connections":4}))
                .without_restart(),
        )
    }

    async fn call(app: Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("content-type", "application/json")
            .body(match body {
                Some(b) => Body::from(serde_json::to_vec(&b).unwrap()),
                None => Body::empty(),
            })
            .unwrap();
        let r = app.oneshot(req).await.unwrap();
        let status = r.status();
        let bytes = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    #[tokio::test]
    async fn seed_until_saved_then_file_wins_and_secret_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let p = plane(dir.path());
        assert_eq!(p.effective()["host"], "seed.example");

        let (st, v) = call(Arc::clone(&p).routes(), "GET", "/config/values", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["host"], "seed.example");
        assert_eq!(v["pass_set"], false);

        let (st, _) = call(
            Arc::clone(&p).routes(),
            "PUT",
            "/config/values",
            Some(serde_json::json!({"host":"news.example","pass":"s3cret"})),
        )
        .await;
        assert_eq!(st, StatusCode::OK);
        let stored = p.stored().unwrap();
        assert_eq!(stored["host"], "news.example");
        assert_eq!(stored["pass"], "s3cret");
        assert_eq!(stored["connections"], 4, "absent field keeps the effective value");

        // A blank secret keeps the stored one; the GET never leaks it.
        call(Arc::clone(&p).routes(), "PUT", "/config/values", Some(serde_json::json!({"pass":""}))).await;
        assert_eq!(p.stored().unwrap()["pass"], "s3cret");
        let (_, v) = call(Arc::clone(&p).routes(), "GET", "/config/values", None).await;
        assert!(v.get("pass").is_none());
        assert_eq!(v["pass_set"], true);
    }

    #[tokio::test]
    async fn schema_and_page_are_served() {
        let dir = tempfile::tempdir().unwrap();
        let p = plane(dir.path());
        let (st, v) = call(Arc::clone(&p).routes(), "GET", "/config/schema", None).await;
        assert_eq!(st, StatusCode::OK);
        assert_eq!(v["fields"].as_array().unwrap().len(), 3);
        let r = p
            .routes()
            .oneshot(Request::get("/config").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[test]
    fn seed_file_if_absent_writes_once() {
        let dir = tempfile::tempdir().unwrap();
        let p = plane(dir.path());
        assert!(p.seed_file_if_absent(&serde_json::json!({"host":"a"})).unwrap());
        assert!(!p.seed_file_if_absent(&serde_json::json!({"host":"b"})).unwrap());
        assert_eq!(p.effective()["host"], "a");
    }

    #[test]
    fn readers_treat_blank_as_unset() {
        let v = serde_json::json!({"a":"", "b":" x ", "n":null, "m":"7", "f":"off", "l":["a"," ",""], "s":"p, q"});
        assert_eq!(read::text(&v, "a"), None);
        assert_eq!(read::text(&v, "b").as_deref(), Some("x"));
        assert_eq!(read::uint(&v, "n"), None);
        assert_eq!(read::uint(&v, "m"), Some(7));
        assert_eq!(read::flag(&v, "f"), Some(false));
        assert_eq!(read::list(&v, "l"), Some(vec!["a".to_string()]));
        assert_eq!(read::list(&v, "s"), Some(vec!["p".to_string(), "q".to_string()]));
        assert_eq!(read::text(&v, "missing"), None);
    }

    #[test]
    fn a_non_object_file_falls_back_to_the_seed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(CONFIG_FILE), b"[1,2]").unwrap();
        assert_eq!(plane(dir.path()).effective()["host"], "seed.example");
    }
}
