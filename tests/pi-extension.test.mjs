import assert from "node:assert/strict";
import { mkdtempSync, readdirSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import rollcall from "../integrations/pi/rollcall.ts";

test("Pi lifecycle bridge tracks exact identity, waits, settlement, and cleanup", { skip: process.platform !== "linux" }, () => {
  const directory = mkdtempSync(join(tmpdir(), "rollcall-pi-"));
  const oldCache = process.env.XDG_CACHE_HOME;
  process.env.XDG_CACHE_HOME = directory;
  try {
    const handlers = new Map();
    rollcall({ on: (name, handler) => handlers.set(name, handler) });
    let id = "session-a";
    let file = "/tmp/session-a.jsonl";
    const ctx = {
      mode: "tui",
      sessionManager: {
        getSessionId: () => id, getSessionFile: () => file,
        getBranch: () => [{ type: "message", message: { role: "user", timestamp: 100000 } }],
      },
    };
    const emit = (name, event = {}) => handlers.get(name)(event, ctx);
    const files = () => readdirSync(join(directory, "rollcall/pi"));
    const state = () => JSON.parse(readFileSync(join(directory, "rollcall/pi", files()[0]), "utf8"));
    emit("session_start");
    assert.equal(state().pid, process.pid);
    assert.equal(state().sessionId, id);
    assert.equal(state().sessionFile, file);
    assert.equal(state().interactionUnixSeconds, 100);
    assert.match(state().startTicks, /^\d+$/);
    assert.equal(state().terminal, true);
    emit("agent_start");
    assert.equal(state().activity, "working");
    emit("ui_prompt_start", { kind: "confirm" });
    assert.equal(state().activity, "waitingApproval");
    emit("ui_prompt_end");
    assert.equal(state().activity, "working");
    emit("message_end", { message: { role: "assistant", stopReason: "error" } });
    emit("agent_settled");
    assert.equal(state().activity, "failed");
    emit("agent_start");
    emit("agent_settled");
    assert.equal(state().activity, "completed");
    emit("session_shutdown");
    assert.deepEqual(files(), []);
    id = "session-b";
    file = "/tmp/session-b.jsonl";
    ctx.mode = "rpc";
    emit("session_start");
    assert.equal(state().sessionId, id);
    assert.equal(state().terminal, false);
    file = undefined;
    emit("agent_start");
    assert.deepEqual(files(), []);
    emit("session_shutdown");
  } finally {
    if (oldCache === undefined) delete process.env.XDG_CACHE_HOME;
    else process.env.XDG_CACHE_HOME = oldCache;
    rmSync(directory, { recursive: true, force: true });
  }
});
