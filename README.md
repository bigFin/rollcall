# rollcall

Find and resume **Codex, Oh My Pi, Pi, and Hermes sessions** on your machine and
across SSH hosts. See what is working, what needs attention, and where you left
off. Open a session in its native terminal rather than another chat interface.

The picker opens from a local cache, then refreshes reachable hosts. Offline
hosts stay visible without blocking navigation. You can search session titles
and last messages, inspect recent output, or archive sessions without deleting
their native history.

## Install

With a recent Rust toolchain:

```sh
git clone https://github.com/bigFin/rollcall.git
cd rollcall
cargo install --path . --locked
```

Or enter the pinned development environment with `nix develop` first. Make sure
Cargo's bin directory (usually `~/.cargo/bin`) is on your `PATH`.

Hosts need the relevant agent CLI and tmux. Remote access uses OpenSSH; Pi and
Hermes discovery also require Python 3 on each host. Live process checks use
Linux `/proc`. You do not need to install Rollcall or run a Rollcall service on
remote hosts.

## Use it

Launch agents normally in your project directories, preferably inside tmux.
Rollcall finds existing sessions; it does not start new work.

```sh
rollcall                        # open the picker
rollcall hosts                  # list aliases from ~/.ssh/config
rollcall list --host HOST        # inspect one host
rollcall history --search QUERY  # search cached sessions without contacting hosts
rollcall watch                  # print lifecycle events and send notifications
```

Replace `HOST` with an SSH alias. Rollcall follows `Include` files and combines
aliases that point to the same SSH endpoint.

Sessions are grouped by **host → project → session**, with the local host first.
Other hosts and project paths stay alphabetical; sessions within a project are
newest first. Archive is collapsed at the bottom.

The table has fixed columns for title, harness, activity, last interaction, and
latest message. Narrow terminals hide message and harness columns first; `i`
and `p` show details and previews. A yellow dot marks unread activity without
moving the project elsewhere in the list.

The header separates session counts from host connectivity. **Live** counts
freshly observed frontends; **24h** and **7d** count sessions with interactions
in those windows, including live, unread, and archived sessions. These totals
overlap and follow the current search and host filter. **Hosts: online** counts
reachable machines, not active sessions.

| Key | Action |
| --- | --- |
| `j` / `k`, arrows | Move |
| `Enter` | Open a session, or toggle a group |
| `Tab`, `Space` | Expand or collapse a group |
| `/` | Search titles, messages, paths, and metadata |
| `h` | Choose a host |
| `[` / `]`, `0` | Previous/next host; all hosts |
| `p`, `i` | Preview output; toggle session details |
| `a`, `x` | Settle/restore; mark read |
| `r` | Refresh or retry hosts |
| `?` | Show all keys |
| `q`, `Esc` | Close an overlay or quit |

Settling a session hides it from the active view; it does not delete anything
from the agent. Idle sessions settle after seven days unless they have unread
notifications. Search includes archived sessions, and opening one restores it.

Opening a session attaches to its existing tmux pane when possible. Rollcall
will not silently start a second frontend for a session already owned elsewhere.
Use `rollcall resume SESSION_ID` only when you deliberately want another
frontend; the agent may still enforce its own ownership rules.

## Appearance

By default, Rollcall uses your terminal's foreground, background, and ANSI
palette. Secondary text is dimmed and selection uses reverse video. It does not
force a dark background or guess whether your terminal is light or dark.

For a fixed color scheme, choose Everforest Dark:

```sh
ROLLCALL_THEME=everforest rollcall
```

To explicitly use the terminal palette:

```sh
ROLLCALL_THEME=terminal rollcall
```

Use `ROLLCALL_THEME=tmux` to borrow colors from tmux instead. It reads
`popup-style` (falling back to `status-style`), `popup-border-style`, and the
current/activity/bell window styles. Missing colors keep their terminal
defaults. Outside tmux, this mode behaves like `terminal`.

Export `ROLLCALL_THEME` in your shell configuration to keep a preference.
Everforest uses RGB colors and needs a truecolor-capable terminal. All modes
apply to the dashboard, menus, previews, and `rollcall popup`. Reopen the picker
after changing the setting or tmux styles. Custom theme files are not supported
yet.

## Shell and tmux

Optional Bash/Zsh wrappers put future agent launches inside tmux when needed:

```sh
eval "$(rollcall shell-init)"
```

They cover `codex`, `claude`, `pi`, `omp`, and `hermes`. Inside tmux or in scripts,
they run the native command normally. Set `ROLLCALL_BYPASS=1` to bypass wrapping.
Wrapping a command does not add a discovery adapter for it—Claude discovery is
not implemented.

Run `rollcall popup` inside tmux for an overlay, or bind it to prefix-k:

```tmux
bind-key k run-shell -b 'ROLLCALL_TMUX_CLIENT=#{q:client_name} rollcall popup'
```

Use the popup command rather than wrapping `rollcall pick` in `display-popup`.
It closes the overlay before attaching and targets the client that opened it.
Remote selections replace that client with SSH; the local session keeps
running. Detaching remotely returns you to the original local session and
socket. If SSH fails, press Enter after reading the error to return. Escape in
the picker cancels without detaching.

Remote tmux commands run through `bash -lc` so the host's login settings,
including `TMUX_TMPDIR` and `PATH`, take effect.

Tmux key bindings use the server's environment. To choose a theme for this
binding, add `ROLLCALL_THEME=tmux` before `rollcall popup` in the command.

## Pi and Hermes

Pi and Oh My Pi have separate inventories. Saved Pi sessions need no extra
setup. For exact live status and tmux attachment, load the optional
[Pi lifecycle extension](docs/native-adapters.md#pi-live-status).

Hermes discovery reads its database without changing it. Session IDs include
the profile, and resume selects that same profile. See
[Pi and Hermes setup](docs/native-adapters.md) for paths and ownership limits.

## More

- [Notifications and `watch`](docs/architecture.md#notifications-and-watching):
  desktop/Termux hooks, JSON events, and one-shot checks.
- [Cache and history](docs/architecture.md#persistence): state location and offline queries.
- [Architecture](docs/architecture.md): adapters, ownership, host refreshes, and picker modules.
- [Development checks](docs/architecture.md#development).

Run `rollcall --help` or `rollcall COMMAND --help` for options. `--json` prints
structured inventory data; `watch --json` prints one event per line. When stdout
is redirected, bare `rollcall` lists sessions instead of opening the picker.
