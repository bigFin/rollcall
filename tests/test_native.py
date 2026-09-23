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
        for name in ("PI_CODING_AGENT_DIR", "PI_CODING_AGENT_SESSION_DIR", "HERMES_HOME"):
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

    def test_missing_harnesses_are_empty_without_creating_directories(self):
        self.assertEqual(native.pi_inventory(), [])
        self.assertEqual(native.hermes_inventory(), [])
        self.assertEqual(list(self.home.iterdir()), [])


if __name__ == "__main__":
    unittest.main()
