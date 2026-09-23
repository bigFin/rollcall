# rollcall

A terminal picker for coding-agent sessions on your machine and over SSH.
Find a session, preview its output, and open it in the agent's own terminal.
Supports Codex, Oh My Pi, Pi, Hermes, Claude Code, and Antigravity CLI (`agy`).

## Install

With a recent Rust toolchain:

```sh
git clone https://github.com/bigFin/rollcall.git
cd rollcall
cargo install --path . --locked
```

For a pinned toolchain, run `nix develop` before installing. Make sure Cargo's
bin directory (usually `~/.cargo/bin`) is on your `PATH`.

Hosts need tmux and the relevant agent CLI. SSH access uses OpenSSH; Pi, Hermes,
Claude, and Antigravity discovery also need Python 3. Live ownership checks use
Linux `/proc`. Remote hosts do not need Rollcall installed.

## Use

Launch agents normally, preferably inside tmux, then open Rollcall:

```sh
rollcall                        # interactive picker
rollcall hosts                  # hosts from ~/.ssh/config
rollcall list --host HOST        # sessions on one SSH host
rollcall history --search QUERY  # search the local cache
rollcall watch                  # watch activity and send notifications
```

The picker loads cached sessions first, then refreshes hosts. Local sessions
come first, grouped by **host → project → session**. Columns keep titles,
harnesses, activity, last-interaction times, and messages aligned.

Use arrows or `j`/`k` to move, `/` to search, and `Enter` to open a session.
`Tab` folds groups, `h` chooses a host, `p` previews output, and `i` shows details.
`a` archives or restores a session without deleting its history. Press `?` for
all keys or `q` to quit. Search includes archived sessions.

**24h** and **7d** count sessions with recent interactions, including live and
archived sessions; the totals overlap. **Hosts: online** counts machines, not sessions.
If a session may already be open elsewhere, Rollcall refuses to start another
frontend unless you explicitly request `rollcall resume SESSION_ID`.

## Tmux and colors

Run `rollcall popup` inside tmux, or bind it to prefix-k:

```tmux
bind-key k run-shell -b 'ROLLCALL_TMUX_CLIENT=#{q:client_name} rollcall popup'
```

Use `popup`, not a manually wrapped `pick`: it closes the overlay before
attaching and targets the client that opened it.

To put future agent launches inside tmux automatically, add this to Bash/Zsh:

```sh
eval "$(rollcall shell-init)"
```

Rollcall uses your terminal colors by default. Set `ROLLCALL_THEME=everforest`
for Everforest Dark or `ROLLCALL_THEME=tmux` to use tmux's colors. Reopen the
picker after changing themes. Tmux bindings use the server's environment.

## Details

- [Adapter setup and limits](docs/native-adapters.md): Pi's optional live-status
  extension, Hermes profiles, Claude ownership limits, and Antigravity CLI support.
- [Notifications](docs/architecture.md#notifications-and-watching) and
  [cache storage](docs/architecture.md#persistence).
- [Architecture and development](docs/architecture.md).

Run `rollcall --help` for commands and options. `--json` prints structured output.
