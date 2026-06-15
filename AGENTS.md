# herdr-simple-mcp — AGENTS.md

## What this is

A single Rust crate: a thin, stateless MCP stdio server that maps MCP tool calls onto
herdr's Unix-socket JSON API. herdr's daemon is the stateful authority; we add none.

## Build & test

```bash
cargo build --release      # → target/release/herdr-simple-mcp
cargo test                 # unit tests (socket module uses an in-test UnixListener fixture)
```

## Architecture

- **`src/herdr.rs`** — the socket client (ported from `herdr-python-client`). Socket
  discovery (`HERDR_SOCKET_PATH` → `HERDR_SESSION` → `~/.config/herdr/herdr.sock`),
  `request(method, params)` opens one `UnixStream` per call, writes a newline-delimited
  `{id, method, params}` envelope, reads one response line, maps `error`→`HerdrError::Api`
  and `result`→value. One connection per call = stateless.
- **`herdr-tools.yaml` + `src/manifest.rs`** — the tool **contract** (single source of
  truth), compiled into the binary via `include_str!` — fixed at build, no runtime override.
  `manifest.rs` parses it into `MethodSpec`s and builds each tool's JSON Schema — strict
  (`additionalProperties:false`, required, enums) for declared params, open for
  `passthrough`. It also appends the `herdr_call` escape hatch.
- **`src/server.rs`** — builds the `ToolRouter` **dynamically**: one `ToolRoute::new_dyn`
  per manifest method, forwarding args through `call(method, params)`. Sets read-only /
  destructive MCP annotations; on failure returns structured
  `{error:{kind,code,message,method}}`.
- **`src/config.rs`** — capability groups (read from the manifest) + role profiles. `new()`
  disables excluded tools via `ToolRouter::disable_route`; `#[tool_handler(router =
  self.tool_router)]` serves the filtered router. **To change the surface, edit
  `herdr-tools.yaml`** — not Rust.
- **`src/main.rs`** — tracing→stderr, `serve(stdio())`, wait.

## Hard invariants

- **stdout is MCP JSON-RPC only.** Never print to stdout from server code; logs → stderr.
- IDs (workspace/tab/pane/agent) are **session-local** and may compact when items close.
  Re-read them from `list_*` after structural changes.

## Socket API notes

The tool list mirrors the daemon's own method set 1:1 (dots → underscores). Params are
declared in `herdr-tools.yaml` where verified (strict schema) and forwarded verbatim
otherwise (`passthrough`). A few facts worth knowing:

- **Method names are strict; params are lenient.** The daemon rejects an unknown method
  with a serde "unknown variant" error that *lists every valid method* — the quickest way
  to re-check/regenerate the surface is to send a bogus method to the socket and read the
  list back. Unknown param fields are ignored.
- Agents are addressed by `target` (a `terminal_id`, `pane_id`, or unambiguous agent name).
- `pane.wait_for_output` / `events.wait` block server-side. The socket read deadline is
  300s by default; override with `HERDR_MCP_TIMEOUT_MS`. If a request carries `timeout_ms`,
  that drives the deadline instead.
- Envelopes: request `{"id","method","params":{}}`; success `{"id","result":{...}}`;
  error `{"id","error":{"code","message"}}`.

### Regenerating / drift

The `manifest_matches_live_daemon` test (`cargo test -- --ignored`, needs a running herdr)
asserts the manifest's methods exactly match the daemon's. When it flags drift, edit
`herdr-tools.yaml` — nothing in Rust changes.

## Dependencies

`rmcp` 1.7 (`server,transport-io,macros`), `tokio` (`rt-multi-thread,macros,net,io-util,time`),
`serde`/`serde_json`/`serde_yaml`, `anyhow`, `tracing`/`tracing-subscriber`. No axum/clap/regex.
