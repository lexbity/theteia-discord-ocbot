# opencode-v2-client

A thin, tolerant HTTP client for the **experimental** opencode v2 HTTP API.

Three properties are normative:

1. **Tolerant deserialization** — unknown fields are ignored, missing fields
   default, and every wire enum carries a forward-compatible catch-all. Hostile
   JSON always produces a `Result`, never a panic (property-tested and fuzzed).
2. **V2 only** — [`Client::probe`] asserts the server is opencode v2 before
   anything else happens.
3. **Async and deterministic** — plain `async fn`s over `reqwest`; you bring
   your own tokio runtime.

```rust
use opencode_v2_client::Client;

let client = Client::new("http://127.0.0.1:8080", "password")?;
let info = client.probe().await?;          // version gate
let session = client.create_session(Some("/path/to/repo"), Some("label")).await?;
let admission = client.send_prompt(&session.id, "Implement the thing").await?;
// ... client.activity(&session.id) until Idle ...
let report = client.last_assistant(&session.id).await?;
```

See [`COMPAT.md`](COMPAT.md) for the pinned opencode server version ↔ client
release matrix and the endpoint/version policy.

> The opencode HTTP API is labeled **experimental**; breaking changes between
> versions are detected by a drift test (a called endpoint disappearing from
> the vendored spec turns CI red), so upgrades are deliberate, not silent.