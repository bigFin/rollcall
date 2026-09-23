# Pi and Hermes

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

## Host requirements

These adapters require **Python 3 on each probed host**; no Rollcall helper or
extra Python packages are needed remotely. Live ownership checks require Linux
`/proc`. Other Unix hosts support saved inventory and explicit resume, but not
verified live attachment. The Pi extension tests additionally require Node.js
24+, supplied by the development shell.
