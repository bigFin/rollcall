import importlib.util
import json
import os
from pathlib import Path
import sqlite3
import tempfile
import unittest
from unittest.mock import patch

spec = importlib.util.spec_from_file_location("native", Path(__file__).parents[1] / "src/probes/native.py")
native = importlib.util.module_from_spec(spec)
spec.loader.exec_module(native)


class NativeInventoryTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.home = Path(self.tmp.name)
        env = patch.dict(os.environ, {"HOME": str(self.home), "XDG_CACHE_HOME": str(self.home / "cache")})
        env.start()
        self.addCleanup(env.stop)
        for name in ("PI_CODING_AGENT_DIR", "PI_CODING_AGENT_SESSION_DIR", "HERMES_HOME", "CLAUDE_CONFIG_DIR"):
            os.environ.pop(name, None)
        self.procs = {101: dict(parent=100, start="123", argv=["pi"], cwd="/work"),
                      100: dict(parent=1, start="99", argv=["bash"], cwd="/work")}
        self.panes = {100: {"session": "workspace", "pane": "%7"}}

    def pi_file(self, native_id="one", relative=None, stamp=100):
        path = self.home / (relative or f".pi/agent/sessions/project/{native_id}.jsonl")
        path.parent.mkdir(parents=True, exist_ok=True)
        entries = [
            {"type": "session", "id": native_id, "cwd": "/work", "timestamp": "1970-01-01T00:00:01Z"},
            {"type": "model_change"}, {"type": "thinking_level_change"},
            {"type": "message", "message": {"role": "user", "content": "first prompt", "timestamp": stamp * 1000}},
            {"type": "session_info", "name": "Named Pi session"},
            {"type": "message", "message": {"role": "assistant", "content": [{"type": "text", "text": "latest answer"}], "stopReason": "stop", "timestamp": (stamp + 1) * 1000}},
        ]
        path.write_text("\n".join(json.dumps(e) for e in entries) + '\n{"unfinished":')
        return path

    def live_state(self, path, start="123", terminal=True):
        state = dict(pid=101, startTicks=start, sessionFile=str(path), sessionId="one", terminal=terminal, activity="working", interactionUnixSeconds=200)
        root = self.home / "cache/rollcall/pi"
        root.mkdir(parents=True, exist_ok=True)
        (root / "101.json").write_text(json.dumps(state))
        return state

    def test_pi_reads_late_metadata_and_native_activity_not_file_mtime(self):
        self.pi_file()
        row = native.pi_inventory()[0]["session"]
        self.assertEqual(row["agent"], "pi")
        self.assertEqual(row["title"], "Named Pi session")
        self.assertEqual(row["lastMessage"], "latest answer")
        self.assertEqual(row["lastInteractionUnixSeconds"], 101)
        self.assertEqual(row["activity"], "completed")

    def test_pi_default_excludes_nested_children_and_omp(self):
        self.pi_file()
        self.pi_file("child", ".pi/agent/sessions/project/parent/child.jsonl")
        self.pi_file("omp", ".omp/agent/sessions/project/omp.jsonl")
        self.assertEqual([r["rawId"] for r in native.pi_inventory()], ["one"])

    def test_pi_custom_flat_directory_and_live_paths(self):
        self.pi_file("flat", "custom/flat.jsonl")
        os.environ["PI_CODING_AGENT_SESSION_DIR"] = str(self.home / "custom")
        self.assertEqual([r["rawId"] for r in native.pi_inventory()], ["flat"])
        outside = self.pi_file("one", "outside/one.jsonl")
        state = self.live_state(outside)
        self.assertEqual([r["rawId"] for r in native.pi_inventory([state], True)], ["one"])

    def test_pi_live_registry_validates_process_identity_and_tmux_ancestry(self):
        path = self.pi_file()
        self.live_state(path)
        rows = native.pi_inventory()
        states = native.pi_live_states(self.procs)
        native.pi_owners(rows, self.procs, self.panes, states)
        self.assertEqual(rows[0]["session"]["runtime"], "tmuxFrontend")
        self.assertEqual(rows[0]["session"]["tmux"]["pane"], "%7")
        self.assertEqual(rows[0]["session"]["activity"], "working")
        self.assertEqual(rows[0]["session"]["lastInteractionUnixSeconds"], 200)
        self.live_state(path, start="122")
        self.assertEqual(native.pi_live_states(self.procs), [])

    def test_pi_never_guesses_newest_file_or_attaches_rpc(self):
        path = self.pi_file()
        rows = native.pi_inventory()
        native.pi_owners(rows, self.procs, self.panes, [])
        self.assertTrue(rows[0]["ownershipUncertain"])
        self.assertEqual(rows[0]["session"]["runtime"], "resumable")
        state = self.live_state(path, terminal=False)
        native.pi_owners(rows, self.procs, self.panes, [state])
        self.assertEqual(rows[0]["session"]["runtime"], "externalFrontend")

    def hermes_db(self, profile="main", legacy=False):
        home = self.home / ".hermes" / "profiles" / profile
        home.mkdir(parents=True, exist_ok=True)
        connection = sqlite3.connect(home / "state.db")
        connection.execute("CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT, started_at REAL, ended_at REAL, title TEXT, cwd TEXT, parent_session_id TEXT" + ("" if legacy else ", archived INTEGER DEFAULT 0, hidden INTEGER DEFAULT 0, last_activity_at REAL") + ")")
        connection.execute("CREATE TABLE messages (id INTEGER PRIMARY KEY, session_id TEXT, role TEXT, content TEXT, timestamp REAL)")
        connection.execute("INSERT INTO sessions (id, source, started_at, title, cwd) VALUES ('same-id', 'cli', 10, 'Hermes title', '/work')")
        connection.execute("INSERT INTO messages VALUES (1, 'same-id', 'user', 'hello', 500)")
        connection.execute("INSERT INTO messages VALUES (2, 'same-id', 'assistant', 'done', 600)")
        connection.commit()
        connection.close()
        return home

    def test_hermes_profile_identity_native_chronology_and_read_only_db(self):
        first = self.hermes_db("main")
        self.hermes_db("voice", legacy=True)
        before = (first / "state.db").read_bytes()
        rows = native.hermes_inventory()
        self.assertEqual({r["session"]["nativeSessionId"] for r in rows}, {"main/same-id", "voice/same-id"})
        self.assertTrue(all(r["session"]["lastInteractionUnixSeconds"] == 600 for r in rows))
        self.assertTrue(all(r["session"]["lastMessage"] == "done" for r in rows))
        self.assertEqual((first / "state.db").read_bytes(), before)
        self.assertFalse((self.home / ".hermes/state.db").exists())

    def test_hermes_filter_archives_but_keep_continuation_sessions(self):
        home = self.hermes_db()
        connection = sqlite3.connect(home / "state.db")
        connection.execute("INSERT INTO sessions (id, source, started_at, archived) VALUES ('archived', 'cli', 1000, 1)")
        connection.execute("INSERT INTO sessions (id, source, started_at, parent_session_id) VALUES ('continuation', 'cli', 900, 'same-id')")
        connection.execute("INSERT INTO sessions (id, source, started_at, parent_session_id) VALUES ('subagent', 'tool', 999, 'same-id')")
        connection.commit()
        connection.close()
        self.assertEqual({r["rawId"] for r in native.hermes_inventory()}, {"same-id", "continuation"})

    @unittest.skipUnless(Path("/proc/self/stat").exists(), "Linux process identity")
    def test_hermes_real_lease_does_not_make_shared_desktop_attachable(self):
        home = self.hermes_db()
        # Use this test process for an independently verified process creation timestamp.
        _, start, _ = native.process_stat(os.getpid())
        boot = next(int(line.split()[1]) for line in Path("/proc/stat").read_text().splitlines() if line.startswith("btime "))
        entry = dict(pid=os.getpid(), session_id="same-id", surface="desktop", process_start_time=boot + int(start) / os.sysconf("SC_CLK_TCK"))
        root = home / "runtime"
        root.mkdir()
        (root / "active_sessions.json").write_text(json.dumps({"entries": [entry]}))
        rows = native.hermes_inventory()
        procs = native.processes()
        panes = {os.getpid(): {"session": "server", "pane": "%99"}}
        native.hermes_owners(rows, procs, panes)
        self.assertEqual(rows[0]["session"]["runtime"], "externalFrontend")
        self.assertNotIn("tmux", rows[0]["session"])
        entry["surface"] = "cli"
        (root / "active_sessions.json").write_text(json.dumps({"entries": [entry]}))
        rows = native.hermes_inventory()
        native.hermes_owners(rows, procs, panes)
        self.assertEqual(rows[0]["session"]["runtime"], "tmuxFrontend")
        entry["process_start_time"] -= 10
        (root / "active_sessions.json").write_text(json.dumps({"entries": [entry]}))
        rows = native.hermes_inventory()
        native.hermes_owners(rows, procs, panes)
        self.assertEqual(rows[0]["session"]["runtime"], "resumable")

    def test_corrupt_hermes_lease_fails_closed_for_attach(self):
        home = self.hermes_db()
        root = home / "runtime"
        root.mkdir()
        (root / "active_sessions.json").write_text("{broken")
        rows = native.hermes_inventory()
        native.hermes_owners(rows, self.procs, self.panes)
        self.assertTrue(rows[0]["ownershipUncertain"])

    def test_inventory_limit_uses_recency_and_live_poll_avoids_historical_files(self):
        self.pi_file("old", stamp=10)
        path = self.pi_file("one", stamp=100)
        self.live_state(path)
        with patch.object(native, "processes", return_value=self.procs), patch.object(native, "tmux_panes", return_value=self.panes):
            self.assertEqual(native.inventory("pi", 1)[0]["rawId"], "one")
            self.assertEqual(native.inventory("pi", 0), [])
            with patch.object(native, "pi_file", wraps=native.pi_file) as parse:
                self.assertEqual(len(native.inventory("pi", live_only=True)), 1)
                self.assertEqual(parse.call_count, 1)

    CLAUDE_ID = "11111111-1111-4111-8111-111111111111"
    AGY_ID = "22222222-2222-4222-8222-222222222222"

    def claude_file(self, root=None, identity=None):
        root = root or self.home / ".claude"
        identity = identity or self.CLAUDE_ID
        path = root / "projects/project" / (identity + ".jsonl")
        path.parent.mkdir(parents=True, exist_ok=True)
        rows = [
            {"type": "user", "sessionId": identity, "cwd": "/work", "timestamp": "2026-01-01T00:00:00Z", "message": {"content": "A Claude task"}},
            {"type": "assistant", "sessionId": identity, "cwd": "/work", "timestamp": "2026-01-01T00:01:00Z", "message": {"content": [{"type": "thinking", "thinking": "private"}, {"type": "text", "text": "Claude answer"}], "stop_reason": "end_turn"}},
            {"type": "summary", "summary": "A summary", "timestamp": "2026-02-01T00:00:00Z"},
            {"type": "custom-title", "customTitle": "Named Claude task"},
        ]
        path.write_text("\n".join(json.dumps(row) for row in rows) + '\nnull\n{"unfinished":')
        return path

    def test_claude_metadata_uses_native_messages_and_ignores_partial_records(self):
        self.claude_file()
        row = native.claude_inventory()[0]
        self.assertEqual(row["rawId"], self.CLAUDE_ID)
        self.assertEqual(row["session"]["title"], "Named Claude task")
        self.assertEqual(row["session"]["lastMessage"], "Claude answer")
        self.assertEqual(row["session"]["lastInteractionUnixSeconds"], native.seconds("2026-01-01T00:01:00Z"))
        self.assertEqual(row["session"]["activity"], "completed")
        self.assertEqual(row["session"]["cwd"], "/work")

    def test_claude_custom_config_sidechains_and_superseded_files(self):
        self.claude_file()  # Must not leak into custom-config inventory.
        root = self.home / "custom Claude"
        path = self.claude_file(root)
        path.with_name(path.stem + ".orphaned-old.jsonl").write_text(path.read_text())
        children = path.parent / self.CLAUDE_ID / "subagents"
        children.mkdir(parents=True)
        (children / "agent-test.jsonl").write_text(path.read_text())
        self.claude_file(root, self.AGY_ID).write_text(json.dumps({"type": "user", "isSidechain": True, "message": {"content": "child"}}))
        os.environ["CLAUDE_CONFIG_DIR"] = str(root)
        rows = native.claude_inventory()
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["locator"], str(root))
        self.assertEqual(native.claude_inventory(live_only=True), [])

    def test_unidentified_claude_process_never_guesses_a_session_from_cwd_or_argv(self):
        self.claude_file()
        rows = native.claude_inventory()
        proc = {101: dict(parent=100, start="123", argv=["claude", "--resume", self.CLAUDE_ID], cwd="/different-workspace")}
        native.uncertain_owners(rows, proc, "claude")
        self.assertTrue(rows[0]["ownershipUncertain"])
        self.assertEqual(rows[0]["session"]["runtime"], "resumable")
        self.assertNotIn("tmux", rows[0]["session"])

    def agy_db(self):
        root = self.home / ".gemini/antigravity-cli"
        (root / "conversations").mkdir(parents=True)
        (root / "conversations" / (self.AGY_ID + ".pb")).write_bytes(b"opaque-native-payload")
        db = root / "conversation_summaries.db"
        connection = sqlite3.connect(db)
        connection.execute("CREATE TABLE conversation_summaries (conversation_id TEXT PRIMARY KEY, title TEXT, preview TEXT, last_modified_time TEXT, last_user_input_time TEXT, workspace_uris TEXT, project_id TEXT, parent_conversation_id TEXT, nesting_depth INTEGER DEFAULT 0, app_data_dir TEXT DEFAULT 'antigravity-cli', status TEXT, raw_summary BLOB)")
        connection.execute("INSERT INTO conversation_summaries (conversation_id,title,preview,last_modified_time,last_user_input_time,workspace_uris,project_id,status,raw_summary) VALUES (?,?,?,?,?,?,?,?,?)", (self.AGY_ID, "Agy title", "Native preview", "2026-01-01 00:00:00+00:00", "2026-01-01 00:01:00.123456789+00:00", '["file:///work/a%20project"]', "default-cli-project", "CASCADE_RUN_STATUS_RUNNING", b"never decode private trajectory"))
        connection.commit()
        connection.close()
        return db

    def test_agy_reads_only_summary_metadata_not_opaque_conversation_payload(self):
        db = self.agy_db()
        before = db.read_bytes()
        row = native.agy_inventory()[0]
        self.assertEqual(row["session"]["cwd"], "/work/a project")
        self.assertEqual(row["session"]["title"], "Agy title")
        self.assertEqual(row["session"]["lastMessage"], "")  # Summary preview is not a verified final message.
        self.assertEqual(row["session"]["activity"], "unknown")  # Saved RUNNING is not liveness.
        self.assertEqual(row["session"]["lastInteractionUnixSeconds"], native.seconds("2026-01-01T00:01:00Z"))
        self.assertEqual(row["profile"], "default-cli-project")
        self.assertEqual(db.read_bytes(), before)
        self.assertEqual(native.agy_inventory(live_only=True), [])
        self.assertEqual(len(native.agy_inventory(live_only=True, live_ids=[self.AGY_ID])), 1)

    def test_agy_ignores_deleted_child_and_ide_conversations(self):
        db = self.agy_db()
        for assignment in ["parent_conversation_id='parent'", "nesting_depth=1", "app_data_dir='antigravity'"]:
            with sqlite3.connect(db) as connection:
                connection.execute("UPDATE conversation_summaries SET parent_conversation_id='', nesting_depth=0, app_data_dir='antigravity-cli'")
                connection.execute("UPDATE conversation_summaries SET " + assignment)
            self.assertEqual(native.agy_inventory(), [])
        with sqlite3.connect(db) as connection:
            connection.execute("UPDATE conversation_summaries SET app_data_dir='antigravity-cli'")
        (db.parent / "conversations" / (self.AGY_ID + ".pb")).unlink()
        self.assertEqual(native.agy_inventory(), [])

    def test_agy_workspace_fallback_uses_only_local_project_uris(self):
        root = self.home / ".gemini/config/projects"
        root.mkdir(parents=True)
        (root / (self.AGY_ID + ".json")).write_text(json.dumps({"projectResources": {"resources": [{"gitFolder": {"folderUri": "file:///fallback/a%20project"}}]}}))
        self.assertEqual(native.agy_workspace("", self.AGY_ID), "/fallback/a project")
        self.assertEqual(native.agy_workspace('["file://other-host/work"]', "../../outside"), "")
        self.assertEqual(native.agy_workspace('not-json', None), "")
        self.assertEqual(native.local_file_uri("file:///work/%00bad"), "")

    @unittest.skipUnless(Path("/proc/locks").exists(), "Linux kernel locks")
    def test_agy_presence_requires_a_held_lock_not_a_stale_file(self):
        import fcntl
        root = self.home / ".gemini/antigravity-cli/presence"
        root.mkdir(parents=True)
        with (root / (self.AGY_ID + ".lock")).open("w") as lock:
            procs = native.processes()
            self.assertEqual(native.agy_presence(procs), {})
            fcntl.flock(lock, fcntl.LOCK_EX)
            self.assertEqual(native.agy_presence(procs), {self.AGY_ID: [os.getpid()]})
            stale = {pid: dict(proc, start="wrong-start") for pid, proc in procs.items()}
            self.assertEqual(native.agy_presence(stale), {})
            fcntl.flock(lock, fcntl.LOCK_UN)
            self.assertEqual(native.agy_presence(procs), {})

    def test_agy_exact_presence_attaches_only_terminal_owners(self):
        self.agy_db()
        procs = {101: dict(parent=100, start="123", argv=["agy"], cwd="/work"), 100: self.procs[100]}
        for fd, runtime in [("/dev/pts/3", "tmuxFrontend"), ("pipe:[123]", "externalFrontend")]:
            rows = native.agy_inventory()
            with patch.object(native.os, "readlink", return_value=fd):
                native.agy_owners(rows, procs, self.panes, {self.AGY_ID: [101]})
            self.assertEqual(rows[0]["session"]["runtime"], runtime)
            self.assertFalse(rows[0]["ownershipUncertain"])
        rows = native.agy_inventory()
        native.agy_owners(rows, procs, self.panes, {})
        self.assertTrue(rows[0]["ownershipUncertain"])
        self.assertNotIn("tmux", rows[0]["session"])

    def test_missing_harnesses_are_empty_without_creating_directories(self):
        self.assertEqual(native.pi_inventory(), [])
        self.assertEqual(native.hermes_inventory(), [])
        self.assertEqual(native.claude_inventory(), [])
        self.assertEqual(native.agy_inventory(), [])
        self.assertEqual(list(self.home.iterdir()), [])


if __name__ == "__main__":
    unittest.main()
