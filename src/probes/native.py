"""Read-only native inventory. Runs locally or over SSH with Python's stdlib only."""
import datetime
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys
from urllib.parse import unquote, urlsplit
import uuid


def seconds(value):
    try:
        if isinstance(value, str):
            return max(0, int(datetime.datetime.fromisoformat(value.replace("Z", "+00:00")).timestamp()))
        return max(0, int(value or 0))
    except (ValueError, TypeError, OverflowError):
        return 0


def text(content):
    if isinstance(content, list):
        content = "\n".join(x.get("text", "") for x in content if isinstance(x, dict) and x.get("type") == "text")
    return " ".join(content.split())[:2000] if isinstance(content, str) else ""


def read_json(path, default=None):
    try:
        return json.loads(path.read_text())
    except FileNotFoundError:
        return default


def record(agent, native_id, cwd, title, last_message, recency, updated, activity, locator, raw_id, profile=None):
    return dict(
        session=dict(id="", host="", agent=agent, nativeSessionId=native_id,
                     cwd=cwd, title=title, source=agent, lastMessage=last_message,
                     lastInteractionUnixSeconds=recency, updatedUnixSeconds=updated,
                     activity=activity, runtime="resumable"),
        locator=str(locator), rawId=raw_id, profile=profile, ownershipUncertain=False,
    )


def pi_file(path):
    header = None
    title = first_user = last_message = ""
    recency = 0
    activity = "unknown"
    # Stream rather than loading multi-megabyte tool results into a session snapshot.
    with path.open() as stream:
        for line in stream:
            try:
                entry = json.loads(line)
            except ValueError:
                continue  # An append may be in progress.
            if not isinstance(entry, dict):
                continue
            kind = entry.get("type")
            if kind == "session":
                header = entry
            elif kind == "session_info" and entry.get("name"):
                title = text(entry["name"])
            elif kind == "message":
                message = entry.get("message") or {}
                role = message.get("role")
                if role not in ("user", "assistant", "toolResult", "bashExecution"):
                    continue
                stamp = seconds(entry.get("timestamp")) or seconds((message.get("timestamp") or 0) / 1000)
                recency = max(recency, stamp)
                if role == "user":
                    first_user = first_user or text(message.get("content"))
                    activity = "unknown"
                elif role == "assistant":
                    last_message = text(message.get("content")) or last_message
                    stop = message.get("stopReason")
                    activity = "failed" if stop == "error" else "completed" if stop in ("stop", "length", "aborted") else "unknown"
                elif role == "toolResult":
                    activity = "unknown"
    if not header or not header.get("id"):
        return None
    cwd = header.get("cwd") or str(Path.home())
    updated = int(path.stat().st_mtime)
    return record("pi", header["id"], cwd, title or first_user or cwd, last_message,
                  recency or seconds(header.get("timestamp")) or updated, updated,
                  activity, path.resolve(), header["id"])


def pi_inventory(live_states=(), live_only=False):
    agent_dir = Path(os.environ.get("PI_CODING_AGENT_DIR") or Path.home() / ".pi/agent").expanduser()
    settings = read_json(agent_dir / "settings.json", {})
    override = os.environ.get("PI_CODING_AGENT_SESSION_DIR") or settings.get("sessionDir")
    root = Path(override).expanduser() if override else agent_dir / "sessions"
    # A custom session directory is flat; the default is grouped by project.
    paths = set() if live_only else set(root.glob("*.jsonl") if override else root.glob("*/*.jsonl"))
    paths.update(Path(state["sessionFile"]) for state in live_states if state.get("sessionFile"))
    result = []
    for path in paths:
        try:
            row = pi_file(path)
            if row:
                result.append(row)
        except FileNotFoundError:
            continue
    return result


