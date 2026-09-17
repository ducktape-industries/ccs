# CCS Messenger Implementation Plan

**Goal:** Local, provider-neutral session messaging with labels, synchronous queue requests and asynchronous inbox messages.
**Architecture:** One foreground Unix socket server owns a JSON state file. Clients address registered sessions by name or AND-matched labels. Existing Claude socket / Codex queue transports deliver synchronous requests; replies are correlated by message ID. Inbox reads do not consume messages; explicit acknowledgement or reply completes them.
**Tech stack:** Existing Rust standard library, serde, serde_json, anyhow; no new dependencies.

## Constraints and decisions
- No repository, manager or worker semantics in server code.
- Same-OS-user trust boundary, private server directory/socket; session names are routing identities, not authentication credentials.
- `CCS_SERVER_DIR` overrides `~/.ccs/messenger`. Account credentials are not opened by messenger commands.
- CLI: `server`, `session register|label|remove`, `sessions`, `queue`, `inbox [send|ack]`, `reply`, `message`.
- A queue command waits for a reply (300-second default); timeout leaves its ID available for later inspection. No forced interruption, retries or success claims from transport acceptance.
- Persist requests before transport; failed/unknown delivery remains inspectable. Restart does not replay requests.
- Session registration is explicit, labels editable, recipient resolution must match exactly one session. Rebinding an existing name to a different endpoint requires removing it first.

## Execution
- [x] Add executable subprocess tests covering labels, async persistence/ack, sync correlated replies, delivery failures and timeout.
- [x] Run against baseline and observe missing-command failure.
- [x] Implement stored session/message operations and bounded local JSON-line server in src/messenger.rs.
- [x] Add CLI parsing/client in src/messenger_cli.rs and dispatch before account Env creation.
- [x] Reuse notification transports through a shared delivery entry point; preserve notification behavior.
- [x] Document usage, trust and delivery semantics in README.
- [x] Run subprocess tests, cargo tests, formatting and clippy; inspect diff for lifecycle and concurrency errors.

## Review refinements
- Client-generated random request IDs exist before submission, including when the response is lost.
- Inbox responses paginate by count and serialized size.
- Reply instructions include the canonical server directory with shell quoting.
- Generic registration only: user explicitly excluded launching/supervising sessions.
- Async replies also enqueue an answer in the original sender's inbox, atomically with the original reply.

## Verification
- 288 existing unit tests and subprocess integration pass; strict CLI clippy, rustfmt and diff whitespace checks pass.
- Workspace strict clippy is blocked by unchanged Linux dead-code warning for app/src/format.rs:9 (`bar_label`).
- Full workspace tests pass: 288 CLI unit tests, 1 subprocess integration, 21 app tests.
