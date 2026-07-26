# rollcall

`rollcall` is an SSH-native control plane for discovering, monitoring, searching,
archiving, and resuming coding-agent CLI sessions across intermittently
available hosts.

The native terminal remains the workspace:

```text
start new work: ssh host -> cd project -> launch the coding agent normally
continue work:  rollcall -> select an existing session -> attach or resume
```

`rollcall` does not replace tmux or render another agent interface. It maintains
awareness of where sessions live, whether they are working or need attention,
and how to enter them using their native CLI.

## Name

Rollcall is fairly literal: ask the known hosts which sessions are present, then
jump back into one. `F2 Code` was an early, mildly cheeky alternative—a nod to
T3 Code and to Fin. We may still find a home for F2 as the tmux binding or a
nickname, but `rollcall` is the command.

## Status

The first Codex-first slice works. Rollcall discovers resumable Codex threads
across the local machine and configured SSH hosts, sorts them by native
interaction recency, distinguishes attachable tmux frontends from external
frontends and its own backend, and hands the terminal back to the exact tmux
pane or to `codex resume`.

The picker opens immediately from its SQLite snapshot, then refreshes each
unique SSH endpoint through a bounded scheduler. Basic session rows arrive
first; last-agent messages fill in afterward without blocking the interface.
Active and Settled views, reversible archive state, status indicators, search,
host filtering, live lifecycle polling, adaptive reconnection, seven-day
automatic settling, offline cached sessions, and on-demand session previews are
implemented. Meaningful lifecycle transitions also create persistent unread
attention and local notifications while Rollcall is running.

Codex inventory and resumed TUIs now converge on one lazily started app-server
per host and `CODEX_HOME`. Additional coding-agent adapters, background
observation while every Rollcall process is closed, configurable stale-after
rules, and the optional local broker remain ahead.

## Product Boundary

`rollcall` owns:

- SSH-host discovery and connectivity state.
- Coding-agent session discovery.
- Runtime and attention status.
- Native attach, reconnect, and resume actions.
- Active, historical, and archived projections.
- Search indexing and notifications.

`rollcall` does not own:

- Starting new coding-agent sessions.
- Conversation rendering or prompt submission.
- Agent configuration, tools, subagents, or models.
- Worktree or project orchestration.
- A required service or installed helper on remote hosts.

## Current Interface

```console
rollcall
rollcall pick [--host HOST] [--limit N]
rollcall popup [--host HOST] [--limit N]
rollcall list [--host HOST] [--limit N | --all]
rollcall history [--host HOST] [--search QUERY] [--limit N]
rollcall hosts [--config PATH]
rollcall attach <session>
rollcall watch [--host HOST] [--limit N] [--once]
rollcall shell-init [AGENT...]
rollcall tmux [--host HOST]
```

Running `rollcall` interactively opens the picker. When stdout is redirected, or
when `--json` is supplied, the no-subcommand form retains the scriptable list
behavior. Inventory commands support structured JSON output.

The default picker is an all-host dashboard grouped by `directory / host`.
Nested rows show activity state, session title, latest agent message, native
interaction age. Runtime and native identifiers stay out of the default view
and are available in the optional detail strip. Cached sessions remain visible
while a host is offline.

Picker keys:

```text
j/k or arrows  move
enter          attach or resume
p              preview the live tmux pane or cached response
/              filter visible sessions
h              select an SSH host
tab            switch Active/Settled views
a              settle or restore the selected session
x              mark the selected session read
i              show or hide selected-session details
r              refresh or retry all hosts
?              open the key and status legend
q or esc       quit
```

Enter on a Settled row restores it before attaching. Search applies to both
views and matches titles, last messages, directories, hosts, statuses, native
IDs, and tmux bindings. Host selection is a filter over the shared dashboard,
not a destructive reload. Press `p` to inspect recent output from an attachable
tmux pane without leaving the picker; resumable, external, offline, and
backend-only rows fall back to their cached response and metadata. The preview
captures only when opened rather than polling pane contents continuously.
Within it, `Enter` opens the session, `a` settles or restores it, `j/k` scrolls,
and `Esc` closes it. Press `i` at normal terminal heights to show a compact
selected-session strip without making every dashboard row taller by default.

## Attention and Notifications

Rollcall records meaningful session transitions rather than treating every
poll as an event:

- working, approval, or input to completed;
- a new approval or input request;
- a transition into failure.