def hermes_homes():
    root = Path.home() / ".hermes"
    override = os.environ.get("HERMES_HOME")
    if override:
        home = Path(override).expanduser().resolve()
        if home == root.resolve():
            return [("default", home)]
        if home.parent == (root / "profiles").resolve():
            return [(home.name, home)]
        return [("home-" + hashlib.sha256(str(home).encode()).hexdigest()[:12], home)]
    return [("default", root)] + [(p.name, p) for p in sorted((root / "profiles").glob("*")) if p.is_dir() and not p.name.startswith(".")]


def hermes_inventory(live_only=False):
    result = []
    for profile, home in hermes_homes():
        path = home / "state.db"
        if not path.is_file():
            continue
        # Do not import Hermes: its SessionDB constructor runs migrations and writes.
        connection = sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True, timeout=2)
        connection.row_factory = sqlite3.Row
        try:
            columns = {r[1] for r in connection.execute("PRAGMA table_info(sessions)")}
            message_columns = {r[1] for r in connection.execute("PRAGMA table_info(messages)")}
            if not {"id", "started_at"} <= columns:
                raise RuntimeError("unsupported Hermes sessions schema: " + str(path))
            # Explicitly project metadata; never load system prompts or provider configuration.
            wanted = ("id", "source", "started_at", "ended_at", "title", "cwd", "last_activity_at", "parent_session_id", "archived", "hidden")
            query = "SELECT " + ", ".join(c if c in columns else "NULL AS " + c for c in wanted) + " FROM sessions"
            params = []
            if live_only:
                entries = read_json(home / "runtime/active_sessions.json", {"entries": []})["entries"]
                params = [entry.get("session_id") for entry in entries]
                if not params:
                    continue
                query += " WHERE id IN (" + ",".join("?" for _ in params) + ")"
            for row in connection.execute(query, params):
                if row["archived"] or row["hidden"] or row["source"] in ("tool", "cron"):
                    continue
                raw_id = row["id"]
                last_message = first_user = ""
                recency = seconds(row["started_at"])
                activity = "unknown"
                if {"session_id", "role", "content", "timestamp"} <= message_columns:
                    active = " AND active = 1" if "active" in message_columns else ""
                    for message in connection.execute(
                        "SELECT role, content, timestamp FROM messages WHERE session_id = ?" + active + " ORDER BY timestamp, id", (raw_id,)
                    ):
                        if message["role"] not in ("user", "assistant", "tool"):
                            continue
                        recency = max(recency, seconds(message["timestamp"]))
                        value = message["content"] or ""
                        try:
                            value = json.loads(value)
                        except ValueError:
                            pass
                        if message["role"] == "user":
                            first_user = first_user or text(value)
                            activity = "unknown"
                        elif message["role"] == "assistant":
                            last_message = text(value) or last_message
                            activity = "completed" if text(value) else "unknown"
                        else:
                            activity = "unknown"
                recency = max(recency, seconds(row["last_activity_at"]))
                result.append(record("hermes", profile + "/" + raw_id,
                                     row["cwd"] or str(Path.home()), text(row["title"]) or first_user or raw_id,
                                     last_message, recency, max(recency, seconds(row["ended_at"])), activity,
                                     home.resolve(), raw_id,
                                     profile if home.resolve() == (Path.home() / ".hermes").resolve() or home.resolve().parent == (Path.home() / ".hermes/profiles").resolve() else None))
        finally:
            connection.close()
    return result


def session_uuid(value):
    try:
        return str(uuid.UUID(value)) == value.lower()
    except (ValueError, TypeError, AttributeError):
        return False


