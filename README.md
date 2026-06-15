# herdr-simple-mcp

A thin, stateless [MCP](https://modelcontextprotocol.io) server that exposes
[herdr](https://github.com/ogulcancelik/herdr) — a terminal-native agent multiplexer —
as tools, by attaching **directly to herdr's Unix-socket API**.

herdr's daemon already owns all workspace/tab/pane/agent state, so this server adds no
state of its own: each tool call opens a fresh socket connection, sends one
newline-delimited JSON request, reads one response, and returns it. No CLI shell-out, no
HTTP bridge, no background server to manage.

## Why

Other herdr MCP servers tend to stand up their own services or layer on new functionality —
HTTP bridges, web playgrounds, recipe/workflow engines, their own persistent state.
herdr-simple-mcp deliberately does none of that. It's a straightforward, faithful library
over herdr's existing socket API and nothing more: herdr's daemon is the authority, and the
binary is just a thin translation from MCP tool calls to socket requests.

Because it's a single Rust binary, it's lightweight in memory and fast to start — cheap to
spawn as a per-session stdio subprocess (one per agent role), with no interpreter, no
runtime, and no extra process to run or babysit.

## Build & install

```bash
cargo build --release          # → target/release/herdr-simple-mcp

# …or install onto your PATH (~/.local/bin):
cargo install --path . --root ~/.local
# upgrade later by re-running the same command with --force
```

## Use

Register with any MCP host (Claude Code/Desktop, Cursor, …). If you installed onto your
PATH the command is just `herdr-simple-mcp`; otherwise use the full path to the binary:

```json
{
  "mcpServers": {
    "herdr": { "command": "herdr-simple-mcp" }
  }
}
```

herdr must be running. The socket is discovered in this order:

| Env | Meaning |
|-----|---------|
| `HERDR_SOCKET_PATH` | Explicit socket path (highest priority) |
| `HERDR_SESSION` | Uses `~/.config/herdr/sessions/<session>/herdr.sock` |
| *(default)* | `~/.config/herdr/herdr.sock` |

Other env:

- `RUST_LOG` — tracing filter (default `herdr_simple_mcp=info`); logs go to **stderr**.
- `HERDR_MCP_TIMEOUT_MS` — socket read deadline for blocking calls (default 300000).

## Tools

One tool per herdr socket method, named identically with dots → underscores
(`workspace.list` → `workspace_list`, `pane.send_text` → `pane_send_text`, …). The full
live surface is exposed — 75 methods on herdr 0.7.0 across server/workspace/worktree/tab/
pane/agent/layout/events/integration/plugin/client.

The tool set and each tool's **input schema** come from a checked-in contract,
[`herdr-tools.yaml`](herdr-tools.yaml), compiled into the binary at build time (the surface
is fixed at build — there is no runtime override). Methods verified against a live daemon
get a strict schema — required fields, enums, and
`additionalProperties: false` so typos are rejected; the rest accept an open params object.
Read-only and destructive tools carry MCP annotations. A **`herdr_call(method, params)`**
escape hatch forwards anything not exposed directly (e.g. after a daemon upgrade).

Failures return `is_error` with structured JSON:
`{"error":{"kind":"api"|"transport","code":…,"message":…,"method":…}}`.

## Selecting a tool surface

75 tools is a lot to hand any one agent. Tools are partitioned into capability **groups**,
and a **profile** is a named bundle of groups for a role. Pick a surface per MCP entry:

| var | meaning |
|-----|---------|
| `HERDR_MCP_PROFILE` | `full` (default) · `coordinator` · `client` · `observer` |
| `HERDR_MCP_GROUPS`  | explicit comma list of groups; overrides the profile |
| `HERDR_MCP_ALLOW`   | comma list of methods / `prefix*` globs to add |
| `HERDR_MCP_DENY`    | comma list of methods / `prefix*` globs to remove (wins) |

Resolution: `groups(profile or GROUPS)` → `+ALLOW` → `−DENY`. Globs match the dotted
method (`pane.*`) or the tool name (`pane_*`).

**Groups:** `observe` (read-only) · `structure` (workspace/tab/worktree lifecycle) ·
`panes` (split/swap/move/zoom/resize/close/layout) · `io` (send_text/keys/input,
wait_for_output) · `agents` (start/send/rename/focus) · `authority` (report_*/authority) ·
`events` (subscribe/wait) · `admin` (server/integration/plugin/notification).

| profile | groups | tools |
|---------|--------|-------|
| `full` *(default)* | all real methods (the `raw` escape hatch is opt-in) | 75 |
| `coordinator` | observe + structure + panes + io + agents + events | 53 |
| `client` | observe + io + authority + events | 34 |
| `observer` | observe | 23 |

```jsonc
// a coordinator agent and a client agent — same binary, different surfaces
"herdr-coord":  { "command": "herdr-simple-mcp", "env": { "HERDR_MCP_PROFILE": "coordinator" } }
"herdr-client": { "command": "herdr-simple-mcp", "env": { "HERDR_MCP_PROFILE": "client" } }

// full, minus the dangerous stuff
"herdr": { "command": "herdr-simple-mcp", "env": { "HERDR_MCP_DENY": "server.*,plugin.*" } }
```

Groups gate *which tools*, not *which objects* — a `client` can still observe every
workspace, not only its own.

## Security

This is a **local, trusted-host bridge**. The default `full` surface exposes terminal
input, worktree, plugin, and server-control actions to anything that can reach the MCP host
— treat it like shell access. Constrain it per role with a profile (`observer` is
read-only) or a denylist (e.g. `HERDR_MCP_DENY=server.*,plugin.*,worktree.remove`).
Defenses in depth:

- **Fail-closed config** — an unknown `HERDR_MCP_PROFILE` falls back to `observer`
  (read-only), never `full`.
- **The `raw` escape hatch is opt-in** — `herdr_call` is off in every profile (enable via
  `HERDR_MCP_GROUPS=…,raw` or `HERDR_MCP_ALLOW=herdr_call`). When on, it still applies your
  `HERDR_MCP_DENY` patterns and refuses any *manifest* method your profile disabled. It
  can't group-classify methods absent from the manifest, so a genuinely new daemon method
  passes through unless a deny pattern matches it.
- **Server-side validation** — arguments are checked against each tool's schema before
  anything is sent to herdr.
- **Annotations** — destructive tools carry MCP `destructiveHint`, read-only tools
  `readOnlyHint`.

## License

[GNU AGPL v3.0](LICENSE) — matching herdr's own license.
