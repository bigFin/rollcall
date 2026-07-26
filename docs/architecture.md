# Architecture

## Runtime Shape

The current slice is one binary that probes hosts on demand:

```text
rollcall                    interactive session picker
rollcall popup              picker in a tmux popup
rollcall list --json        scriptable inventory client
rollcall attach <session>   native terminal handoff
```

The current picker needs no Rollcall daemon. It loads its SQLite materialized
view, starts short-lived background probes, and keeps refreshing while the
picker is open. Codex itself has one lazily started app-server per host and
`CODEX_HOME`; Rollcall contains it in tmux and communicates over a Unix socket.
A later observation layer may add a lazily started local broker for
notifications and monitoring between picker invocations. If added, it should
communicate over a Unix-domain socket and exit after an idle period rather than
becoming mandatory infrastructure.

Remote hosts require only their existing SSH server, tmux installation when
used, and the coding-agent CLIs already present there. Probes and protocol
clients are launched over SSH and live only while they are needed.

## Normalized Session Model

Harness-specific adapters normalize only:

- Discovery metadata.
- Runtime and attention state.
- Native attach and resume commands.
- Optional read-only text extraction for search.

The control plane does not normalize conversation rendering, tools, subagents,
approvals UI, model selection, or other harness-native behavior.

Runtime state and attention state are independent:

```text
owner:     resumable | shared-backend | external-frontend | tmux-frontend
runtime:   offline | connecting | idle | working | failed | unknown
attention: none | completed | approval | input | plan-ready
```

This permits a session to be working while awaiting approval, offline with an
unseen completion, or idle without requiring attention.

## Observation Strategy

Efficient observation follows one rule:

> Snapshots establish truth, events maintain it, and polling repairs it.

Each host receives an initial inventory snapshot. Native event streams are used
when available. Polling frequency adapts to whether the host is active,
reachable but idle, or offline. Reconnection triggers reconciliation before
new notifications are emitted.

The current implementation applies the snapshot portion progressively:

1. Load cached SQLite rows and open the TUI immediately.
2. Probe every unique SSH endpoint in parallel.
3. Publish the fast `thread/list` plus live-process correlation result.
4. Fetch expensive `thread/read` details in the background.
5. Skip detail reads for unchanged sessions that already have a cached message.
6. Poll lifecycle state every two seconds locally and every five seconds
   remotely, only on reachable hosts that currently contain loaded threads.
7. Reconcile selected, local, working, and attention-bearing hosts every minute
   while online; reconcile idle hosts every three minutes.
8. Retry unavailable important hosts on an approximately `2, 5, 10, 20, 30s`
   curve and idle hosts on `5, 15, 30, 60, 120s`, with deterministic jitter.
9. Pause automatic retries for authentication and host-key failures until the
   user explicitly requests a retry.

Offline hosts never block local or reachable-host navigation.

## Host Inventory

The initial host inventory is derived from literal aliases in the user's
OpenSSH configuration. `Include` files are followed, while wildcard and
negated `Host` patterns are retained as SSH policy rather than presented as
selectable hosts.

Effective connection settings are delegated back to OpenSSH through `ssh -G`.
This is local configuration evaluation, not a network connection. A later
broker will cache the resulting inventory and only recompute it when relevant
configuration files change.

Aliases with the same effective `(hostname, user, port)` are one discovery
target. The shortest literal alias is used as the canonical probe name.

## Codex Adapter

Codex threads, rather than tmux sessions, are the primary entities in the
current active projection. The adapter ensures one
`codex app-server --listen unix://...` runtime per host and `CODEX_HOME`, then
requests `thread/list` over WebSocket. Remote clients reach the same Unix socket
through a temporary SSH stream-local forward. Rollcall uses the native thread
ID, title/preview, cwd, source, and timestamps without reading Codex's private
SQLite schema directly.

The runtime is namespaced by a checksum of `CODEX_HOME`:

```text
tmux:   rc-codex-PROFILE
socket: $XDG_RUNTIME_DIR/rollcall/codex-PROFILE.sock
```

Startup is race-tolerant: concurrent clients wait for an existing launch,
identify stale tmux sessions by tmux session ID before removing them, and let
one `tmux new-session` winner create the socket. SSH forwards use keepalives and
are removed when the client disconnects.

The user-facing chronology is `recencyAt`, which represents native thread
interaction recency. `updatedAt` is retained separately because broader
session activity can update it without representing the user's last
interaction.

Live process correlation is a read-only Linux probe:

1. Enumerate tmux panes and their root PIDs.
2. Walk each pane's process descendants through `/proc`.
3. Inspect live Codex processes for open rollout files.
4. Extract the native thread ID from the rollout filename.
5. Inspect the process command line to distinguish Rollcall's app-server
   backend, another frontend's app-server or CLI, and an attachable tmux TUI.
6. For a Rollcall-managed `codex --remote ... resume THREAD_ID` TUI, recover the
   native thread ID directly from its command line because the remote client
   does not hold the rollout file open itself.
7. Associate a terminal frontend with its exact tmux session and pane.
8. Read the latest rollout lifecycle event in reverse:
   `task_started` means working, while `task_complete` and `turn_aborted` mean
   completed.

Ownership precedence is attachable tmux frontend, external frontend, then
Rollcall shared backend. A backend-only thread can safely receive a terminal
frontend through the existing socket. An externally owned thread remains
visible and monitored but is not reopened. This prevents Rollcall from starting
a competing native client while keeping its own backend from being mistaken for
an attachable pane.

