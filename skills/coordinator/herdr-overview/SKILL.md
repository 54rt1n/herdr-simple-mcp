---
name: herdr-overview
description: >-
  How to operate herdr — a terminal-native agent multiplexer — through the
  herdr-simple-mcp tools. Read this before using any herdr tool (workspace_*, tab_*,
  pane_*, agent_*, events_*). Explains the workspace/tab/pane/agent model, how agent
  status is detected, and which tool to reach for to discover, build, spawn, drive, wait,
  and clean up.
---

# Operating herdr

herdr is a tmux-like terminal multiplexer that is **agent-aware**: processes running in its
panes are recognized as agents, and their state is tracked for you. You drive it through
MCP tools, each named after a herdr socket method with dots replaced by underscores
(`pane.send_text` → `pane_send_text`). You hold no state; herdr is the source of truth.

## The model

```
workspace ── tab ── pane ── (agent)
```

- **workspace** — a top-level project space. Has tabs.
- **tab** — a screenful within a workspace. Has panes.
- **pane** — a real terminal (a shell, an agent, a command). The unit you read from and
  send input to.
- **agent** — a coding agent (Claude Code, Codex, …) detected running in a pane. Addressed
  by **`target`**.

**IDs are session-local and compact when things close.** After anything structural
(closing/creating panes, tabs, workspaces), *re-read* ids from the `list_*` tools before
using them — a stale id may now point at something else.

## Agent status is automatic

herdr detects each agent's state on its own: **`idle` · `working` · `blocked` · `done`**
(also `unknown`). You do **not** need to report it — just read it. `blocked` means the
agent is waiting on input; `done` means it finished its turn. This is the signal you watch
to coordinate work.

## Which tool, when

### Find your way around (start here)
Scope ids are **optional** — omit them to act on the focused context or list session-wide.
- `workspace_list` → `tab_list {workspace_id?}` → `pane_list {tab_id?}` → `agent_list {workspace_id?}`
- `*_get` / `pane_current` for details on one thing.
- `ping` to confirm herdr is up.

### Build structure
- `create_workspace {cwd?, label?}` — a new project space.
- `create_tab {workspace_id?, label?}` — a new screen.
- `split_pane {direction: right|down|left|up, ratio?}` — carve a pane out of another.
- `worktree_create {branch, ...}` — a git worktree, for running work on several branches in
  parallel (one agent per worktree).

### Run agents
- `start_agent {name}` — launch an agent; `name` is which (e.g. `claude`, `codex`). herdr
  places it in a pane. Re-read `agent_list` afterward to get its id.
- `agent_send {target, text}` — give an agent an instruction.
- `agent_read {target, source, lines?}` — read its output. `agent_get` / `agent_explain`
  for status and a plain-language account of what it's doing.

### Drive a raw terminal (no agent)
- `pane_send_input {pane_id?, input}` — type a line **and submit it** (a command). The usual
  choice.
- `pane_send_text` (no Enter) / `pane_send_keys {keys}` (e.g. `"Enter"`, `"C-c"`) — for
  finer control.
- `pane_read {pane_id?, source, lines?}` — read recent output. `source` is required (e.g.
  `recent`, `visible`).

### Wait / synchronize
- `pane_wait_for_output {pattern, pane_id?, timeout_ms?}` — block until specific text appears
  in a pane. Use for "wait for this prompt / this build to finish."
- `events_wait {subscriptions, timeout_ms?}` — block until an event fires, e.g. an **agent
  reaching a status**. Use this to wait for an agent to go `done` or `blocked` rather than
  polling. `timeout_ms` bounds the wait.

### Clean up
- `pane_close` / `tab_close` / `workspace_close` — tear down when work is finished. Remember
  ids compact afterward.

## Conventions & gotchas

- **Addressing agents:** always by `target` — a `terminal_id`, a `pane_id`, or an agent name
  *if it's unambiguous* (a bare name like `claude` errors when several are running; use the
  id from `agent_list`).
- **Errors are structured:** a failed call returns `is_error` with
  `{"error":{"kind","code","message","method"}}`. `kind:"request"` means your arguments were
  rejected before reaching herdr (fix the call); `kind:"api"` is herdr's own error;
  `kind:"transport"` means herdr was unreachable.
- **Destructive tools** (close/remove/stop/…) are flagged with a `destructiveHint`
  annotation — confirm intent before firing them.
- **Surface limits:** you only see the tools your configured profile exposes. If a tool you
  expect is missing, it's been withheld by profile/policy — not a bug.
- **`herdr_call {method, params}`** (only if enabled) is a raw escape hatch for methods not
  exposed directly — e.g. after a herdr upgrade adds new ones.

## A typical flow

1. `workspace_list` / `agent_list` — see what already exists.
2. `create_workspace` (or reuse one) → `start_agent {name: "claude"}` per task; for parallel
   branch work, `worktree_create` then `start_agent` in each.
3. `agent_send` each agent its task.
4. `events_wait` for agents to reach `done` (or `blocked`).
5. On `blocked`: `agent_read` to see what it needs, then `agent_send` the answer.
6. When all `done`: collect results via `agent_read`, then `pane_close` / `workspace_close`.
