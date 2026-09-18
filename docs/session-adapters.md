# Adding a compiled session adapter

CCS messenger uses the Rust `Adapter` interface in `src/adapters/mod.rs`:

```rust
pub trait Adapter {
    fn provider(&self) -> &'static str;
    fn validate(&self) -> anyhow::Result<()>;
    fn deliver(&self, body: &str) -> anyhow::Result<()>;
}
```

`ClaudeCode` and `Codex` implement this interface in their own modules. An adapter
holds the endpoint of an already running session. `validate` checks that endpoint
before registration; `deliver` submits a queue message through the agent's native
transport. A successful submission is not a receipt or an answer.

To support another agent:

1. Add `src/adapters/<agent>.rs` with a `Clone + Debug + Serialize + Deserialize`
   configuration type and an `Adapter` implementation. Implement session discovery
   or configuration loading in that module. Return errors for invalid endpoints,
   unsupported modes and failed submissions; bound every transport wait and never
   automatically retry an ambiguous delivery.
2. Export the type from `src/adapters/mod.rs`, add its variant to `Session`, and
   dispatch it from `Session::adapter`. The serialized `provider` tag must equal
   the name returned by `Adapter::provider`. Keep tags and endpoint fields stable:
   the server stores this configuration across restarts.
3. Add its name to `Session::register` and the unknown-adapter diagnostic. That
   branch calls the new module's discovery/configuration function. New adapters
   can read their own environment variables; they need no account `model::Provider`
   variant. Built-in environment auto-detection remains for Claude Code and Codex;
   use the explicit name for an additional adapter.
4. Add a synthetic transport test covering registration, queue submission and
   failure. Verify the stored endpoint's JSON round trip and the correlated
   `ccs reply` path. Rebuild CCS and restart the server with the new binary.

Registration then uses the shared CLI:

```sh
ccs session register my-session --adapter <agent> --label role=worker
```

The built-ins accept `--adapter claude` and `--adapter codex`; existing `--claude`
and `--codex` options remain aliases. The current stored JSON format is unchanged.

Inbox storage, labels, name exclusivity, queue correlation, history, HTTP/SSE and
GPUI do not need provider-specific changes. Inbox messages stay in CCS until
acknowledged; inbox sends invoke `deliver` unless the recipient has `wake=sentry`,
while queue sends always invoke it. The delivered body
includes the message ID; the recipient replies through the common CCS API.
Replies and reads trigger the same streams for every adapter.

Account-usage notifications (`ccs notify`) are a separate feature. They currently
support Claude and Codex; adding a messenger adapter does not add account usage
or notification support. The fallback reports that usage notices are unsupported.
