// sessiontap-managed-extension v1
// Owned by SessionTap (`sessiontap setup pi`). Do not edit by hand; refresh
// with `sessiontap setup pi` or remove with `sessiontap hooks remove pi`.
// Forwards bounded lifecycle metadata to the local SessionTap broker. The
// handlers return synchronously, never write stdout or stderr, and treat
// every delivery failure as a silent no-op, so pi behaves identically with
// or without this extension.

import { spawn } from "node:child_process";
import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";

const SESSIONTAP_EXECUTABLE = "__SESSIONTAP_EXECUTABLE__";
const BOUND_CHARS = 160;

let inputTokens = 0;
let outputTokens = 0;
let lastExcerpt: string | undefined;

function boundedText(value: unknown, maxChars: number): string | undefined {
  if (typeof value !== "string") return undefined;
  const clean = value
    .replace(/[\u0000-\u001f\u007f]/g, " ")
    .replace(/\s+/g, " ")
    .trim();
  if (clean.length === 0) return undefined;
  return Array.from(clean).slice(0, maxChars).join("");
}

function toNumber(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 ? value : 0;
}

// Cumulative accounting matches Claude-style collection: fresh, cache-read,
// and cache-write tokens all count toward input.
function addUsage(usage: any): void {
  if (!usage || typeof usage !== "object") return;
  inputTokens += toNumber(usage.input) + toNumber(usage.cacheRead) + toNumber(usage.cacheWrite);
  outputTokens += toNumber(usage.output);
}

function sessionFields(ctx: any): Record<string, unknown> {
  const fields: Record<string, unknown> = {};
  try {
    const manager = ctx && ctx.sessionManager;
    if (manager) {
      const id = typeof manager.getSessionId === "function" ? manager.getSessionId() : undefined;
      if (typeof id === "string" && id.length > 0) fields.session_id = id;
      const name =
        typeof manager.getSessionName === "function"
          ? boundedText(manager.getSessionName(), BOUND_CHARS)
          : undefined;
      if (name) fields.session_name = name;
    }
    const model = ctx && ctx.model;
    if (model && typeof model.provider === "string" && typeof model.id === "string") {
      fields.model = model.provider + "/" + model.id;
    }
    if (ctx && typeof ctx.thinkingLevel === "string") fields.thinking_level = ctx.thinkingLevel;
    if (ctx && typeof ctx.mode === "string") fields.mode = ctx.mode;
  } catch {
    // Metadata is best-effort; missing fields degrade to absent metadata.
  }
  return fields;
}

function messageText(message: any): string | undefined {
  if (!message || !Array.isArray(message.content)) return undefined;
  const parts: string[] = [];
  for (const part of message.content) {
    if (part && typeof part === "object" && part.type === "text" && typeof part.text === "string") {
      parts.push(part.text);
    }
  }
  return parts.length > 0 ? parts.join(" ") : undefined;
}

function lastAssistantMessage(ctx: any): any {
  try {
    const manager = ctx && ctx.sessionManager;
    const entries =
      manager && typeof manager.getEntries === "function" ? manager.getEntries() : [];
    for (let index = entries.length - 1; index >= 0; index -= 1) {
      const entry = entries[index];
      if (entry && entry.type === "message" && entry.message && entry.message.role === "assistant") {
        return entry.message;
      }
    }
  } catch {
    // Settled status degrades to complete when entries are unavailable.
  }
  return undefined;
}

function settledStatus(ctx: any): string {
  const message = lastAssistantMessage(ctx);
  if (message && message.stopReason === "error") return "error";
  if (message && message.stopReason === "aborted") return "aborted";
  return "complete";
}

// Seeds cumulative totals from pi's own session API whenever the opened
// session already contains entries (explicit continue/resume/session launches
// and in-session switches). Fresh sessions start at zero.
function seedUsage(ctx: any): void {
  inputTokens = 0;
  outputTokens = 0;
  try {
    const manager = ctx && ctx.sessionManager;
    const entries =
      manager && typeof manager.getEntries === "function" ? manager.getEntries() : [];
    for (const entry of entries) {
      if (!entry || entry.type !== "message") continue;
      const message = entry.message;
      if (!message || message.role !== "assistant") continue;
      addUsage(message.usage);
    }
  } catch {
    // Seeding is best-effort; live turns still accumulate.
  }
}

function forward(payload: Record<string, unknown>): void {
  try {
    const child = spawn(SESSIONTAP_EXECUTABLE, ["hook", "emit", "pi"], {
      stdio: ["pipe", "ignore", "ignore"],
      detached: true,
    });
    child.on("error", () => undefined);
    child.stdin.on("error", () => undefined);
    child.stdin.end(JSON.stringify(payload));
    child.unref();
  } catch {
    // Delivery failures are silent by contract.
  }
}

