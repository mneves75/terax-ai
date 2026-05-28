import type { UIMessage } from "@ai-sdk/react";
import type { ChatTransport, UIMessageChunk } from "ai";
import { currentWorkspaceEnv } from "@/modules/workspace";
import {
  loadCodexProcessedApprovalIds,
  loadCodexThreadId,
  saveCodexProcessedApprovalId,
  saveCodexThreadId,
} from "./sessions";
import {
  native,
  type CodexAppServerEvent,
  type CodexRunResult,
} from "./native";

type LiveSnapshot = {
  cwd: string | null;
  terminalPrivate: boolean;
  workspaceRoot: string | null;
  activeFile: string | null;
};

type Deps = {
  getLive: () => LiveSnapshot;
  getCliPath: () => string | undefined;
  onStep?: (step: string | null) => void;
};

type SendOptions = Parameters<ChatTransport<UIMessage>["sendMessages"]>[0];

const processedApprovals = new Set<string>();

export function createCodexTransport(deps: Deps): ChatTransport<UIMessage> {
  return {
    sendMessages: (options) => runCodexTurn(options, deps),
    async reconnectToStream() {
      return null;
    },
  };
}

async function runCodexTurn(
  options: SendOptions,
  deps: Deps,
): Promise<ReadableStream<UIMessageChunk>> {
  const persistedApprovals = new Set(
    await loadCodexProcessedApprovalIds(options.chatId),
  );
  const approval = latestApprovalResponse(
    options.messages,
    options.chatId,
    persistedApprovals,
  );
  const prompt = approval ? null : latestUserText(options.messages);
  if (!approval && !prompt?.trim()) {
    throw new Error("Codex CLI needs a prompt.");
  }

  const live = deps.getLive();
  const workspace = currentWorkspaceEnv();
  const cliPath = deps.getCliPath()?.trim() || "";
  const scopeKey = codexScopeKey({
    cwd: live.cwd,
    workspaceRoot: live.workspaceRoot,
    workspace,
    cliPath,
  });
  const threadId = approval
    ? null
    : await loadCodexThreadId(options.chatId, scopeKey);
  let textOpen = false;
  let finished = false;
  const textId = `codex-text-${Date.now().toString(36)}`;
  const knownToolInputs = new Set<string>();

  let controllerRef: ReadableStreamDefaultController<UIMessageChunk> | null =
    null;

  const enqueue = (chunk: UIMessageChunk) => {
    if (!finished) controllerRef?.enqueue(chunk);
  };

  const ensureText = () => {
    if (textOpen) return;
    enqueue({ type: "text-start", id: textId });
    textOpen = true;
  };

  const closeText = () => {
    if (!textOpen) return;
    enqueue({ type: "text-end", id: textId });
    textOpen = false;
  };

  const ensureToolInput = (
    toolCallId: string,
    toolName: string,
    input: unknown,
    title: string,
  ) => {
    if (knownToolInputs.has(toolCallId)) return;
    knownToolInputs.add(toolCallId);
    enqueue({
      type: "tool-input-available",
      toolCallId,
      toolName,
      input,
      providerExecuted: true,
      dynamic: true,
      title,
    });
  };

  const handleEvent = (event: CodexAppServerEvent) => {
    switch (event.kind) {
      case "agent_delta": {
        ensureText();
        enqueue({ type: "text-delta", id: textId, delta: event.delta });
        break;
      }
      case "plan_updated": {
        deps.onStep?.(event.text);
        ensureText();
        enqueue({ type: "text-delta", id: textId, delta: `\n\n${event.text}\n\n` });
        break;
      }
      case "item_started": {
        const item = asRecord(event.item);
        const tool = itemTool(item);
        if (tool) ensureToolInput(tool.id, tool.name, tool.input, tool.title);
        break;
      }
      case "approval_request": {
        const params = asRecord(event.params) ?? {};
        const item = asRecord(params.item) ?? params;
        const tool =
          itemTool(item) ??
          genericTool(
            `codex-approval-${event.requestId}`,
            "codex_approval",
            params,
            "Codex approval",
          );
        ensureToolInput(tool.id, tool.name, tool.input, tool.title);
        enqueue({
          type: "tool-approval-request",
          approvalId: String(event.requestId),
          toolCallId: tool.id,
        });
        break;
      }
      case "item_completed": {
        const item = asRecord(event.item);
        const tool = itemTool(item);
        if (!tool) break;
        ensureToolInput(tool.id, tool.name, tool.input, tool.title);
        enqueue({
          type: "tool-output-available",
          toolCallId: tool.id,
          output: tool.output ?? item,
          providerExecuted: true,
          dynamic: true,
        });
        break;
      }
      case "turn_completed": {
        deps.onStep?.(null);
        break;
      }
      case "status": {
        deps.onStep?.(event.message);
        break;
      }
      case "error": {
        enqueue({ type: "error", errorText: event.message });
        break;
      }
    }
  };

  const stream = new ReadableStream<UIMessageChunk>({
    start(controller) {
      controllerRef = controller;
      enqueue({ type: "start" });
      enqueue({ type: "start-step" });
      deps.onStep?.(
        approval ? "Sending approval to Codex..." : "Starting Codex...",
      );

      const run = approval
        ? native.codexRespondApproval(
            {
              sessionId: options.chatId,
              requestId: Number(approval.approvalId),
              approved: approval.approved,
            },
            handleEvent,
          )
        : native.codexStartTurn(
            {
              sessionId: options.chatId,
              threadId,
              cwd: live.cwd,
              workspaceRoots: live.workspaceRoot ? [live.workspaceRoot] : [],
              workspace,
              prompt: prompt ?? "",
              cliPath: cliPath || null,
            },
            handleEvent,
          );

      run
        .then(async (result) => {
          if (approval) {
            processedApprovals.add(approvalKey(options.chatId, approval.approvalId));
            await saveCodexProcessedApprovalId(options.chatId, approval.approvalId);
          }
          await persistThreadId(options.chatId, result, scopeKey);
          closeText();
          enqueue({ type: "finish-step" });
          enqueue({ type: "finish", finishReason: "stop" });
          finished = true;
          controller.close();
        })
        .catch((error) => {
          closeText();
          enqueue({ type: "error", errorText: errorMessage(error) });
          enqueue({ type: "finish-step" });
          enqueue({ type: "finish", finishReason: "error" });
          finished = true;
          controller.close();
        });
    },
    cancel() {
      finished = true;
      void native.codexInterrupt(options.chatId).catch(() => {});
    },
  });

  options.abortSignal?.addEventListener(
    "abort",
    () => {
      finished = true;
      void native.codexInterrupt(options.chatId).catch(() => {});
    },
    { once: true },
  );

  return stream;
}