def claude_file(path, root):
    # UUID basenames exclude superseded/orphaned copies and agent-* sidechains.
    if not session_uuid(path.stem):
        return None
    cwd = title = summary = first_user = last_message = ""
    recency = 0
    activity = "unknown"
    seen_message = False
    with path.open(encoding="utf-8", errors="replace") as stream:
        for line in stream:
            try:
                entry = json.loads(line)
            except ValueError:
                continue  # Incomplete append or a damaged line, not a whole lost session.
            if not isinstance(entry, dict) or entry.get("isSidechain"):
                continue
            kind = entry.get("type")
            if kind == "custom-title":
                title = text(entry.get("customTitle")) or title
            elif kind == "summary":
                summary = text(entry.get("summary")) or summary
            elif kind in ("user", "assistant"):
                message = entry.get("message")
                if not isinstance(message, dict):
                    continue
                seen_message = True
                if isinstance(entry.get("cwd"), str) and Path(entry["cwd"]).is_absolute():
                    cwd = entry["cwd"]
                recency = max(recency, seconds(entry.get("timestamp")))
                content = text(message.get("content"))
                if kind == "user":
                    if not entry.get("isMeta") and not entry.get("isCompactSummary"):
                        first_user = first_user or content
                    activity = "unknown"
                else:
                    last_message = content or last_message
                    activity = "completed" if message.get("stop_reason") in ("end_turn", "stop_sequence", "max_tokens") else "unknown"
    if not seen_message:
        return None
    return record("claude", path.stem, cwd, title or summary or first_user or path.stem,
                  last_message, recency, int(path.stat().st_mtime), activity, root.resolve(), path.stem)


def claude_inventory(live_only=False):
    # Without an independently verified identity bridge we do not claim live
    # ownership from argv, the most recent file, or a session-registry PID alone.
    if live_only:
        return []
    root = Path(os.environ.get("CLAUDE_CONFIG_DIR") or Path.home() / ".claude").expanduser()
    rows = {}
    for path in sorted((root / "projects").glob("*/*.jsonl")):
        try:
            row = claude_file(path, root)
        except FileNotFoundError:
            continue
        if row:
            key = row["rawId"]
            previous = rows.get(key)
            if not previous or row["session"]["lastInteractionUnixSeconds"] > previous["session"]["lastInteractionUnixSeconds"]:
                rows[key] = row
    return list(rows.values())


def local_file_uri(value):
    if not isinstance(value, str):
        return ""
    try:
        parsed = urlsplit(value)
    except ValueError:
        return ""
    if parsed.scheme == "file" and parsed.netloc in ("", "localhost"):
        path = unquote(parsed.path)
        if Path(path).is_absolute() and "\x00" not in path:
            return path
    return ""


def agy_workspace(uris, project):
    try:
        values = json.loads(uris or "[]")
    except (ValueError, TypeError):
        values = []
    if isinstance(values, list):
        for value in values:
            if path := local_file_uri(value):
                return path
    # Project IDs are UUIDs (plus the built-in default); never interpolate an
    # arbitrary database field into a path outside this metadata directory.
    if session_uuid(project) or project == "default-cli-project":
        try:
            data = read_json(Path.home() / ".gemini/config/projects" / (project + ".json"), {})
            for resource in data.get("projectResources", {}).get("resources", []):
                if path := local_file_uri(resource.get("gitFolder", {}).get("folderUri")):
                    return path
        except (OSError, ValueError, TypeError, AttributeError):
            pass
    return ""