Those transitions mark the session unread, move its group toward the top of the
picker, and add a yellow dot to its row. Unread sessions are not automatically
settled. Press `x` to mark one read; attaching to or manually settling it also
acknowledges the notification.

While Rollcall is running, transitions and host recovery emit a terminal bell
by default. Set `ROLLCALL_NOTIFY_COMMAND` to run a local notification command
instead. The hook receives data through environment variables, so session text
does not need to be interpolated into shell source:

```console
export ROLLCALL_NOTIFY_COMMAND='notify-send "$ROLLCALL_NOTIFICATION_TITLE" "$ROLLCALL_NOTIFICATION_BODY"'
```

On Android with Termux:API:

```console
export ROLLCALL_NOTIFY_COMMAND='termux-notification --title "$ROLLCALL_NOTIFICATION_TITLE" --content "$ROLLCALL_NOTIFICATION_BODY"'
```

The hook receives `ROLLCALL_NOTIFICATION_KIND`,
`ROLLCALL_NOTIFICATION_TITLE`, `ROLLCALL_NOTIFICATION_BODY`,
`ROLLCALL_NOTIFICATION_HOST`, and `ROLLCALL_NOTIFICATION_SESSION_ID`. Set the
command to an empty value to disable both the hook and default bell.

There is still no background daemon. If a turn finishes while Rollcall is
closed, it becomes unread the next time a picker refresh can compare the new
native state with its previous snapshot.

For notification monitoring without keeping the picker open, run the foreground
observer:

```console
rollcall watch
rollcall watch --host coda
rollcall --json watch
```

It uses the same bounded host scheduler, live lifecycle polling, adaptive
reconnection, persistent unread state, and notification hook as the picker.
Events are written as tab-separated lines, or as newline-delimited JSON with
`--json`. The process remains attached to the invoking terminal and stops
normally with `Ctrl-C`; it is not a daemon and installs nothing remotely.

For a cron, timer, or Termux task that should reconcile once and exit:

```console
rollcall watch --once
```

One-shot mode checks each selected host once, emits any newly observed
transitions, updates the cache, and exits after detail and live-state probes
finish. Unavailable hosts remain represented by their existing cached rows
rather than being retried indefinitely in one-shot mode.

From an ordinary shell inside tmux:

```console
rollcall popup
```

Or bind the picker directly as a lightweight overlay:

```tmux
bind-key F2 display-popup -E -w 90% -h 80% 'rollcall pick'
```

Archiving is reversible control-plane state, not deletion. `a` moves a selected
session between Active and Settled while retaining its native identity,
timestamps, cached message, and resume path. Completed or otherwise idle
sessions with no native interaction for seven days are settled automatically.
A manually restored stale session stays active until it receives new native
activity, and new activity automatically wakes a session that Rollcall settled
for staleness. Unread sessions remain Active until acknowledged, and manual
archives stay settled. A configurable threshold and a full-text index remain
planned.

`rollcall history` reads the local SQLite cache without connecting to any host.
It combines Active and Settled sessions in native-interaction chronology and
can filter cached metadata:

```console
rollcall history
rollcall history --search "rollcall ownership"
rollcall history --host coda --limit 20
rollcall --json history --search kubernetes
```

Local state is stored in:

```text
$ROLLCALL_STATE_PATH
$XDG_STATE_HOME/rollcall/rollcall.db
~/.local/state/rollcall/rollcall.db
```

The first defined path wins. `ROLLCALL_STATE_PATH` is useful for isolated
testing.

## SSH Host Discovery

`rollcall hosts` reads literal `Host` aliases from `~/.ssh/config` and its
`Include` files. Wildcard and negated patterns are configuration rules rather
than selectable hosts, so they are intentionally omitted.

For each alias, the local OpenSSH client resolves the effective hostname, user,
port, and identity files with `ssh -G`. This does not connect to the host and
does not require anything to be installed remotely. Aliases resolving to the
same `(hostname, user, port)` are probed once using the shortest alias, avoiding
duplicate work for short and fully qualified names.

Use a different user configuration when testing or scripting:

```console
rollcall hosts --config ./fixtures/ssh-config
rollcall hosts --config ./fixtures/ssh-config --json
```

## Codex Sessions

Running `rollcall` opens the interactive picker for resumable threads from the
local Codex state:

```console
rollcall
rollcall pick
rollcall popup
```

Use the explicit list command for textual output:

```console
rollcall list
rollcall --json
rollcall list --all
```

Inspect one configured SSH host without installing Rollcall or a daemon there:

```console
rollcall list --host coda
rollcall list --host coda --json
```