function forwardTool(name: string, event: any, ctx: any): void {
  try {
    const payload = Object.assign({ pi_event: name }, sessionFields(ctx));
    if (event && typeof event.toolName === "string") payload.tool_name = event.toolName;
    if (event && typeof event.toolCallId === "string") payload.tool_call_id = event.toolCallId;
    forward(payload);
  } catch {
    // Tool forwarding is best-effort.
  }
}

export default function sessiontapBroker(pi: ExtensionAPI): void {
  try {
    pi.on("session_start", (event: any, ctx: any) => {
      try {
        seedUsage(ctx);
        lastExcerpt = undefined;
        const payload: Record<string, unknown> = { pi_event: "session_start" };
        if (event && typeof event.reason === "string") payload.reason = event.reason;
        forward(Object.assign(payload, sessionFields(ctx)));
      } catch {
        // Fail open.
      }
    });
    pi.on("session_shutdown", (event: any, ctx: any) => {
      try {
        const payload: Record<string, unknown> = { pi_event: "session_shutdown" };
        if (event && typeof event.reason === "string") payload.reason = event.reason;
        forward(Object.assign(payload, sessionFields(ctx)));
      } catch {
        // Fail open.
      }
    });
    pi.on("session_info_changed", (_event: any, ctx: any) => {
      try {
        forward(Object.assign({ pi_event: "session_info_changed" }, sessionFields(ctx)));
      } catch {
        // Fail open.
      }
    });
    pi.on("model_select", (event: any, ctx: any) => {
      try {
        const payload = Object.assign({ pi_event: "model_select" }, sessionFields(ctx));
        const model = event && event.model;
        if (model && typeof model.provider === "string" && typeof model.id === "string") {
          payload.model = model.provider + "/" + model.id;
        }
        forward(payload);
      } catch {
        // Fail open.
      }
    });
    pi.on("thinking_level_select", (event: any, ctx: any) => {
      try {
        const payload = Object.assign(
          { pi_event: "thinking_level_select" },
          sessionFields(ctx)
        );
        if (event && typeof event.level === "string") payload.thinking_level = event.level;
        forward(payload);
      } catch {
        // Fail open.
      }
    });
    pi.on("before_agent_start", (_event: any, ctx: any) => {
      try {
        forward(Object.assign({ pi_event: "before_agent_start" }, sessionFields(ctx)));
      } catch {
        // Fail open.
      }
    });
    pi.on("turn_start", (event: any, ctx: any) => {
      try {
        const payload = Object.assign({ pi_event: "turn_start" }, sessionFields(ctx));
        if (event && typeof event.turnIndex === "number") payload.turn_index = event.turnIndex;
        forward(payload);
      } catch {
        // Fail open.
      }
    });
    // Local accounting only: accumulate per-turn usage and capture the
    // bounded last-assistant excerpt consumed by the next settled payload.
    // This event is never forwarded.
    pi.on("turn_end", (event: any, _ctx: any) => {
      try {
        const message = event && event.message;
        if (message && message.role === "assistant") {
          addUsage(message.usage);
          lastExcerpt = boundedText(messageText(message), BOUND_CHARS);
        }
      } catch {
        // Fail open.
      }
    });
    pi.on("tool_execution_start", (event: any, ctx: any) => {
      forwardTool("tool_execution_start", event, ctx);
    });
    pi.on("tool_execution_end", (event: any, ctx: any) => {
      forwardTool("tool_execution_end", event, ctx);
    });
    pi.on("agent_settled", (_event: any, ctx: any) => {
      try {
        const payload = Object.assign({ pi_event: "agent_settled" }, sessionFields(ctx));
        payload.settled_status = settledStatus(ctx);
        if (lastExcerpt) payload.excerpt = lastExcerpt;
        payload.input_tokens = inputTokens;
        payload.output_tokens = outputTokens;
        const contextUsage =
          ctx && typeof ctx.getContextUsage === "function" ? ctx.getContextUsage() : undefined;
        if (contextUsage && typeof contextUsage === "object") {
          if (typeof contextUsage.tokens === "number" && Number.isFinite(contextUsage.tokens)) {
            payload.context_tokens = contextUsage.tokens;
          }
          if (typeof contextUsage.percent === "number" && Number.isFinite(contextUsage.percent)) {
            payload.context_window_percent = Math.min(
              100,
              Math.max(0, Math.round(contextUsage.percent))
            );
          }
        }
        forward(payload);
      } catch {
        // Fail open.
      }
    });
  } catch {
    // Registration failures leave pi unchanged; worst case is missing
    // observability.
  }
}
