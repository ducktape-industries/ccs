# `ccs serve`: a pi-compatible gateway in front of the stash

## Goal

Let a tool that speaks the Anthropic Messages API — pi (`pi.dev`) in
particular, configured through its `models.json` — use the accounts `ccs` holds,
without its own login and without knowing which account is in use. The account
follows `ccs`: whatever `ccs use`, the picker, or `ccs watch --rotate` installs
is what the gateway sends the next request as.

## What pi does on its side

pi's `anthropic-messages` API treats any key containing `sk-ant-oat` as an OAuth
token. In that mode pi itself sends `Authorization: Bearer <key>`, the
`claude-code-20250219,oauth-2025-04-20` betas, Claude Code's user agent and
`x-app`, prefixes the system prompt with the Claude Code identity line, and maps
tool names onto Claude Code's. Overriding only `baseUrl` on the built-in
`anthropic` provider keeps every built-in model.

So the gateway never rewrites a body. It swaps the bearer token and relays.

## Surface

```
ccs serve [--port <n>] [--rotate <a>,<b>,...]
ccs serve --key
```

- `--port` defaults to 4141. The listener binds `127.0.0.1` only.
- `--rotate` names a pool, resolved like `ccs watch --rotate`. It is only
  consulted on a 429 from upstream (see below).
- `--key` prints the gateway key and exits, for pi's `"apiKey": "!ccs serve --key"`.

On start the command prints the `models.json` snippet to paste into pi:

```json
{ "providers": { "anthropic": { "baseUrl": "http://127.0.0.1:4141", "apiKey": "!ccs serve --key" } } }
```

Each provider's entry also carries a `models` array read from the live catalog
(Claude's `/v1/models`, Codex's `codex/models`). pi merges it into its built-in
list by id, so models released after pi was built (e.g. `claude-opus-5-5`,
`claude-sonnet-5-5`, `gpt-6-sol`) become selectable. If a catalog cannot be
read the array is left out and pi keeps only its own models.

## Gateway key

`<stash root>/gateway.key`, mode 0600, created on first use with the form
`sk-ant-oat-ccs-<32 hex chars>` from `/dev/urandom`. The `sk-ant-oat` prefix is
what flips pi into OAuth mode; the random suffix is what stops another local
process from spending the subscription. A request whose `Authorization: Bearer`
value (or `x-api-key`) is not the key gets `401` with an Anthropic-shaped error
body and never reaches upstream.

## Request handling

One thread per connection, HTTP/1.1 parsed by hand on `std::net::TcpListener`
(no new dependencies; the crate already hand-rolls SHA-256). Request bodies with
`Content-Length` or `Transfer-Encoding: chunked` are read whole into memory —
they are needed whole for the 429 retry anyway.

For each request:

1. Authenticate against the gateway key.
2. Resolve the account: the stash's `active` pointer, falling back to
   identifying the live credentials against the stash. Refresh its access token
   through the existing `freshen` path when spent, and `propagate` the result to
   every copy (stash, live credentials, pen) exactly as the other commands do.
3. Forward method, path and query to `https://api.anthropic.com`, with every
   request header except hop-by-hop ones, `host`, `content-length`, and the
   client's own credentials (`authorization`, `x-api-key`); add
   `Authorization: Bearer <access token>`.
4. On a `429` with a pool configured: try each other pool member in turn
   (freshening as needed) before any byte is sent to the client. The first
   non-429 answer is relayed. If every member is limited, the last 429 is
   relayed as is.
5. On a `401` from upstream, the stashed copy is presumed stale: `reconcile`
   that one account against its other copies, and retry once if that produced
   a newer token.
6. Relay status, response headers and body. The body is streamed with
   `Transfer-Encoding: chunked` as it arrives, so SSE works.

Upstream is a second `ureq::Agent` without the 15-second global timeout the
identity/usage client uses, since a streamed completion can run for minutes.

Everything else — `GET /` and any unknown route — answers `404`.

## Files

- `src/serve.rs` — HTTP parsing and writing, header filtering, key generation
  and checking, the retry decision. Pure parts unit-tested; the socket loop is
  thin.
- `src/cmd.rs` — `serve` command: account resolution and token freshening
  reuse `freshen`, `propagate`, `reconcile`, `identify`.
- `src/cli.rs`, `README.md` — surface and help text.

## Out of scope

- OpenAI-format translation (`openai-completions`). pi speaks Anthropic Messages.
- Choosing accounts per request by usage. The active account is the policy;
  `ccs watch --rotate` already moves it.
- TLS, non-loopback binding, multiple clients with different keys.
