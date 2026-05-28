import { spawn } from "node:child_process";

const cwd = process.cwd();
const expected = "TERAX_E2E_OK";
const timeoutMs = Number.parseInt(
  process.env.TERAX_CODEX_E2E_TIMEOUT_MS ?? "90000",
  10,
);

const child = spawn("codex", ["app-server", "--listen", "stdio://"], {
  cwd,
  stdio: ["pipe", "pipe", "pipe"],
});

let nextId = 1;
let buffer = "";
let stderr = "";
let completed = false;
const pending = new Map();
const methods = [];
const deltas = [];
let turnStartedAt = 0;
let firstDeltaAt = 0;
let completedAt = 0;

const timeout = setTimeout(() => {
  child.kill("SIGTERM");
  fail("timeout");
}, timeoutMs);

child.stderr.on("data", (chunk) => {
  stderr += chunk.toString();
});

child.stdout.on("data", (chunk) => {
  buffer += chunk.toString();
  for (;;) {
    const idx = buffer.indexOf("\n");
    if (idx === -1) break;
    const line = buffer.slice(0, idx).trim();
    buffer = buffer.slice(idx + 1);
    if (!line) continue;
    handleMessage(JSON.parse(line));
  }
});

try {
  const init = await request("initialize", {
    clientInfo: {
      name: "terax-e2e",
      title: "Terax E2E",
      version: "0.7.3",
    },
    capabilities: { experimentalApi: true },
  });
  notify("initialized");

  const threadResult = await request("thread/start", {
    serviceName: "terax-e2e",
    sessionStartSource: "startup",
    cwd,
    runtimeWorkspaceRoots: [cwd],
  });
  const threadId = threadResult.thread?.id;
  if (!threadId) throw new Error("missing thread id");

  turnStartedAt = Date.now();
  const turnResult = await request("turn/start", {
    threadId,
    input: [
      {
        type: "text",
        text: `Reply with exactly ${expected}. Do not run shell commands and do not edit files.`,
        text_elements: [],
      },
    ],
    cwd,
    runtimeWorkspaceRoots: [cwd],
  });

  const startedAt = Date.now();
  while (!completed && Date.now() - startedAt < timeoutMs - 5000) {
    await sleep(250);
  }

  clearTimeout(timeout);
  child.kill("SIGTERM");
  const text = deltas.join("");
  const ok = completed && text.includes(expected);
  const result = {
    ok,
    codexHome: init.codexHome,
    platformOs: init.platformOs,
    threadId,
    turnId: turnResult.turn?.id ?? null,
    completed,
    timeToFirstDeltaMs: firstDeltaAt ? firstDeltaAt - turnStartedAt : null,
    timeToCompletedMs: completedAt ? completedAt - turnStartedAt : null,
    text,
    methods,
  };
  console.log(JSON.stringify(result, null, 2));
  process.exit(ok ? 0 : 2);
} catch (error) {
  clearTimeout(timeout);
  child.kill("SIGTERM");
  fail(error instanceof Error ? error.message : String(error));
}

function request(method, params) {
  const id = nextId++;
  child.stdin.write(JSON.stringify({ method, id, params }) + "\n");
  return new Promise((resolve, reject) => {
    pending.set(id, { resolve, reject });
  });
}

function notify(method, params = {}) {
  child.stdin.write(JSON.stringify({ method, params }) + "\n");
}

function handleMessage(message) {
  if (message.id && pending.has(message.id)) {
    const callbacks = pending.get(message.id);
    pending.delete(message.id);
    if (message.error) callbacks.reject(new Error(JSON.stringify(message.error)));
    else callbacks.resolve(message.result ?? {});
    return;
  }
  if (!message.method) return;
  methods.push(message.method);
  if (
    message.method === "item/agentMessage/delta" &&
    typeof message.params?.delta === "string"
  ) {
    if (!firstDeltaAt) firstDeltaAt = Date.now();
    deltas.push(message.params.delta);
  }
  if (message.method === "turn/completed") {
    completed = true;
    completedAt = Date.now();
  }
}

function fail(reason) {
  console.error(
    JSON.stringify(
      {
        ok: false,
        reason,
        completed,
        text: deltas.join(""),
        methods,
        stderr,
      },
      null,
      2,
    ),
  );
  process.exit(1);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
