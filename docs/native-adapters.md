# Native adapters

The Python-backed adapters cover Pi, Hermes, Claude Code, and Antigravity CLI.
Discovery reads existing files; it does not launch an agent or modify its data.

## Pi sessions

Pi and Oh My Pi are separate adapters and IDs (`HOST:pi:ID` versus
`HOST:omp:ID`). Pi discovery reads `~/.pi/agent/sessions/`, including session
names and messages beyond the header. It honors `PI_CODING_AGENT_DIR`,
`PI_CODING_AGENT_SESSION_DIR`, and the agent's `sessionDir` setting. Custom
session directories are flat. Resume uses the exact JSONL path, not a fuzzy ID.

## Pi live status

For **exact Pi live status and tmux attachment**, load the bundled lifecycle
extension on each host:

```sh
pi --extension /path/to/rollcall/integrations/pi/rollcall.ts
```

For persistent loading, add that absolute path to Pi's user `extensions` setting
and reload Pi. Nix packages also install it at
`$out/share/rollcall/pi/rollcall.ts`. The extension publishes only PID, process
start identity, session ID/path, and activity under
`$XDG_CACHE_HOME/rollcall/pi` (default `~/.cache/rollcall/pi`). It does not write
prompts or message text, and removes its record on shutdown. Valid live records
also make sessions in per-invocation `--session-dir` locations discoverable.

Without the extension, saved Pi sessions are still listed. A running bare Pi
process cannot reliably be mapped to a session: Rollcall refuses an implicit
resume when ownership in that working directory is uncertain, rather than
attaching the newest file to an unrelated terminal. `rollcall resume` remains
an explicit request to open a frontend.

## Hermes profiles and ownership

Hermes discovery reads `state.db` **read-only**, without importing Hermes or
running migrations. By default it searches `~/.hermes` and its named profiles;
`HERMES_HOME` restricts discovery to that home. IDs include the profile to avoid
collisions, for example `HOST:hermes:main/SESSION_ID`. The `nativeSessionId`
field is profile-qualified for Hermes. Resume selects the original profile and
passes the unqualified ID to Hermes. Custom homes get a stable path-derived
namespace and are resumed through `HERMES_HOME`.

Hermes live ownership comes from `runtime/active_sessions.json`, with PID and
process-start validation. Only a CLI lease with a proven tmux ancestor is
attachable; desktop and shared gateway sessions remain external. Hermes itself
may refuse explicit resume while another frontend owns the session.

## Claude Code

IDs use `HOST:claude:SESSION_UUID`. Discovery streams the main transcripts in
`~/.claude/projects/*/*.jsonl`, or the corresponding `CLAUDE_CONFIG_DIR`.
It reads custom titles, summaries, message text, working directories, and native
message timestamps. Nested subagents, sidechains, and orphaned/superseded copies
are excluded. Partial JSONL writes do not discard the rest of a transcript.

Resume runs `claude --resume SESSION_UUID` in the recorded working directory,
preserving the discovered configuration directory. A missing working directory
is not replaced with a guess.

**Live Claude ownership and activity are not yet verified.** A running Claude
process blocks implicit resume across Claude sessions: its initial argv and cwd
are not proof of which conversation it currently owns. Use the existing
terminal, or explicitly request `rollcall resume HOST:claude:SESSION_UUID`.
No Claude hooks or settings are installed automatically.

References: [Claude storage layout](https://code.claude.com/docs/en/claude-directory)
and [CLI resume](https://code.claude.com/docs/en/cli-reference).

## Antigravity CLI (`agy`)

IDs use `HOST:agy:CONVERSATION_UUID`. Discovery opens
`~/.gemini/antigravity-cli/conversation_summaries.db` read-only and projects its
metadata columns. It checks that the corresponding `conversations/ID.db` or
`ID.pb` still exists, but does not decode the private trajectory payload.
Child conversations, stale summaries without backing files, and IDE-owned
records are excluded. This adapter targets the **`agy` CLI**, not the IDE.

The native title (or summary preview) supplies the display title. Native summary
and user-input timestamps supply recency. The latest-message column is blank:
a summary preview is not proof of the last assistant message. Activity remains
unknown rather than treating a saved RUNNING flag as evidence of current work.
Workspace URIs are decoded as local file paths, with a fallback to the named
project's `gitFolder.folderUri` metadata under `~/.gemini/config/projects/`.

Resume runs `agy --conversation CONVERSATION_UUID`, with `--project PROJECT_ID`
when recorded, from the original workspace. These flags were checked against
local `agy --help`. Unknown workspaces prevent resume rather than choosing a
potentially unrelated directory.

On Linux, a **held** native `presence/ID.lock` can establish current ownership.
The probe verifies the kernel lock against the exact open file, not merely a
leftover lock file or matching inode. Only a recognized CLI process with a
terminal input and a proven tmux ancestor is attachable. Other owners stay
external; unmatched CLI processes block implicit resume. Explicit resume remains
available, subject to Antigravity's own checks. The summary and presence formats
are implementation details observed in September 2026, not a stable public API.

## Host requirements

These adapters require **Python 3 on each probed host**; no Rollcall helper or
extra Python packages are needed remotely. Live ownership checks require Linux
`/proc`. Other Unix hosts support saved inventory and explicit resume, but not
verified live attachment. The Pi extension tests additionally require Node.js
24+, supplied by the development shell.