def agy_inventory(live_only=False, live_ids=()):
    root = Path.home() / ".gemini/antigravity-cli"
    path = root / "conversation_summaries.db"
    if not path.is_file() or (live_only and not live_ids):
        return []
    connection = sqlite3.connect(path.resolve().as_uri() + "?mode=ro", uri=True, timeout=2)
    connection.row_factory = sqlite3.Row
    rows = []
    try:
        columns = {r[1] for r in connection.execute("PRAGMA table_info(conversation_summaries)")}
        if not {"conversation_id", "last_modified_time", "workspace_uris"} <= columns:
            raise RuntimeError("unsupported Antigravity summary schema: " + str(path))
        wanted = ("conversation_id", "title", "preview", "last_modified_time", "last_user_input_time",
                  "workspace_uris", "project_id", "parent_conversation_id", "nesting_depth", "app_data_dir", "status")
        query = "SELECT " + ", ".join(c if c in columns else "NULL AS " + c for c in wanted) + " FROM conversation_summaries"
        params = list(live_ids) if live_only else []
        if live_only:
            query += " WHERE conversation_id IN (" + ",".join("?" for _ in params) + ")"
        for data in connection.execute(query, params):
            identity = data["conversation_id"]
            if not session_uuid(identity) or data["parent_conversation_id"] or data["nesting_depth"]:
                continue
            if data["app_data_dir"] not in (None, "", "antigravity-cli"):
                continue  # This adapter does not open IDE-owned storage.
            if not any((root / "conversations" / (identity + suffix)).is_file() for suffix in (".db", ".pb")):
                continue  # A stale summary is not a resumable conversation.
            recency = max(seconds(data["last_modified_time"]), seconds(data["last_user_input_time"]))
            rows.append(record("agy", identity, agy_workspace(data["workspace_uris"], data["project_id"]),
                               text(data["title"]) or text(data["preview"]) or identity,
                               "", recency, recency, "unknown", root, identity,
                               data["project_id"] or None))
    finally:
        connection.close()
    return rows


def is_harness_process(proc, agent):
    argv = proc["argv"]
    names = [Path(arg).name for arg in argv[:2]]
    if agent == "claude":
        return "claude" in names or any("/claude-code/" in arg and arg.endswith("cli.js") for arg in argv[:2])
    return any(name in ("agy", "antigravity") for name in names)


def uncertain_owners(rows, procs, agent, registered=()):
    # A process can switch conversations without changing argv or cwd. An
    # unmatched frontend therefore blocks implicit resume across this harness,
    # not just whichever saved file happens to be newest in its directory.
    if any(pid not in registered and is_harness_process(proc, agent) for pid, proc in procs.items()):
        for row in rows:
            row["ownershipUncertain"] = True


def agy_presence(procs):
    """Kernel-held locks prove presence; a leftover .lock file proves nothing."""
    root = Path.home() / ".gemini/antigravity-cli/presence"
    paths = {}
    for path in root.glob("*.lock"):
        if not session_uuid(path.stem):
            continue
        try:
            stat = path.stat()
            paths.setdefault(stat.st_ino, []).append(path)
        except OSError:
            continue
    if not paths:
        return {}
    try:
        lines = Path("/proc/locks").read_text().splitlines()
    except OSError:
        return {}
    owners = {}
    for line in lines:
        fields = line.split()
        # Blocked lock requests contain '->' and must not count as ownership.
        if len(fields) < 8 or fields[1:4] != ["FLOCK", "ADVISORY", "WRITE"]:
            continue
        try:
            pid = int(fields[4])
            _, _, inode = fields[5].split(":")
            candidates = paths.get(int(inode), [])
        except ValueError:
            continue
        if not candidates or pid not in procs:
            continue
        try:
            if process_stat(pid)[1] != procs[pid]["start"]:
                continue  # PID was recycled since the process snapshot.
        except (OSError, ValueError, IndexError):
            continue
        # Btrfs can report a different virtual device in stat() and /proc/locks.
        # Verify the exact open file AND its fdinfo lock instead of trusting an
        # inode alone (which can collide across mounts).
        try:
            for fd in Path(f"/proc/{pid}/fd").iterdir():
                for path in candidates:
                    try:
                        if not os.path.samefile(fd, path):
                            continue
                        info = Path(f"/proc/{pid}/fdinfo/{fd.name}").read_text()
                        held = any(line.split()[2:6] == ["FLOCK", "ADVISORY", "WRITE", str(pid)]
                                   for line in info.splitlines() if line.startswith("lock:"))
                        if held and pid not in owners.get(path.stem, []):
                            owners.setdefault(path.stem, []).append(pid)
                    except OSError:
                        continue
        except OSError:
            continue
    return owners


