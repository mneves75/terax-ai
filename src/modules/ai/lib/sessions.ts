import type { UIMessage } from "@ai-sdk/react";
import { LazyStore } from "@tauri-apps/plugin-store";

export type SessionMeta = {
  id: string;
  title: string;
  createdAt: number;
  updatedAt: number;
};

const STORE_PATH = "terax-ai-sessions.json";
const KEY_SESSIONS = "sessions";
const KEY_ACTIVE = "activeId";
const messagesKey = (id: string) => `messages:${id}`;
const codexThreadKey = (id: string) => `codexThread:${id}`;
const codexProcessedApprovalsKey = (id: string) =>
  `codexProcessedApprovals:${id}`;

type StoredCodexThread =
  | string
  | {
      threadId?: string;
      scopeKey?: string;
      updatedAt?: number;
    };

const store = new LazyStore(STORE_PATH, { defaults: {}, autoSave: 200 });

export type LoadedSessions = {
  sessions: SessionMeta[];
  activeId: string | null;
};

export async function loadAll(): Promise<LoadedSessions> {
  // One IPC roundtrip via entries() rather than two parallel get()s. Per-
  // session messages are loaded lazily via `loadMessages` only when a
  // session is opened, so cold boot stays at a single store call.
  const entries = await store.entries();
  let sessions: SessionMeta[] | undefined;
  let activeId: string | null | undefined;
  for (const [k, v] of entries) {
    if (k === KEY_SESSIONS) sessions = v as SessionMeta[];
    else if (k === KEY_ACTIVE) activeId = v as string | null;
  }
  return { sessions: sessions ?? [], activeId: activeId ?? null };
}

export async function loadMessages(id: string): Promise<UIMessage[] | null> {
  return (await store.get<UIMessage[]>(messagesKey(id))) ?? null;
}

export async function saveSessionsList(sessions: SessionMeta[]): Promise<void> {
  await store.set(KEY_SESSIONS, sessions);
}

export async function saveActiveId(id: string | null): Promise<void> {
  await store.set(KEY_ACTIVE, id);
}

export async function saveMessages(
  id: string,
  messages: UIMessage[],
): Promise<void> {
  await store.set(messagesKey(id), messages);
}

export async function deleteSessionData(id: string): Promise<void> {
  await store.delete(messagesKey(id));
  await store.delete(codexThreadKey(id));
  await store.delete(codexProcessedApprovalsKey(id));
}

export async function loadCodexThreadId(
  id: string,
  scopeKey?: string,
): Promise<string | null> {
  const value = await store.get<StoredCodexThread>(codexThreadKey(id));
  if (!value) return null;
  if (typeof value === "string") return scopeKey ? null : value;
  if (!value.threadId) return null;
  if (scopeKey && value.scopeKey !== scopeKey) return null;
  return value.threadId;
}

export async function saveCodexThreadId(
  id: string,
  threadId: string,
  scopeKey?: string,
): Promise<void> {
  await store.set(
    codexThreadKey(id),
    scopeKey ? { threadId, scopeKey, updatedAt: Date.now() } : threadId,
  );
}

export async function deleteCodexThreadId(id: string): Promise<void> {
  await store.delete(codexThreadKey(id));
}

export async function loadCodexProcessedApprovalIds(
  id: string,
): Promise<string[]> {
  return (await store.get<string[]>(codexProcessedApprovalsKey(id))) ?? [];
}

export async function saveCodexProcessedApprovalId(
  id: string,
  approvalId: string,
): Promise<void> {
  const key = codexProcessedApprovalsKey(id);
  const approvalKey = `${id}:${approvalId}`;
  const current = (await store.get<string[]>(key)) ?? [];
  if (current.includes(approvalKey)) return;
  await store.set(key, [...current.slice(-199), approvalKey]);
}

export function newSessionId(): string {
  return `s-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 8)}`;
}

export function deriveTitle(messages: UIMessage[]): string {
  for (const m of messages) {
    if (m.role !== "user") continue;
    for (const p of m.parts) {
      if (p.type !== "text") continue;
      const text = (p as { text: string }).text
        .replace(/<terminal-context[\s\S]*?<\/terminal-context>\s*/g, "")
        .replace(/<selection[\s\S]*?<\/selection>\s*/g, "")
        .replace(/<file[\s\S]*?<\/file>\s*/g, "")
        .trim();
      if (!text) continue;
      const first = text.split("\n")[0].trim();
      return first.length > 40 ? `${first.slice(0, 40)}…` : first;
    }
  }
  return "New chat";
}
