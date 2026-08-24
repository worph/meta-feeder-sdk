# meta-feeder-sdk

The source-agnostic foundation shared by the MetaMesh gateway core and every
feeder sidecar.

A **feeder** bridges one external source into the MetaMesh network. It
implements [`FeederPlugin`](src/plugin.rs) — find records, resolve them to
content-addressed outcomes, optionally fetch bytes — and `serve_feeders` exposes
any set of plugins over the feeder HTTP contract. The gateway core consumes that
contract and keeps the libp2p wire, the bitswap blockstore, the
hashing-into-the-blockstore and the meta-core store-back to itself.

Deliberately **libp2p-free and blockstore-free**: a feeder finds and fetches
bytes; it never joins the swarm.

## Using it

```toml
[dependencies]
meta-feeder-sdk = { git = "https://github.com/worph/meta-feeder-sdk", tag = "v0.1.0" }
```

Not on crates.io yet — the contract is still moving, so consumers pin a tag.

```rust
use meta_feeder_sdk::{serve_feeders, FeederPlugin};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let plugins: Vec<Box<dyn FeederPlugin>> = vec![Box::new(MyPlugin::new())];
    serve_feeders(plugins, "/data/meta-feeder".into(), "0.0.0.0:8080".parse()?).await
}
```

A **service is a `Vec`** — grouping plugins into one binary is a deployment
choice, not an architectural one.

## Writing a plugin

Four required methods; everything else has a default:

| method | you must | notes |
|---|---|---|
| `upstream_id` | return a stable `&'static str` | identifies your records |
| `configure` | set up a cache dir; return `MissingConfig` if unconfigured | the harness soft-skips a plugin that returns `MissingConfig`, so an unconfigured plugin degrades instead of killing the sidecar |
| `handle_query` | answer a `GatewayQuery` with `DiscoveryRecord`s | **open with `query_eval::query_accepts_plugin`** — see below |
| `compute_outcomes` | resolve a `record_id` to `HashOutcome`s | `bytes: None` for metadata-only sources |

Defaults you can override: `handle_query_stream`, `handle_fetch`, `health`,
`served_file_types`, `served_content_kinds`, `config_schema`, `config_values`,
`get_blob`.

### The routing gate

The gateway dispatches on `(fileType, contentKind)`, never on a service name.
Advertise your axes and gate on them, or you will be handed queries you cannot
answer:

```rust
fn served_file_types(&self)    -> &'static [&'static str] { &["video"] }
fn served_content_kinds(&self) -> &'static [&'static str] { &["movie", "series"] }

async fn handle_query(&self, q: &GatewayQuery, n: usize) -> Result<Vec<DiscoveryRecord>, GatewayError> {
    if !meta_feeder_sdk::query_eval::query_accepts_plugin(
        q, self.served_file_types(), self.served_content_kinds()) {
        return Ok(Vec::new());
    }
    // ...
}
```

`&["*"]` on an axis means "I serve every value on it".

### Three rules that are not obvious

1. **Records must carry a type filter.** A record with no `fileType` /
   `contentKind` never reaches a client wall, and the symptom looks like a
   transport bug rather than a metadata one.
2. **`filters` / `ranges` / `negations` are authoritative; `raw_text` is
   informational.** Consume one source per filter or you will double-filter and
   under-match.
3. **Never degrade an unresolvable anchor into a free-text query.**
   `free_text_or_star()` exists for upstreams that 4xx on empty input — reaching
   for it when an *identity* filter failed to resolve dumps the upstream's
   recent feed into someone's library.

## Hash families

`HashKind` selects how the core routes an outcome. Only `Sha2_256` is seeded
into the bitswap blockstore.

| kind | codec | means |
|---|---|---|
| `Sha2_256` | standard | real bytes, retrievable via bitswap |
| `Midhash256` | `0x1000` | size + middle-sample; MetaMesh-internal |
| `BtV1File` | `0x1001` | a file inside a torrent — locator |
| `NzbRelease` | `0x1005` | a Usenet release — locator, redeemed by a credentialed peer |
| `CardLocator` | `0x1007` | a *work*, no bytes anywhere |
| `YtVideo` | `0x1008` | **delegated playback** — bytes are permanently someone else's |

The last two carry an identity multihash. Anything that dispatches on the
multihash instead of the **codec** will silently mis-rank them.

## Layout

| module | what |
|---|---|
| `plugin` | the `FeederPlugin` trait, `HashKind`, `HashOutcome`, `ConfigError` |
| `types` | `DiscoveryRecord`, `GatewayError`, `PluginHealth`, `ByteStream` |
| `query` / `query_eval` | the structured query and its evaluator |
| `serve` | the axum harness and the HTTP contract DTOs |
| `config` | self-describing config schema — the dashboard renders a form from it |
| `cache` | per-plugin redb cache |
| `hash` | the content-addressing families above |
| `filename_meta` / `lang` | release-tag parsing helpers |
| `enrich`, `meta_core`, `common` | MetaMesh-internal; not part of the plugin contract |

## Development

```bash
cargo build && cargo test
```

To work on the SDK from inside a feeder checkout without pushing a tag, add a
gitignored `.cargo/config.toml` in the feeder:

```toml
paths = ["../meta-feeder-sdk"]
```

## Context

Design rationale, the feeder roster, and the naming rules live in the meta-root:
`docs/project-architecture/feeder-architecture.md`.

## License

MIT