The Codex adapter connects to one lazily started `codex app-server` Unix socket
per host and `CODEX_HOME`. Rollcall contains that native server in a
`rc-codex-PROFILE` tmux session, reuses it across inventory clients, and opens a
temporary SSH Unix-socket forward when the host is remote. A remote host
therefore needs SSH, Codex, and tmux, but it does not need Rollcall or a
separately installed service.

Session state in the picker means:

- `resumable`: persisted by Codex and not currently loaded.
- `backend`: loaded only by Rollcall's internal shared app-server and safe to
  open in a terminal frontend.
- `external`: owned by an independent frontend such as an IDE, desktop
  application, or unmanaged CLI.
- `tmux`: owned by an attachable terminal frontend in the named tmux session.

Working and completed state comes from the live Codex rollout lifecycle:
Rollcall observes the latest `task_started`, `task_complete`, or `turn_aborted`
event for each loaded thread. Approval and input states remain best-effort
native app-server metadata.

- `working`
- `approval`
- `input`
- `completed`
- `failed`

Loaded threads are polled every two seconds locally and every five seconds on
remote hosts while the picker is open, and the working marker visibly pulses
on every redraw. This lightweight probe runs only for reachable hosts with
loaded sessions.

Complete inventory reconciliation is scheduled per host rather than by one
global timer. Selected, local, working, and attention-bearing hosts retry on an
approximately `2, 5, 10, 20, 30s` curve while unavailable; idle hosts relax
toward two minutes. Successful important hosts reconcile every minute and idle
hosts every three minutes. Retries include jitter, at most four host refreshes
run concurrently, and authentication or host-key failures pause until a manual
`r`. Cached rows remain visible with an offline countdown, and a recovered host
is reconciled before Rollcall reports it online.

Selecting a `tmux` row attaches to its exact pane. Selecting a `resumable` or
`backend` row creates or reuses a deterministic `rc-PROJECT-XXXXXXXX` tmux
session on that host and runs `codex --remote unix://SOCKET resume <id>` against
Rollcall's internal app-server. Repeated selection reuses that tmux container
rather than adding more sessions.

An external frontend cannot be moved into tmux portably. Enter therefore leaves
an externally owned thread alone and explains why instead of starting a
competing Codex process. Once that frontend releases the thread, the next
inventory reconciliation makes it resumable normally.

For future shell launches, enable the transparent shim:

```console
eval "$(rollcall shell-init)"
```

This defines Bash/Zsh functions for `codex`, `claude`, and `pi`. From an
interactive shell outside tmux, the native executable starts in a temporary
`rc-pending-*` tmux session. Inside tmux, in scripts, without tmux installed, or
with `ROLLCALL_BYPASS=1`, the command runs normally. Once Codex exposes its
native thread ID, Rollcall renames the temporary container to the stable
`rc-PROJECT-XXXXXXXX` name. Nothing new needs to be installed on a remote host
besides tmux and the agent CLI.

The shim makes future terminal processes attachable, but Rollcall deliberately
does not require T3 Code, an IDE, or another frontend to know that Rollcall
exists. Those applications remain external owners discovered through native
Codex state and process observation.

Codex 0.145.0 also exposes experimental `app-server daemon` and
`app-server proxy` commands. The daemon bootstrap installs durable user-service
management intended for remote Codex clients, so Rollcall does not bootstrap or
depend on it: doing so would violate the zero-install remote-host boundary and
would not make unrelated frontends adopt that daemon automatically. If Codex
later standardizes one automatically shared local endpoint for every frontend,
Rollcall can discover and adopt it without adding a T3-specific integration.

The picker uses the terminal's default foreground and background for ordinary
text, `DIM` for secondary metadata, and reverse video for selection. This keeps
thread titles readable with dark, light, and transparent terminal themes
without Rollcall guessing the background color.

```console
rollcall attach topo:codex:019f9631-f91c-7f20-95f3-019a0ba62241
rollcall attach coda:codex:019f7097-2965-7572-9a6a-0d289c93eab3
```

## Raw Tmux Inventory

Tmux sessions remain available as secondary diagnostic inventory:

```console
rollcall tmux
rollcall tmux --host coda
rollcall attach fabric
rollcall attach coda:tmux:fabric
```

## Development

Enter the pinned development environment:

```console
nix develop
```

Run the primary checks:

```console
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
nix flake check
```

The project is intentionally not published yet. Licensing and remote repository
creation will be settled before the first public release.
