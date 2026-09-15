# GPUI migration and model routing validation

## Native app

- GPUI Kit 0.6.1 replaces Iced and Ice entirely.
- Inspected the running macOS preview with native window controls, compact
  monochrome tabs, account rows and five usage limits wrapping onto two rows.
- Headless GPUI pointer/input tests cover adding, editing and removing routes,
  primary/fallback order, seven limits including a long name, 780/1100px window
  widths and the reserved space for macOS window controls.
- The app shows actual quota windows only; availability-only metadata such as
  Astra is excluded from the usage grid. CLI availability reporting is unchanged.
- Six combined Claude/Codex accounts have zero vertical scroll range at 780px
  and 860px widths in a headless UI test. Verified all six real accounts visible
  in the installed app; minimum height follows up to six accounts.
- Replaced `/Applications/ccs.app`, verified its signature and executable hash,
  and confirmed it runs from the installed path.
- Menu bar activation sets macOS MoveToActiveSpace before raising the dashboard.
  Actual activation across Spaces remains unverified.
- Final app tests: 19 passed. Earlier core tests: 272 passed, 1 ignored.
  Final app Clippy and formatting checks passed.

## Claude Code

Installed version tested: 2.1.269.

1. `scripts/verify-claude-routing.py` runs real Claude Code against a local SSE
   stub, using a temporary configuration and fake token. A Fable parent makes a
   real Agent tool call with `model: opus`, then resumes. Requests were
   `claude-fable-5-1`, `claude-opus-5`, `claude-fable-5-1`; all kept the local
   endpoint, bearer credential and OAuth beta header.
2. A live `ccs claude` Fable request using the existing active account returned OK.
3. A live parent/subagent check used two different existing accounts through
   temporary access-only stash copies (refresh tokens removed) and temporary
   rules. Successful upstream request trace:

   ```text
   POST /v1/messages 200 as verify-fable
   POST /v1/messages 200 as verify-opus
   POST /v1/messages 200 as verify-fable
   ```

   Claude Code returned `ROUTING_OK`. No permanent model assignments were changed.
   Temporary copies were removed when the check finished. This proves this tested
   flow; future provider entitlement or authentication changes are not covered.

Core tests also cover exact/longest-prefix precedence, provider separation,
per-model fallback exhaustion, invalid/missing targets, hot reloads, unchanged
request bytes, simultaneous model requests and preservation of the active login.

## Commands

```sh
cargo fmt --all --check
TMPDIR=/private/tmp cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo build --release --workspace
python3 scripts/verify-claude-routing.py
```

`TMPDIR=/private/tmp` avoids existing macOS tests comparing `/var` paths with
canonical `/private/var` paths. No CI was invoked. Linux runtime was not tested.
