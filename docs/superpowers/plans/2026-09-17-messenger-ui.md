# Messenger UI implementation plan

**Goal:** Visualize session-based inbox and queue flows in the existing GPUI app, with selectable CCS servers.
**Architecture:** A separate Messenger view connects through the CCS client API. Server/session changes clear prior data and invalidate stale background results. The server provides bounded per-session history, including queue outcomes and async replies. No independent mailboxes or sentry orchestration.

- [x] Add a paginated per-session history query and provider labels to session discovery; cover with the existing subprocess test.
- [x] Add server connection selection (local plus direct HTTP with a bearer token); generate a private token on the server; keep app tokens in memory.
- [x] Build a Messenger tab with server controls, session list/labels, Inbox/Queue filters, message details, receipt status, and explicit acknowledgement/reply actions.
- [x] Keep socket/network work off the UI thread. Invalidate responses when selection changes and keep drafts tied to their server/session/message.
- [x] Add GPUI interaction checks, exercise real isolated server traffic, and capture the rendered view.
- [x] Run workspace tests, formatting and lint; update the existing organization PR.

User refinement: start on current/local CCS; expose remote HTTP connection through a secondary View remote CCS action, never an initial server picker.

Realtime refinement: Unix subscription and authenticated HTTP SSE send initial/revision invalidations. The app coalesces reloads, cancels replaced subscriptions, and reconnects before loading fresh state. Heartbeats do not reload pages.

Verification: workspace tests (288 core + 6 integration + 23 app), strict workspace Clippy, format/whitespace checks, and native GPUI with isolated local and separate HTTP servers. Native screenshots are in docs/images/messenger-local.png and docs/images/messenger-remote.png. A remote reply was also submitted through GPUI and verified in server storage.