The probe is executed over SSH for remote hosts and does not require a resident
helper. While the picker is open, the same bounded probe runs every two seconds
locally and every five seconds remotely, only for hosts with loaded sessions.
GNU `tac` is preferred for efficient reverse reads, with `tail -r` and a bounded
tail scan as portable fallbacks.

Codex control-plane IDs have the form `HOST:codex:THREAD_ID`.

## Tmux Adapter

Tmux is the native process container and terminal handoff mechanism, not the
primary session model. Raw inventory remains available for diagnostics. Local
inventory comes from `tmux list-sessions`; a selected remote host is queried
with the same command over SSH.

Raw tmux IDs have the form `HOST:tmux:NAME`. Attachment remains native:

- Inside local tmux, switch the current client.
- Outside local tmux, attach normally.
- For a remote session, allocate a terminal over SSH and run tmux attach there.

When attaching to a live Codex thread, Rollcall selects the exact correlated
pane before attaching. When resuming an unloaded thread, Rollcall atomically
creates or reuses a deterministic tmux session and launches
`codex --remote unix://SOCKET resume <THREAD_ID>` in the thread cwd. Rollcall's
inventory client and resumed terminal TUIs converge on the same internal
app-server. Other frontends remain independent and are reported distinctly.

The picker can capture a bounded tail of an attachable pane on demand. Local
capture uses `tmux capture-pane`; remote capture runs the same command through a
short, noninteractive SSH request. Capture is asynchronous and happens only
when the user opens a preview, so Rollcall does not continuously scrape terminal
contents. Sessions without an attachable pane use their cached response and
metadata instead.

An external process is not reopened implicitly because an ordinary Codex CLI,
T3 Code app-server, desktop application, or IDE process does not expose a
portable terminal attachment point. The shell shim prevents this for future
terminal launches by placing the native process in a temporary
`rc-pending-*` tmux session before the native thread ID exists. Discovery later
renames that same tmux session to the deterministic `rc-PROJECT-SUFFIX` name.
Rollcall never claims to have re-parented an already-running process.

The experimental native Codex app-server daemon is not adopted in this slice.
Its bootstrap installs durable user-service management for remote clients,
which conflicts with Rollcall's no-required-remote-service boundary, and
independent frontends do not automatically converge on it. Rollcall can adopt a
future standard endpoint opportunistically if Codex makes it the common
frontend rendezvous.

Tmux activity timestamps are deliberately not used as coding-agent interaction
timestamps.

## Persistence

SQLite holds:

- The current materialized host and session projections.
- Semantic lifecycle events.
- User state such as acknowledgement, pinning, and archive status.
- Search documents and incremental artifact cursors.

Session snapshots and archive classification are implemented. A refresh updates
native metadata without clearing the user's archive choice. Cached sessions
remain queryable when their host is unavailable, but their live tmux binding is
discarded until a successful probe confirms it again.

Heartbeats and unchanged snapshots are not retained as events. Archiving is a
control-plane classification and does not delete native session artifacts.

## Archive Projection

Archive state belongs to Rollcall rather than tmux or a coding-agent harness.
Archiving removes a session from the default active projection but preserves
its last snapshot, native identity, timestamps, and resume path.

The persistence layer supports manual archive and restore from the picker.
Completed and unknown sessions are automatically settled after seven days
without native interaction. Working, approval, input, and failed-attention
sessions are exempt. Auto-settled sessions wake when native activity advances;
manual archives do not. A manual restore pins that exact stale interaction in
the active view until activity advances, preventing an immediate re-settle.

Future extensions should add:

- Archiving the session containing the current tmux client.
- A user-configurable stale-after duration.
- A full-text index and richer ranking for very large histories.

Automatic staleness must not destroy native artifacts. Archive and deletion are
separate operations, and deletion is outside the initial scope.

## Terminal Handoff

Selecting a session exits the overlay and executes the native attachment path:

```text
local live agent pane       -> tmux select-pane + switch-client
remote live agent pane      -> ssh -t host tmux select-pane + attach-session
local persisted thread      -> tmux new-session -A + codex --remote ... resume
remote persisted thread     -> ssh -t host tmux new-session -A + codex --remote ... resume
shared backend only         -> tmux new-session -A + codex --remote ... resume
external frontend           -> explain; do not duplicate the native process
future shell launch         -> shim starts directly inside rc-pending-* tmux
```

`rollcall` does not embed or proxy the agent terminal.

## Picker

The picker is deliberately a transient control surface rather than a second
agent UI. It can own the current terminal briefly or run inside
`tmux display-popup`, but selection always exits the alternate screen before
executing the native handoff.

The default projection combines all configured hosts and groups rows beneath
`directory / host` headings. The host menu filters that shared projection.
Active and Settled tabs use the same grouping and search behavior.

Session rows show only the activity marker, native title, latest agent message,
and Codex interaction age. Working markers pulse; completed rows use a quiet
dim checkmark. Runtime, full path, and native identity are progressively
disclosed through the optional detail strip. A loaded-outside-tmux row is
visible for awareness and follows the explicit reopen policy above.

An on-demand preview overlay shows the newest portion of a live tmux pane when
one is safely attachable, otherwise the last cached response. It supports
scrolling, attach/resume, and settle/restore without turning Rollcall into a
conversation renderer. Capture failures retain the cached fallback and explain
the live error.

Ordinary text deliberately uses the terminal's default foreground/background.
Secondary metadata uses the terminal `DIM` attribute, and selected rows use
reverse video. Accent colors are reserved for lifecycle and connectivity state,
which avoids unreadable hard-coded dark text on transparent terminal themes.
