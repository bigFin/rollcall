/** Optional, read-only lifecycle bridge for Rollcall. No prompts or transcript text are written. */
import { mkdirSync, readFileSync, renameSync, rmSync, writeFileSync } from "node:fs";
import { createHash } from "node:crypto";
import { homedir } from "node:os";
import { join, resolve } from "node:path";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

export default function rollcall(pi: ExtensionAPI) {
  let statePath: string | undefined;
  let running = false;
  let failed = false;
  let interaction = 0;

  function cleanup() {
    if (statePath) {
      try { rmSync(statePath, { force: true }); } catch { /* Best effort; PID/start-time validation handles stale files. */ }
      statePath = undefined;
    }
  }

  function publish(ctx: ExtensionContext, activity: string) {
    const sessionFile = ctx.sessionManager.getSessionFile();
    if (!sessionFile) { cleanup(); return; }
    try {
      // PID alone is not identity: Linux may reuse it after an unclean exit.
      const stat = readFileSync("/proc/self/stat", "utf8");
      const startTicks = stat.slice(stat.lastIndexOf(")") + 2).split(" ")[19];
      const directory = join(process.env.XDG_CACHE_HOME || join(homedir(), ".cache"), "rollcall", "pi");
      mkdirSync(directory, { recursive: true, mode: 0o700 });
      const sessionId = ctx.sessionManager.getSessionId();
      const suffix = createHash("sha256").update(sessionId).digest("hex").slice(0, 16);
      const path = join(directory, `${process.pid}-${suffix}.json`);
      if (statePath !== path) cleanup();
      statePath = path;
      const temporary = `${path}.tmp`;
      writeFileSync(temporary, JSON.stringify({
        pid: process.pid, startTicks, sessionId, sessionFile: resolve(sessionFile),
        terminal: ctx.mode === "tui", activity, interactionUnixSeconds: interaction,
      }), { mode: 0o600 });
      renameSync(temporary, path);
    } catch { /* Monitoring must never interrupt the agent; unsupported platforms remain inventory-only. */ }
  }

  pi.on("session_start", (_event, ctx) => {
    running = false;
    failed = false;
    interaction = 0;
    for (const entry of ctx.sessionManager.getBranch()) {
      if (entry.type !== "message" || entry.message.role === "system") continue;
      interaction = Math.max(interaction, Math.floor(entry.message.timestamp / 1000));
      if (entry.message.role === "assistant") failed = entry.message.stopReason === "error";
    }
    publish(ctx, failed ? "failed" : "unknown");
  });
  pi.on("agent_start", (_event, ctx) => {
    running = true;
    failed = false;
    interaction = Math.floor(Date.now() / 1000);
    publish(ctx, "working");
  });
  pi.on("message_end", (event, ctx) => {
    if (event.message.role === "system") return;
    interaction = Math.floor(Date.now() / 1000);
    if (event.message.role === "assistant") failed = event.message.stopReason === "error";
    publish(ctx, running ? "working" : failed ? "failed" : "completed");
  });
  pi.on("agent_settled", (_event, ctx) => {
    running = false;
    publish(ctx, failed ? "failed" : "completed");
  });
  pi.on("ui_prompt_start", (event, ctx) => publish(ctx, event.kind === "confirm" ? "waitingApproval" : "waitingInput"));
  pi.on("ui_prompt_end", (_event, ctx) => publish(ctx, running ? "working" : failed ? "failed" : "completed"));
  pi.on("session_shutdown", cleanup);
}
