# rollcall

Find and resume **Codex, Oh My Pi, Pi, and Hermes sessions** across your local
machine and SSH hosts. Rollcall shows what is working, what needs attention, and where to pick up.
Your agent's native terminal stays the workspace—no replacement chat UI.

- Opens from a local cache; offline hosts do not block navigation.
- Attaches to the exact tmux pane, or resumes with the native agent CLI.
- Keeps unread attention, searchable history, and reversible archives.
- Uses **[Everforest Dark](https://github.com/sainnhe/everforest)** throughout the picker.

## Install

Build from source with a recent Rust toolchain:

```sh
git clone https://github.com/bigFin/rollcall.git
cd rollcall
cargo install --path . --locked
```

Ensure Cargo's bin directory (normally `~/.cargo/bin`) is on your `PATH`.
You need tmux and the relevant agent CLI on hosts running sessions, plus OpenSSH
for remote access. Pi and Hermes discovery also need Python 3 on each host.
Live process discovery currently uses Linux `/proc`.
Remote hosts do **not** need Rollcall or a separately installed Rollcall service.

## Start here

Launch agents normally in your project directories, preferably inside tmux.
Rollcall discovers existing sessions; it does not start new work.

```sh
rollcall                        # open the all-host picker
rollcall hosts                  # inspect aliases from ~/.ssh/config
rollcall list --host HOST        # inspect one configured SSH host
rollcall history --search QUERY  # search the cache without contacting hosts
rollcall watch                  # foreground notifications and lifecycle events
```

Replace `HOST` with an SSH alias. `Host` and `Include` directives support both
whitespace and `=` separators; wildcard aliases are not selectable hosts.

The dashboard groups sessions by **activity/recency → host → project**.
Cached sessions remain visible while a host is offline. Use a truecolor-capable
terminal, including tmux passthrough, for Everforest's RGB palette. There is no
theme selector yet.

| Key | Action |
| --- | --- |
| `j` / `k`, arrows | Move |
| `Enter` | Open a session, or toggle a group/section |
| `Tab`, `Space` | Expand/collapse a group or section |
| `/` | Search sessions, messages, paths, and metadata |
| `h` | Choose a host filter |
| `[` / `]`, `0` | Previous/next host; all hosts |
| `p`, `i` | Preview output; toggle session details |
| `a`, `x` | Settle/restore; mark read |
| `r` | Refresh or retry hosts |
| `?` | Full key reference |
| `q`, `Esc` | Close an overlay or quit |

Settling does **not** delete native sessions. Idle sessions settle automatically
after seven days; unread sessions are exempt. Search includes archived sessions.

**Ownership safety:** `Enter` and `rollcall attach SESSION_ID` leave sessions
owned by external frontends alone. `rollcall resume SESSION_ID` explicitly starts
a new managed frontend even if another frontend is active. Enter on an archived
session restores it before opening it.

## Shell and tmux integration

Optional Bash/Zsh wrappers make future agent launches attachable by starting them
inside tmux when needed. Add this to your shell configuration:

```sh
eval "$(rollcall shell-init)"
```

The wrappers cover `codex`, `claude`, `pi`, `omp`, and `hermes`; wrapping a command does not
add a discovery adapter for it. Inside tmux or in scripts they run normally.
Use `ROLLCALL_BYPASS=1` to bypass wrapping.

For a picker overlay, run `rollcall popup` or add this to `~/.tmux.conf`:

```tmux
bind-key F2 display-popup -E -w 90% -h 80% 'rollcall pick'
```

## Pi and Hermes

Pi and Oh My Pi have separate inventories. Saved Pi sessions are discovered
without extra setup; exact live status and tmux attachment need the optional
[Pi lifecycle extension](docs/native-adapters.md#pi-live-status).

Hermes sessions are read without changing its database. Profile-qualified IDs
keep sessions from different profiles separate, and resume selects the original
profile. See [Pi and Hermes setup](docs/native-adapters.md) for paths, host
requirements, and ownership limitations.

## More

- [Notifications and `watch`](docs/architecture.md#notifications-and-watching):
  desktop/Termux hooks, JSON event streams, and one-shot reconciliation.
- [Cache and history](docs/architecture.md#persistence): state location and offline queries.
- [SSH discovery](docs/architecture.md#host-inventory): aliases, includes, and effective settings.
- [Architecture](docs/architecture.md): adapters, ownership, reconnection, and picker modules.
- [Development checks](docs/architecture.md#development).

Run `rollcall --help` or `rollcall COMMAND --help` for command options. `--json`
enables structured inventory output; `watch --json` emits newline-delimited
JSON events. With redirected stdout, bare `rollcall` lists sessions instead of
opening the picker.