def agy_owners(rows, procs, panes, owners):
    registered = set()
    for row in rows:
        pids = owners.get(row["rawId"], [])
        if len(pids) > 1:
            row["ownershipUncertain"] = True
        elif pids:
            pid = pids[0]
            registered.add(pid)
            # A shared backend's lock is presence, not an attachable terminal.
            terminal = is_harness_process(procs[pid], "agy")
            try:
                terminal = terminal and os.readlink(f"/proc/{pid}/fd/0").startswith("/dev/pts/")
            except OSError:
                terminal = False
            set_owner(row, pid, procs, panes, terminal)
    uncertain_owners(rows, procs, "agy", registered)


def process_stat(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return int(fields[1]), fields[19], fields[0]  # parent PID, start ticks, state


def processes():
    result = {}
    for path in Path("/proc").glob("[0-9]*"):
        try:
            if path.stat().st_uid != os.getuid():
                continue
            parent, start, state = process_stat(path.name)
            if state == "Z":
                continue
            argv = path.joinpath("cmdline").read_bytes().decode(errors="replace").rstrip("\0").split("\0")
            result[int(path.name)] = dict(parent=parent, start=start, argv=argv, cwd=os.readlink(path / "cwd"))
        except (OSError, ValueError, IndexError):
            continue
    return result


def tmux_panes():
    try:
        output = subprocess.run(["tmux", "list-panes", "-a", "-F", "#{pane_pid}\t#{session_name}\t#{pane_id}"],
                                capture_output=True, text=True, timeout=3)
    except (OSError, subprocess.TimeoutExpired):
        return {}
    result = {}
    for line in output.stdout.splitlines():
        fields = line.split("\t")
        if len(fields) == 3 and fields[0].isdigit():
            result[int(fields[0])] = dict(session=fields[1], pane=fields[2])
    return result


def binding(pid, procs, panes):
    seen = set()
    while pid in procs and pid not in seen:
        if pid in panes:
            return panes[pid]
        seen.add(pid)
        pid = procs[pid]["parent"]
    return None


def set_owner(row, pid, procs, panes, terminal=True, activity=None):
    session = row["session"]
    pane = binding(pid, procs, panes) if terminal else None
    # Never replace a known attachable frontend with an external one.
    if session["runtime"] != "tmuxFrontend":
        session["runtime"] = "tmuxFrontend" if pane else "externalFrontend"
        if pane:
            session["tmux"] = pane
    if activity in ("working", "completed", "failed", "waitingInput", "waitingApproval"):
        session["activity"] = activity


def pi_live_states(procs):
    result = []
    root = Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache") / "rollcall/pi"
    for path in root.glob("*.json"):
        try:
            live = read_json(path, {})
            pid = int(live.get("pid", 0))
            if pid in procs and str(live.get("startTicks")) == procs[pid]["start"]:
                result.append(live)
        except (OSError, ValueError, TypeError, AttributeError):
            continue
    return result


def pi_owners(rows, procs, panes, live_states):
    by_file = {row["locator"]: row for row in rows}
    registered = set()
    for live in live_states:
        pid = int(live["pid"])
        row = by_file.get(live.get("sessionFile"))
        if row and live.get("sessionId") == row["rawId"]:
            registered.add(pid)
            set_owner(row, pid, procs, panes, live.get("terminal") is True, live.get("activity"))
            session = row["session"]
            session["lastInteractionUnixSeconds"] = max(session["lastInteractionUnixSeconds"], seconds(live.get("interactionUnixSeconds")))
    # Pi overwrites argv and opens JSONL files only while appending. A bare live
    # process cannot safely be matched to 'the newest file in this directory'.
    for pid, proc in procs.items():
        if pid in registered:
            continue
        name = Path(proc["argv"][0]).name if proc["argv"] else ""
        if name in ("pi", "pi-rpc"):
            for row in rows:
                if row["session"]["cwd"] == proc["cwd"]:
                    row["ownershipUncertain"] = True


def hermes_owners(rows, procs, panes):
    by_home = {}
    for row in rows:
        by_home.setdefault(row["locator"], {})[row["rawId"]] = row
    try:
        boot = next(int(line.split()[1]) for line in Path("/proc/stat").read_text().splitlines() if line.startswith("btime "))
        ticks = os.sysconf("SC_CLK_TCK")
    except (OSError, StopIteration, ValueError):
        boot, ticks = 0, 1
    for home, sessions in by_home.items():
        path = Path(home) / "runtime/active_sessions.json"
        registered = set()
        try:
            registry = read_json(path, {"entries": []})
            entries = registry["entries"]
            if not isinstance(entries, list):
                raise ValueError("invalid entries")
            for entry in entries:
                row = sessions.get(entry.get("session_id"))
                if not row:
                    continue
                pid = int(entry.get("pid", 0))
                if pid not in procs:
                    if Path(f"/proc/{pid}").exists():
                        row["ownershipUncertain"] = True
                    continue
                expected = entry.get("process_start_time")
                if expected is None or not boot:
                    row["ownershipUncertain"] = True
                    continue
                actual = boot + int(procs[pid]["start"]) / ticks
                if abs(actual - float(expected)) > 0.1:
                    continue  # Dead lease with a reused PID.
                registered.add(pid)
                # Shared gateway/desktop backends are not terminal frontends.
                set_owner(row, pid, procs, panes, entry.get("surface") == "cli")
        except (OSError, ValueError, TypeError, KeyError, AttributeError):
            for row in sessions.values():
                row["ownershipUncertain"] = True
        # Older Hermes CLIs may hold the database without publishing a lease.
        # That proves a possible owner, not which session is in its terminal.
        for pid, proc in procs.items():
            if pid in registered:
                continue
            targets = [Path(arg).name for arg in proc["argv"][:2]]
            if not any(name in ("hermes", "hermes-agent", "run_agent.py") for name in targets):
                continue
            try:
                opened = {os.readlink(fd).removesuffix(" (deleted)") for fd in Path(f"/proc/{pid}/fd").iterdir()}
            except OSError:
                opened = set()
            if opened.intersection(str(Path(home) / name) for name in ("state.db", "state.db-wal", "state.db-shm")):
                for row in sessions.values():
                    row["ownershipUncertain"] = True


def inventory(agent, limit=None, live_only=False):
    procs = processes()
    live_states = pi_live_states(procs) if agent == "pi" else []
    presence = agy_presence(procs) if agent == "agy" else {}
    if agent == "pi":
        rows = pi_inventory(live_states, live_only)
    elif agent == "hermes":
        rows = hermes_inventory(live_only)
    elif agent == "claude":
        rows = claude_inventory(live_only)
    elif agent == "agy":
        rows = agy_inventory(live_only, presence)
    else:
        raise ValueError("unsupported adapter")
    panes = tmux_panes() if procs and agent != "claude" else {}
    if agent == "pi":
        pi_owners(rows, procs, panes, live_states)
    elif agent == "hermes":
        hermes_owners(rows, procs, panes)
    elif agent == "claude":
        uncertain_owners(rows, procs, "claude")
    else:
        agy_owners(rows, procs, panes, presence)
    if not Path("/proc").is_dir():
        for row in rows:
            row["ownershipUncertain"] = True
    rows.sort(key=lambda r: (-r["session"]["lastInteractionUnixSeconds"], -r["session"]["updatedUnixSeconds"], r["session"]["nativeSessionId"]))
    return rows if limit is None else rows[:limit]


if __name__ == "__main__":
    try:
        agent = sys.argv[1]
        if agent not in ("pi", "hermes", "claude", "agy"):
            raise ValueError("unsupported adapter")
        limit = None if sys.argv[2] == "all" else int(sys.argv[2])
        print(json.dumps(inventory(agent, limit, len(sys.argv) > 3 and sys.argv[3] == "live")))
    except Exception as error:
        print(f"{sys.argv[1]} inventory failed: {error}", file=sys.stderr)
        sys.exit(1)
