import type { UIMessage } from "@ai-sdk/react";
import { describe, expect, it } from "vitest";

import { codexScopeKey, latestApprovalResponse } from "./codexTransport";

function approvalMessage(id: string): UIMessage {
  return {
    id: "a1",
    role: "assistant",
    parts: [
      {
        type: "dynamic-tool",
        state: "approval-responded",
        approval: { id, approved: true },
      },
    ],
  } as unknown as UIMessage;
}

describe("latestApprovalResponse", () => {
  it("does not replay an approval when a newer user prompt exists", () => {
    const messages = [
      approvalMessage("approval-1"),
      { id: "u1", role: "user", parts: [{ type: "text", text: "next" }] },
    ] as UIMessage[];

    expect(latestApprovalResponse(messages, "s1")).toBeNull();
  });

  it("does not replay persisted processed approvals", () => {
    const processed = new Set(["s1:approval-1"]);
    expect(
      latestApprovalResponse([approvalMessage("approval-1")], "s1", processed),
    ).toBeNull();
  });

  it("keeps persisted thread ids scoped to workspace and CLI", () => {
    const base = {
      cwd: "/repo",
      workspaceRoot: "/repo",
      workspace: { kind: "local" } as const,
      cliPath: "codex",
    };

    expect(codexScopeKey(base)).toBe(codexScopeKey({ ...base }));
    expect(codexScopeKey(base)).not.toBe(
      codexScopeKey({ ...base, workspaceRoot: "/other", cwd: "/other" }),
    );
    expect(codexScopeKey(base)).not.toBe(
      codexScopeKey({ ...base, cliPath: "/opt/codex" }),
    );
  });
});