async function persistThreadId(
  sessionId: string,
  result: CodexRunResult,
  scopeKey: string,
): Promise<void> {
  if (result.threadId)
    await saveCodexThreadId(sessionId, result.threadId, scopeKey);
}

export function codexScopeKey(input: {
  cwd: string | null;
  workspaceRoot: string | null;
  workspace: ReturnType<typeof currentWorkspaceEnv>;
  cliPath: string;
}): string {
  return JSON.stringify({
    cwd: input.cwd ?? "",
    workspaceRoot: input.workspaceRoot ?? "",
    workspace: input.workspace,
    cliPath: input.cliPath.trim() || "codex",
  });
}

function latestUserText(messages: UIMessage[]): string {
  for (let i = messages.length - 1; i >= 0; i--) {
    const message = messages[i];
    if (message.role !== "user") continue;
    return message.parts
      .map((part) => (part.type === "text" ? part.text : ""))
      .join("\n")
      .trim();
  }
  return "";
}

export function latestApprovalResponse(
  messages: UIMessage[],
  sessionId: string,
  persistedApprovals: Set<string> = new Set(),
): { approvalId: string; approved: boolean } | null {
  for (let i = messages.length - 1; i >= 0; i--) {
    const message = messages[i];
    if (message.role === "user") return null;
    if (message.role !== "assistant") continue;
    for (let j = message.parts.length - 1; j >= 0; j--) {
      const part = asRecord(message.parts[j]);
      const approval = asRecord(part?.approval);
      const approvalId = str(approval?.id);
      if (part?.state !== "approval-responded" || !approvalId) continue;
      const key = approvalKey(sessionId, approvalId);
      if (processedApprovals.has(key) || persistedApprovals.has(key)) continue;
      return { approvalId, approved: approval?.approved === true };
    }
    return null;
  }
  return null;
}

function approvalKey(sessionId: string, approvalId: string): string {
  return `${sessionId}:${approvalId}`;
}

function itemTool(item: Record<string, unknown> | null):
  | {
      id: string;
      name: string;
      input: unknown;
      output?: unknown;
      title: string;
    }
  | null {
  if (!item) return null;
  const id = extractItemId(item);
  const type = str(item.type) ?? str(item.kind) ?? "item";
  if (/command/i.test(type)) {
    return {
      id,
      name: "codex_command",
      title: "Codex command",
      input: pickFields(item, ["command", "cmd", "cwd", "args"]),
      output: pickFields(item, ["stdout", "stderr", "exitCode", "exit_code", "status"]),
    };
  }
  if (/file|patch|diff/i.test(type)) {
    return {
      id,
      name: "codex_file_change",
      title: "Codex file change",
      input: pickFields(item, ["path", "file", "diff", "changes"]),
      output: pickFields(item, ["path", "file", "diff", "changes", "status"]),
    };
  }
  if (/approval/i.test(type)) {
    return genericTool(id, "codex_approval", item, "Codex approval");
  }
  return null;
}

function genericTool(
  id: string,
  name: string,
  input: unknown,
  title: string,
) {
  return { id, name, input, title };
}

function extractItemId(item: Record<string, unknown>): string {
  return (
    str(item.id) ??
    str(item.itemId) ??
    str(item.callId) ??
    `codex-item-${Math.random().toString(36).slice(2, 10)}`
  );
}

function pickFields(
  item: Record<string, unknown>,
  keys: readonly string[],
): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const key of keys) {
    if (item[key] != null) out[key] = item[key];
  }
  return Object.keys(out).length > 0 ? out : item;
}

function asRecord(value: unknown): Record<string, unknown> | null {
  return value && typeof value === "object"
    ? (value as Record<string, unknown>)
    : null;
}

function str(value: unknown): string | null {
  return typeof value === "string" && value.trim() ? value : null;
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
