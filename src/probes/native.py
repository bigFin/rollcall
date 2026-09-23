"""Read-only Pi/Hermes inventory. Runs locally or over SSH with Python's stdlib only."""
import datetime
import hashlib
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys


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
    rows = pi_inventory(live_states, live_only) if agent == "pi" else hermes_inventory(live_only)
    panes = tmux_panes() if procs else {}
    if agent == "pi":
        pi_owners(rows, procs, panes, live_states)
    else:
        hermes_owners(rows, procs, panes)
    if not Path("/proc").is_dir():
        for row in rows:
            row["ownershipUncertain"] = True
    rows.sort(key=lambda r: (-r["session"]["lastInteractionUnixSeconds"], -r["session"]["updatedUnixSeconds"], r["session"]["nativeSessionId"]))
    return rows if limit is None else rows[:limit]


if __name__ == "__main__":
    try:
        agent = sys.argv[1]
        if agent not in ("pi", "hermes"):
            raise ValueError("unsupported adapter")
        limit = None if sys.argv[2] == "all" else int(sys.argv[2])
        print(json.dumps(inventory(agent, limit, len(sys.argv) > 3 and sys.argv[3] == "live")))
    except Exception as error:
        print(f"{sys.argv[1]} inventory failed: {error}", file=sys.stderr)
        sys.exit(1)
