import { Database } from "bun:sqlite";
import { afterAll, describe, expect, test } from "bun:test";
import { spawn, spawnSync } from "node:child_process";
import {
  appendFileSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { conversationsBinary } from "./helpers/conversations-bin";
import { herdrSocketEndpoint, socketConsistentEnv, testEnv } from "./helpers/test-env";

/**
 * Contract tests for the NATIVE rebuildup.conversations implementation
 * (Rust, Issue #293). These spawn the shipped binary —
 * `plugins/conversations/bin/conversations` (committed prebuilt,
 * ADR-0035) with a cargo-built fallback — against the same fake herdr
 * socket protocol and transcript fixtures the retired TS suite used, so
 * wire behavior is verified end-to-end without Bun or a live session.
 *
 * Coverage mirrors the retired TS-internal tests at contract level:
 * capability, claude/codex/opencode index + normalization, incremental
 * rescans, explicit skip/error reasons, stale-generation marking,
 * history/search/show/usage paging (incl. the #306 cursor fix), and the
 * non-TTY `tui` fallback. Rust unit coverage lives in the crate
 * (indexer incremental/guard scans, claude cwd encoding + UTF-16
 * bounding); TS↔Rust output parity is evidenced by the diff harness
 * documented in docs/research/conversations-native-rust.md.
 */

const CONV_BIN = conversationsBinary();

// --- env / spawn plumbing (same hygiene as the retired TS suite) -----------

const dirs: string[] = [];
const savedEnv: Record<string, string | undefined> = {};
for (const key of [
  "HERDR_PLUGIN_STATE_DIR",
  "HERDR_PLUGIN_CONFIG_DIR",
  "HERDR_SOCKET_PATH",
  "HERDR_SESSION",
  "CLAUDE_CONFIG_DIR",
  "CODEX_HOME",
  "OPENCODE_DATA_DIR",
  "XDG_DATA_HOME",
]) {
  savedEnv[key] = process.env[key];
}

function tempDir(): string {
  const dir = mkdtempSync(path.join(tmpdir(), "conversations-native-"));
  dirs.push(dir);
  return dir;
}

afterAll(() => {
  for (const dir of dirs) rmSync(dir, { recursive: true, force: true });
  for (const [key, value] of Object.entries(savedEnv)) {
    if (value === undefined) delete process.env[key];
    else process.env[key] = value;
  }
});

const DEAD_SOCKET = path.join(tempDir(), "no-such-herdr.sock");

const SESSION_ID = "3bc481ff-60ac-491a-abd0-07c408c85e71";
const SESSION_ID_2 = "11111111-2222-3333-4444-555555555555";
const CODEX_SESSION = "01a1f843-77fa-7d30-b3c1-19f822a13215";
const OPENCODE_SESSION = "ses_test123abc";
const CWD = "/repo/proj";

interface CliResult {
  status: number;
  stdout: string;
  stderr: string;
  json: Record<string, unknown> | null;
  error: Record<string, unknown> | null;
}

function cliEnv(extra?: Record<string, string>): Record<string, string> {
  return testEnv({ HERDR_SOCKET_PATH: DEAD_SOCKET, ...extra });
}

function parseCliOutput(stdout: string, stderr: string, status: number): CliResult {
  let json: Record<string, unknown> | null = null;
  let error: Record<string, unknown> | null = null;
  try {
    json = JSON.parse(stdout) as Record<string, unknown>;
  } catch {
    /* usage output */
  }
  try {
    error = (JSON.parse(stderr) as { error?: Record<string, unknown> }).error ?? null;
  } catch {
    /* empty stderr */
  }
  return { status, stdout, stderr, json, error };
}

function runCli(args: string[], extraEnv?: Record<string, string>): CliResult {
  const result = spawnSync(CONV_BIN, args, {
    encoding: "utf8",
    env: cliEnv(socketConsistentEnv(args, extraEnv)),
    timeout: 30_000,
  });
  return parseCliOutput(result.stdout, result.stderr, result.status ?? -1);
}

function runCliAsync(args: string[], extraEnv?: Record<string, string>): Promise<CliResult> {
  return new Promise((resolve, reject) => {
    const child = spawn(CONV_BIN, args, {
      env: cliEnv(socketConsistentEnv(args, extraEnv)),
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString("utf8");
    });
    child.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
    });
    child.on("error", reject);
    child.on("close", (code) => resolve(parseCliOutput(stdout, stderr, code ?? -1)));
  });
}

// --- fake herdr socket --------------------------------------------------------

interface FakeAgent {
  pane_id: string;
  agent?: string;
  agent_status?: string;
  state_change_seq?: number;
  cwd?: string;
  agent_session?: { agent?: string; kind?: string; value?: string } | null;
}

interface FakePane {
  pane_id: string;
  workspace_id?: string;
  tab_id?: string;
  cwd?: string;
}

interface FakeHerdrState {
  panes: FakePane[];
  agents: FakeAgent[];
}

/** Fake herdr socket answering session.snapshot from live-mutated state. */
async function fakeHerdr(state: FakeHerdrState): Promise<{
  socketPath: string;
  setAgents(agents: FakeAgent[]): void;
  setPanes(panes: FakePane[]): void;
  close(): Promise<void>;
}> {
  const socketPath = path.join(tempDir(), "fake-herdr.sock");
  const server = net.createServer((conn) => {
    let buffer = "";
    conn.on("data", (chunk: Buffer) => {
      buffer += chunk.toString("utf8");
      let newline = buffer.indexOf("\n");
      while (newline !== -1) {
        const line = buffer.slice(0, newline);
        buffer = buffer.slice(newline + 1);
        newline = buffer.indexOf("\n");
        if (!line.trim()) continue;
        const message = JSON.parse(line) as { id?: string; method?: string };
        const result =
          message.method === "session.snapshot"
            ? {
                snapshot: {
                  session: { name: "test-session" },
                  panes: state.panes,
                  agents: state.agents,
                },
              }
            : {};
        conn.write(`${JSON.stringify({ id: message.id, result })}\n`);
      }
    });
  });
  await new Promise<void>((resolveListen, rejectListen) => {
    server.once("error", rejectListen);
    server.listen(herdrSocketEndpoint(socketPath), () => resolveListen());
  });
  return {
    socketPath,
    setAgents(next: FakeAgent[]) {
      state.agents = next;
    },
    setPanes(next: FakePane[]) {
      state.panes = next;
    },
    close: () => new Promise((resolveClose) => server.close(() => resolveClose())),
  };
}

function herdrFor(claudeSessions: { pane: string; sessionId: string; cwd?: string }[]) {
  return fakeHerdr({
    panes: claudeSessions.map((s) => ({
      pane_id: s.pane,
      workspace_id: "w1",
      tab_id: "w1:t1",
      cwd: s.cwd ?? CWD,
    })),
    agents: claudeSessions.map((s) => ({
      pane_id: s.pane,
      agent: "claude",
      agent_status: "idle",
      state_change_seq: 1,
      cwd: s.cwd ?? CWD,
      agent_session: { agent: "claude", kind: "session", value: s.sessionId },
    })),
  });
}

// --- transcript fixtures ------------------------------------------------------

/** Same encoding as the retired TS `encodeClaudeCwd` (UTF-16-unit regex). */
function encodeClaudeCwd(cwd: string): string {
  return cwd.replace(/[^A-Za-z0-9]/g, "-");
}

function writeTranscript(
  claudeHome: string,
  cwd: string,
  sessionId: string,
  lines: unknown[],
): string {
  const dir = path.join(claudeHome, "projects", encodeClaudeCwd(cwd));
  mkdirSync(dir, { recursive: true });
  const file = path.join(dir, `${sessionId}.jsonl`);
  writeFileSync(
    file,
    lines.map((l) => (typeof l === "string" ? l : JSON.stringify(l))).join("\n") + "\n",
  );
  return file;
}

function userLine(uuid: string, text: string, second = 0): unknown {
  return {
    type: "user",
    uuid,
    timestamp: `2026-01-01T00:00:${String(second).padStart(2, "0")}.000Z`,
    message: { role: "user", content: text },
  };
}

function assistantLine(uuid: string, text: string, second: number): unknown {
  return {
    type: "assistant",
    uuid,
    timestamp: `2026-01-01T00:00:${String(second).padStart(2, "0")}.000Z`,
    message: {
      id: `msg-${uuid}`,
      role: "assistant",
      model: "claude-test-1",
      content: [
        { type: "text", text },
        { type: "tool_use", id: `tool-${uuid}`, name: "Bash", input: { command: "ls" } },
      ],
      usage: { input_tokens: 10, output_tokens: 5 },
    },
  };
}

function toolResultLine(uuid: string, toolUseId: string, second: number): unknown {
  return {
    type: "user",
    uuid,
    timestamp: `2026-01-01T00:00:${String(second).padStart(2, "0")}.000Z`,
    message: {
      role: "user",
      content: [{ type: "tool_result", tool_use_id: toolUseId, content: "out", is_error: false }],
    },
  };
}

/** Two-turn fixture with searchable text and usage-bearing assistant events (t3). */
function needleFixtureLines(): unknown[] {
  return [
    userLine("u-1", "please fix the needle bug", 1),
    assistantLine("a-1", "working on the needle", 2),
    toolResultLine("u-2", "tool-a-1", 3),
    {
      type: "system",
      subtype: "compact_boundary",
      uuid: "s-1",
      timestamp: "2026-01-01T00:00:04.000Z",
    },
    userLine("u-3", "second prompt about haystack", 5),
    assistantLine("a-2", "done with needle cleanup", 6),
    toolResultLine("u-4", "tool-a-2", 7),
    "{not valid json",
  ];
}

/** Richer single-turn fixture with tool_use + usage details (t1). */
function richFixtureLines(): unknown[] {
  return [
    { type: "mode", mode: "normal", sessionId: SESSION_ID }, // bookkeeping: skipped
    {
      type: "user",
      uuid: "u-1",
      timestamp: "2026-01-01T00:00:01.000Z",
      sessionId: SESSION_ID,
      cwd: CWD,
      message: { role: "user", content: "please fix the bug" },
    },
    {
      type: "assistant",
      uuid: "a-1",
      timestamp: "2026-01-01T00:00:02.000Z",
      sessionId: SESSION_ID,
      message: {
        id: "msg-1",
        role: "assistant",
        model: "claude-test-1",
        content: [
          { type: "text", text: "on it" },
          { type: "tool_use", id: "tool-1", name: "Bash", input: { command: "ls" } },
        ],
        usage: {
          input_tokens: 10,
          output_tokens: 5,
          cache_read_input_tokens: 3,
          cache_creation_input_tokens: 2,
          output_tokens_details: { thinking_tokens: 1 },
        },
      },
    },
    {
      type: "user",
      uuid: "u-2",
      timestamp: "2026-01-01T00:00:03.000Z",
      sessionId: SESSION_ID,
      message: {
        role: "user",
        content: [
          { type: "tool_result", tool_use_id: "tool-1", content: "a.txt\n", is_error: false },
        ],
      },
    },
    {
      type: "system",
      subtype: "compact_boundary",
      uuid: "s-1",
      timestamp: "2026-01-01T00:00:04.000Z",
    },
    "{not valid json",
    { type: "attachment", attachment: { type: "hook_success" } }, // bookkeeping: skipped
  ];
}

function eventLines(stateDir: string): Record<string, unknown>[] {
  const eventsDir = path.join(stateDir, "events");
  const files = readdirSync(eventsDir);
  expect(files.length).toBe(1);
  return readFileSync(path.join(eventsDir, files[0] as string), "utf8")
    .split("\n")
    .filter((l) => l.trim() !== "")
    .map((l) => JSON.parse(l) as Record<string, unknown>);
}

function setupEnv(): {
  stateDir: string;
  claudeHome: string;
  codexHome: string;
  opencodeDir: string;
  extra: Record<string, string>;
} {
  const stateDir = tempDir();
  const claudeHome = tempDir();
  const codexHome = tempDir();
  const opencodeDir = tempDir();
  return {
    stateDir,
    claudeHome,
    codexHome,
    opencodeDir,
    extra: {
      HERDR_PLUGIN_STATE_DIR: stateDir,
      HERDR_PLUGIN_CONFIG_DIR: path.join(stateDir, "config"),
      CLAUDE_CONFIG_DIR: claudeHome,
      CODEX_HOME: codexHome,
      OPENCODE_DATA_DIR: opencodeDir,
      XDG_DATA_HOME: path.join(stateDir, "xdg"), // nonexistent → no ambient opencode
    },
  };
}

// --- codex fixtures ------------------------------------------------------------

function codexRolloutLines(): unknown[] {
  const ts = (s: number) => `2026-01-01T00:00:${String(s).padStart(2, "0")}.000Z`;
  return [
    {
      timestamp: ts(0),
      ordinal: 0,
      type: "session_meta",
      payload: { id: CODEX_SESSION, cwd: CWD, cli_version: "0.169.0" },
    },
    {
      timestamp: ts(1),
      ordinal: 1,
      type: "turn_context",
      payload: { turn_id: "t1", model: "gpt-6-test", cwd: CWD },
    },
    {
      timestamp: ts(2),
      ordinal: 2,
      type: "event_msg",
      payload: { type: "task_started", turn_id: "t1" },
    },
    {
      timestamp: ts(3),
      ordinal: 3,
      type: "response_item",
      payload: {
        type: "message",
        id: "msg_u1",
        role: "user",
        content: [{ type: "input_text", text: "fix the bug" }],
      },
    },
    {
      timestamp: ts(4),
      ordinal: 4,
      type: "event_msg",
      payload: {
        type: "item_completed",
        turn_id: "t1",
        item: {
          type: "UserMessage",
          id: "item-u1",
          content: [{ type: "text", text: "fix the bug" }],
        },
      },
    },
    {
      timestamp: ts(5),
      ordinal: 5,
      type: "response_item",
      payload: {
        type: "function_call",
        id: "ctc_1",
        call_id: "call_1",
        name: "shell",
        arguments: '{"command":"ls"}',
      },
    },
    {
      timestamp: ts(6),
      ordinal: 6,
      type: "response_item",
      payload: {
        type: "function_call_output",
        id: "ctco_1",
        call_id: "call_1",
        output: [{ type: "input_text", text: "a.txt" }],
      },
    },
    {
      timestamp: ts(7),
      ordinal: 7,
      type: "response_item",
      payload: { type: "reasoning", id: "rs_1", summary: [] },
    },
    {
      timestamp: ts(8),
      ordinal: 8,
      type: "event_msg",
      payload: {
        type: "item_completed",
        turn_id: "t1",
        item: {
          type: "AgentMessage",
          id: "item-a1",
          content: [{ type: "text", text: "done" }],
        },
      },
    },
    {
      timestamp: ts(9),
      ordinal: 9,
      type: "event_msg",
      payload: {
        type: "token_count",
        info: {
          last_token_usage: {
            input_tokens: 100,
            cached_input_tokens: 40,
            cache_write_input_tokens: 0,
            output_tokens: 10,
            reasoning_output_tokens: 5,
            total_tokens: 110,
          },
          total_token_usage: {
            input_tokens: 100,
            cached_input_tokens: 40,
            cache_write_input_tokens: 0,
            output_tokens: 10,
            reasoning_output_tokens: 5,
            total_tokens: 110,
          },
        },
      },
    },
    {
      timestamp: ts(10),
      ordinal: 10,
      type: "event_msg",
      payload: { type: "task_complete", turn_id: "t1" },
    },
    "{not valid json",
  ];
}

function writeCodexHome(codexHome: string): string {
  const dir = path.join(codexHome, "sessions", "2026", "01", "01");
  mkdirSync(dir, { recursive: true });
  const rolloutPath = path.join(dir, `rollout-2026-01-01T00-00-00-${CODEX_SESSION}.jsonl`);
  writeFileSync(
    rolloutPath,
    codexRolloutLines()
      .map((l) => (typeof l === "string" ? l : JSON.stringify(l)))
      .join("\n") + "\n",
  );
  return rolloutPath;
}

// --- opencode fixtures -----------------------------------------------------------

const OC_T0 = 1767225600000; // 2026-01-01T00:00:00Z

function ocData(assistantContent: unknown[], extra: Record<string, unknown> = {}): string {
  return JSON.stringify({
    time: { created: OC_T0 + 1, completed: OC_T0 + 2 },
    agent: "build",
    model: { id: "gpt-5.4-mini", providerID: "openai" },
    content: assistantContent,
    finish: "stop",
    cost: 0.01,
    tokens: { input: 100, output: 20, reasoning: 5, cache: { read: 50, write: 0 } },
    ...extra,
  });
}

function writeOpenCodeDb(dir: string): string {
  const dbPath = path.join(dir, "opencode.db");
  const db = new Database(dbPath);
  db.run("CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT, time_created INTEGER)");
  db.run(
    "CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER, time_created INTEGER, time_updated INTEGER, data TEXT)",
  );
  const insertSession = db.query(
    "INSERT INTO session_v2 (id, directory, time_created) VALUES (?, ?, ?)",
  );
  const insertMsg = db.query(
    "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?, ?, ?)",
  );
  insertSession.run(OPENCODE_SESSION, CWD, OC_T0);
  insertMsg.run(
    "msg_u1",
    OPENCODE_SESSION,
    "user",
    1,
    OC_T0 + 1,
    OC_T0 + 1,
    JSON.stringify({ time: { created: OC_T0 + 1 }, text: "hello opencode" }),
  );
  insertMsg.run(
    "msg_a1",
    OPENCODE_SESSION,
    "assistant",
    2,
    OC_T0 + 2,
    OC_T0 + 3,
    ocData([
      { type: "reasoning", text: "thinking…" },
      { type: "text", text: "hi there" },
      {
        type: "tool",
        id: "call-1",
        name: "bash",
        state: {
          status: "completed",
          input: { command: "ls" },
          output: [{ type: "text", text: "file.txt" }],
        },
      },
      {
        type: "tool",
        id: "call-2",
        name: "read",
        state: { status: "error", input: { path: "x" }, error: "boom" },
      },
    ]),
  );
  insertMsg.run(
    "msg_i1",
    OPENCODE_SESSION,
    "idle",
    3,
    OC_T0 + 4,
    OC_T0 + 4,
    JSON.stringify({ time: { created: OC_T0 + 4 }, outcome: "succeeded" }),
  );
  insertMsg.run(
    "msg_c1",
    OPENCODE_SESSION,
    "compaction",
    4,
    OC_T0 + 5,
    OC_T0 + 5,
    JSON.stringify({ time: { created: OC_T0 + 5 } }),
  );
  insertMsg.run("msg_bad", OPENCODE_SESSION, "assistant", 5, OC_T0 + 6, OC_T0 + 6, "{corrupt json");
  db.close();
  return dbPath;
}

function writeOpenCodeDbV1(dir: string): string {
  const dbPath = path.join(dir, "opencode.db");
  const db = new Database(dbPath);
  db.run("CREATE TABLE session (id TEXT PRIMARY KEY, directory TEXT)");
  db.run(
    "CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT)",
  );
  db.run(
    "CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT, time_created INTEGER, time_updated INTEGER, data TEXT)",
  );
  db.query("INSERT INTO session (id, directory) VALUES (?, ?)").run(OPENCODE_SESSION, CWD);
  db.query(
    "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?)",
  ).run(
    "msg_v1u",
    OPENCODE_SESSION,
    OC_T0,
    OC_T0,
    JSON.stringify({ role: "user", time: { created: OC_T0 } }),
  );
  db.query(
    "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?, ?)",
  ).run(
    "prt_v1u1",
    "msg_v1u",
    OPENCODE_SESSION,
    OC_T0,
    OC_T0,
    JSON.stringify({ type: "text", text: "v1 prompt" }),
  );
  db.query(
    "INSERT INTO message (id, session_id, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?)",
  ).run(
    "msg_v1a",
    OPENCODE_SESSION,
    OC_T0 + 1,
    OC_T0 + 2,
    JSON.stringify({
      role: "assistant",
      modelID: "claude-sonnet-4-5",
      providerID: "anthropic",
      cost: 0.02,
      tokens: { input: 10, output: 4, reasoning: 0, cache: { read: 0, write: 0 } },
      time: { created: OC_T0 + 1 },
    }),
  );
  db.query(
    "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?, ?)",
  ).run(
    "prt_v1a1",
    "msg_v1a",
    OPENCODE_SESSION,
    OC_T0 + 1,
    OC_T0 + 2,
    JSON.stringify({ type: "text", text: "v1 reply" }),
  );
  db.query(
    "INSERT INTO part (id, message_id, session_id, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?, ?)",
  ).run(
    "prt_v1a2",
    "msg_v1a",
    OPENCODE_SESSION,
    OC_T0 + 2,
    OC_T0 + 2,
    JSON.stringify({
      type: "tool",
      tool: "bash",
      callID: "call-v1",
      state: { status: "completed", input: { command: "pwd" }, output: "ok" },
    }),
  );
  db.close();
  return dbPath;
}

const t = (name: string, fn: () => void | Promise<void>) => test(name, fn, 30_000);

// --- capability -----------------------------------------------------------------

describe("conversations native capability", () => {
  t("capability advertises the v1 contract and provider support", () => {
    const r = runCli(["capability"]);
    expect(r.status).toBe(0);
    expect(r.json?.contract).toBe("herdr-plugins.conversations/v1");
    expect(r.json?.command).toBe("capability");
    const providers = r.json?.providers as { id: string; status: string }[];
    expect(providers.find((p) => p.id === "claude")?.status).toBe("supported");
    expect(providers.find((p) => p.id === "codex")?.status).toBe("supported");
    expect(providers.find((p) => p.id === "opencode")?.status).toBe("supported");
    const commands = r.json?.commands as string[];
    for (const c of ["search", "show", "usage"]) expect(commands).toContain(c);
    expect(r.json?.mutation_commands).toEqual(["index", "refresh"]);
  });
});

// --- claude end-to-end ------------------------------------------------------------

describe("conversations native claude", () => {
  t("index normalizes a claude fixture end-to-end", async () => {
    const { stateDir, claudeHome, extra } = setupEnv();
    writeTranscript(claudeHome, CWD, SESSION_ID, richFixtureLines());
    const herdr = await fakeHerdr({
      panes: [{ pane_id: "w1:p1", workspace_id: "w1", tab_id: "w1:t1", cwd: CWD }],
      agents: [
        {
          pane_id: "w1:p1",
          agent: "claude",
          agent_status: "idle",
          state_change_seq: 7,
          cwd: CWD,
          agent_session: { agent: "claude", kind: "session", value: SESSION_ID },
        },
      ],
    });
    try {
      const r = await runCliAsync(["index", "--socket", herdr.socketPath], extra);
      expect(r.status).toBe(0);
      const indexed = r.json?.indexed as Record<string, unknown>[];
      expect(indexed.length).toBe(1);
      expect(indexed[0]?.events_appended).toBe(6);
      const events = eventLines(stateDir);
      const types = events.map((e) => e.type);
      expect(types).toEqual([
        "turn",
        "user_message",
        "assistant_message",
        "tool_call",
        "tool_result",
        "system",
      ]);
      const assistant = events.find((e) => e.type === "assistant_message");
      expect(assistant?.model).toBe("claude-test-1");
      expect(assistant?.usage).toEqual({
        input_tokens: 10,
        output_tokens: 5,
        cache_read_input_tokens: 3,
        cache_creation_input_tokens: 2,
        thinking_tokens: 1,
      });
      const identity = events[0]?.identity as Record<string, unknown>;
      expect(identity.pane_id).toBe("w1:p1");
      expect(identity.workspace_id).toBe("w1");
      expect(identity.herdr_session).toBe("test-session");
      expect(identity.state_change_seq).toBe(7);
      expect((identity.agent_session as Record<string, unknown>).value).toBe(SESSION_ID);
      const toolCall = events.find((e) => e.type === "tool_call");
      expect(toolCall?.tool_name).toBe("Bash");
      expect(toolCall?.tool_use_id).toBe("tool-1");
      expect(events[1]?.turn_index).toBe(1);
      expect(events[4]?.turn_index).toBe(1);
    } finally {
      await herdr.close();
    }
  });

  t("index is incremental across runs", async () => {
    const { stateDir, claudeHome, extra } = setupEnv();
    const file = writeTranscript(claudeHome, CWD, SESSION_ID, richFixtureLines().slice(0, 4));
    const herdr = await fakeHerdr({
      panes: [{ pane_id: "w1:p1", cwd: CWD }],
      agents: [
        {
          pane_id: "w1:p1",
          agent: "claude",
          agent_session: { value: SESSION_ID },
        },
      ],
    });
    try {
      const first = await runCliAsync(["index", "--socket", herdr.socketPath], extra);
      expect(first.status).toBe(0);
      const firstCount = eventLines(stateDir).length;
      appendFileSync(
        file,
        JSON.stringify({
          type: "user",
          uuid: "u-3",
          timestamp: "2026-01-01T00:01:00Z",
          message: { role: "user", content: "second prompt" },
        }) + "\n",
      );
      const second = await runCliAsync(["refresh", "--socket", herdr.socketPath], extra);
      expect(second.status).toBe(0);
      const indexed = second.json?.indexed as Record<string, unknown>[];
      expect(indexed[0]?.events_appended).toBe(2); // turn + user_message
      expect(indexed[0]?.reindexed).toBe(false);
      const all = eventLines(stateDir);
      expect(all.length).toBe(firstCount + 2);
      const lastTurn = all.filter((e) => e.type === "turn").at(-1);
      expect(lastTurn?.turn_index).toBe(2);
    } finally {
      await herdr.close();
    }
  });

  t(
    "unknown provider, missing session, and missing transcript are explicit, not success",
    async () => {
      const { claudeHome, codexHome, extra } = setupEnv();
      writeTranscript(claudeHome, CWD, SESSION_ID, richFixtureLines());
      const herdr = await fakeHerdr({
        panes: [
          { pane_id: "w1:p1", cwd: CWD },
          { pane_id: "w1:p2", cwd: CWD },
          { pane_id: "w1:p3", cwd: CWD },
          { pane_id: "w1:p4", cwd: CWD },
        ],
        agents: [
          { pane_id: "w1:p1", agent: "claude", agent_session: { value: SESSION_ID } },
          { pane_id: "w1:p2", agent: "codex", agent_session: { value: "codex-rollout-1" } },
          { pane_id: "w1:p3", agent: "claude", agent_session: { value: "no-such-session" } },
          { pane_id: "w1:p4", agent: "gemini", agent_session: { value: "gem-1" } },
        ],
      });
      try {
        const r = await runCliAsync(["index", "--socket", herdr.socketPath], extra);
        expect(r.status).toBe(0);
        const skipped = r.json?.skipped as Record<string, unknown>[];
        const reasons = skipped.map((s) => s.reason);
        expect(reasons).toContain("unknown_provider");
        expect(reasons).toContain("transcript_missing");
        expect(reasons.filter((x) => x === "transcript_missing").length).toBe(2);
        const unknown = skipped.find((s) => s.reason === "unknown_provider");
        expect(unknown?.pane_id).toBe("w1:p4");
        const codex = skipped.find((s) => s.provider === "codex");
        expect(codex?.reason).toBe("transcript_missing");
        const sessions = await runCliAsync(["sessions", "--socket", herdr.socketPath], extra);
        const list = sessions.json?.sessions as Record<string, unknown>[];
        expect(list.length).toBe(1);
        expect(list[0]?.provider).toBe("claude");
        expect(codexHome).toBeTruthy();
      } finally {
        await herdr.close();
      }
    },
  );

  t("pane reuse / generation change marks the index stale", async () => {
    const { claudeHome, extra } = setupEnv();
    writeTranscript(claudeHome, CWD, SESSION_ID, richFixtureLines());
    const state: FakeHerdrState = {
      panes: [{ pane_id: "w1:p1", cwd: CWD }],
      agents: [{ pane_id: "w1:p1", agent: "claude", agent_session: { value: SESSION_ID } }],
    };
    const herdr = await fakeHerdr(state);
    try {
      const first = await runCliAsync(["index", "--socket", herdr.socketPath], extra);
      expect(first.status).toBe(0);

      herdr.setAgents([
        { pane_id: "w1:p1", agent: "claude", agent_session: { value: "different-session" } },
      ]);
      const sessions = await runCliAsync(["sessions", "--socket", herdr.socketPath], extra);
      expect(sessions.status).toBe(0);
      const list = sessions.json?.sessions as Record<string, unknown>[];
      const entry = list.find((s) => s.session_id === SESSION_ID);
      expect(entry?.stale).toBe(true);
      expect(entry?.stale_reason).toBe("agent_session_changed");
    } finally {
      await herdr.close();
    }
  });

  t("sessions --offline lists indexed transcripts without a socket", async () => {
    const { claudeHome, extra } = setupEnv();
    writeTranscript(claudeHome, CWD, SESSION_ID, richFixtureLines());
    const herdr = await fakeHerdr({
      panes: [{ pane_id: "w1:p1", cwd: CWD }],
      agents: [{ pane_id: "w1:p1", agent: "claude", agent_session: { value: SESSION_ID } }],
    });
    try {
      await runCliAsync(["index", "--socket", herdr.socketPath], extra);
    } finally {
      await herdr.close();
    }
    const r = runCli(["sessions", "--offline"], extra);
    expect(r.status).toBe(0);
    const list = r.json?.sessions as Record<string, unknown>[];
    expect(list.length).toBe(1);
    expect(list[0]?.live).toBeNull();
    expect(list[0]?.stale).toBe(false);
    expect(list[0]?.event_count).toBe(6);
  });
});

// --- codex -------------------------------------------------------------------------

describe("conversations native codex", () => {
  t("index + history for a codex pane, with paging", async () => {
    const { codexHome, extra } = setupEnv();
    writeCodexHome(codexHome);
    const herdr = await fakeHerdr({
      panes: [{ pane_id: "w1:p1", workspace_id: "w1", tab_id: "w1:t1", cwd: CWD }],
      agents: [
        {
          pane_id: "w1:p1",
          agent: "codex",
          agent_status: "idle",
          state_change_seq: 3,
          cwd: CWD,
          agent_session: { agent: "codex", kind: "session", value: CODEX_SESSION },
        },
      ],
    });
    try {
      const idx = await runCliAsync(["index", "--socket", herdr.socketPath], extra);
      expect(idx.status).toBe(0);
      const indexed = idx.json?.indexed as Record<string, unknown>[];
      expect(indexed.length).toBe(1);
      expect(indexed[0]?.provider).toBe("codex");
      expect(indexed[0]?.malformed_lines).toBe(1);

      const h1 = await runCliAsync(["history", "--pane", "w1:p1", "--offline"], extra);
      expect(h1.status).toBe(0);
      const events = h1.json?.events as Record<string, unknown>[];
      expect(events.map((e) => e.type)).toEqual([
        "turn",
        "user_message",
        "tool_call",
        "tool_result",
        "assistant_message",
      ]);
      const page = h1.json?.page as Record<string, unknown>;
      expect(page.returned).toBe(5);
      expect(page.has_more).toBe(false);
      const session = h1.json?.session as Record<string, unknown>;
      expect(session.provider).toBe("codex");
      expect(session.stale).toBe(false);
      expect(session.events_present).toBe(true);

      const p1 = await runCliAsync(
        ["history", "--session-id", `codex:${CODEX_SESSION}`, "--limit", "2", "--offline"],
        extra,
      );
      expect(p1.status).toBe(0);
      const page1 = p1.json?.page as Record<string, unknown>;
      expect(page1.returned).toBe(2);
      expect(page1.has_more).toBe(true);
      const p2 = await runCliAsync(
        [
          "history",
          "--session-id",
          `codex:${CODEX_SESSION}`,
          "--limit",
          "3",
          "--cursor",
          String(page1.next_cursor),
          "--offline",
        ],
        extra,
      );
      const all = [
        ...((p1.json?.events ?? []) as Record<string, unknown>[]),
        ...((p2.json?.events ?? []) as Record<string, unknown>[]),
      ];
      expect(all.map((e) => e.event_id)).toEqual(events.map((e) => e.event_id));

      const miss = runCli(["history", "--session-id", "codex:nope", "--offline"], extra);
      expect(miss.status).toBe(1);
      expect(miss.error?.kind).toBe("session_not_found");
      const usage = runCli(["history", "--offline"], extra);
      expect(usage.status).toBe(2);
    } finally {
      await herdr.close();
    }
  });

  t("history on a missing events file is explicit, cursor overflow fails", async () => {
    const { stateDir, codexHome, extra } = setupEnv();
    writeCodexHome(codexHome);
    const herdr = await fakeHerdr({
      panes: [{ pane_id: "w1:p1", cwd: CWD }],
      agents: [{ pane_id: "w1:p1", agent: "codex", agent_session: { value: CODEX_SESSION } }],
    });
    try {
      await runCliAsync(["index", "--socket", herdr.socketPath], extra);
      rmSync(path.join(stateDir, "events", `codex_${CODEX_SESSION}.jsonl`), { force: true });
      const h = await runCliAsync(
        ["history", "--session-id", `codex:${CODEX_SESSION}`, "--offline"],
        extra,
      );
      expect(h.status).toBe(0);
      expect(((h.json?.events ?? []) as unknown[]).length).toBe(0);
      expect(((h.json?.page ?? {}) as Record<string, unknown>).events_present).toBe(false);
      expect(((h.json?.session ?? {}) as Record<string, unknown>).events_present).toBe(false);

      const re = await runCliAsync(["index", "--socket", herdr.socketPath], extra);
      expect(re.status).toBe(0);
      const h2 = await runCliAsync(
        ["history", "--session-id", `codex:${CODEX_SESSION}`, "--offline"],
        extra,
      );
      expect(((h2.json?.events ?? []) as unknown[]).length).toBe(5);

      const eventsPath = path.join(stateDir, "events", `codex_${CODEX_SESSION}.jsonl`);
      const size = statSync(eventsPath).size;
      const bad = runCli(
        [
          "history",
          "--session-id",
          `codex:${CODEX_SESSION}`,
          "--cursor",
          String(size + 1000),
          "--offline",
        ],
        extra,
      );
      expect(bad.status).toBe(1);
      expect(bad.error?.kind).toBe("cursor_invalid");
    } finally {
      await herdr.close();
    }
  });
});

// --- opencode -----------------------------------------------------------------------

describe("conversations native opencode", () => {
  t("index + history for an opencode pane; rescan rewrites without duplicates", async () => {
    const { opencodeDir, extra } = setupEnv();
    writeOpenCodeDb(opencodeDir);
    const herdr = await fakeHerdr({
      panes: [{ pane_id: "w1:p9", cwd: CWD }],
      agents: [
        {
          pane_id: "w1:p9",
          agent: "opencode",
          agent_session: { value: OPENCODE_SESSION },
        },
      ],
    });
    try {
      const first = await runCliAsync(["index", "--socket", herdr.socketPath], extra);
      expect(first.status).toBe(0);
      const indexed = first.json?.indexed as Record<string, unknown>[];
      expect(indexed.length).toBe(1);
      expect(indexed[0]?.provider).toBe("opencode");
      expect(indexed[0]?.reindexed).toBe(true);
      expect(indexed[0]?.malformed_lines).toBe(1);
      const total = indexed[0]?.total_events as number;
      expect(total).toBe(8);

      const second = await runCliAsync(["refresh", "--socket", herdr.socketPath], extra);
      expect(second.status).toBe(0);
      const secondIndexed = second.json?.indexed as Record<string, unknown>[];
      expect(secondIndexed[0]?.total_events).toBe(total);
      expect(secondIndexed[0]?.events_appended).toBe(total);

      const h = await runCliAsync(
        ["history", "--session-id", `opencode:${OPENCODE_SESSION}`, "--offline"],
        extra,
      );
      expect(h.status).toBe(0);
      const events = h.json?.events as Record<string, unknown>[];
      expect(events.length).toBe(total);
      const ids = events.map((e) => e.event_id);
      expect(new Set(ids).size).toBe(ids.length);
      expect(events[2]?.model).toBe("openai/gpt-5.4-mini");
      expect(JSON.stringify(events)).not.toContain("thinking…");
    } finally {
      await herdr.close();
    }
  });

  t("opencode session_not_found / unsupported_layout are explicit skips", async () => {
    const { opencodeDir, extra } = setupEnv();
    writeOpenCodeDb(opencodeDir);
    const herdr = await fakeHerdr({
      panes: [
        { pane_id: "w1:p1", cwd: CWD },
        { pane_id: "w1:p2", cwd: CWD },
      ],
      agents: [
        { pane_id: "w1:p1", agent: "opencode", agent_session: { value: "ses_absent" } },
        { pane_id: "w1:p2", agent: "opencode", agent_session: { value: OPENCODE_SESSION } },
      ],
    });
    const legacyDir = tempDir();
    mkdirSync(path.join(legacyDir, "storage", "message"), { recursive: true });
    try {
      const r = await runCliAsync(["index", "--socket", herdr.socketPath], extra);
      expect(r.status).toBe(0);
      const skipped = r.json?.skipped as Record<string, unknown>[];
      const miss = skipped.find((s) => s.pane_id === "w1:p1");
      expect(miss?.reason).toBe("session_not_found");
      expect(miss?.transcript_path).toContain("opencode.db");
      expect(((r.json?.indexed ?? []) as unknown[]).length).toBe(1);

      const r2 = await runCliAsync(["index", "--socket", herdr.socketPath], {
        ...extra,
        OPENCODE_DATA_DIR: legacyDir,
      });
      const skipped2 = r2.json?.skipped as Record<string, unknown>[];
      expect(skipped2.every((s) => s.reason === "unsupported_layout")).toBe(true);
      expect(skipped2[0]?.data_dir).toBe(legacyDir);
    } finally {
      await herdr.close();
    }
  });

  t("v1 message+part tables normalize", async () => {
    const { opencodeDir, extra } = setupEnv();
    writeOpenCodeDbV1(opencodeDir);
    const herdr = await fakeHerdr({
      panes: [{ pane_id: "w1:p1", cwd: CWD }],
      agents: [{ pane_id: "w1:p1", agent: "opencode", agent_session: { value: OPENCODE_SESSION } }],
    });
    try {
      const idx = await runCliAsync(["index", "--socket", herdr.socketPath], extra);
      expect(idx.status).toBe(0);
      const indexed = idx.json?.indexed as Record<string, unknown>[];
      expect(indexed.length).toBe(1);

      const h = await runCliAsync(
        ["history", "--session-id", `opencode:${OPENCODE_SESSION}`, "--offline"],
        extra,
      );
      const events = h.json?.events as Record<string, unknown>[];
      expect(events.map((e) => e.type)).toEqual([
        "turn",
        "user_message",
        "assistant_message",
        "tool_call",
        "tool_result",
      ]);
      expect(events[1]?.text).toBe("v1 prompt");
      expect(events[2]?.text).toBe("v1 reply");
      expect(events[2]?.model).toBe("anthropic/claude-sonnet-4-5");
      expect(events[3]?.tool_use_id).toBe("call-v1");
      expect(events[4]?.text).toBe("ok");
    } finally {
      await herdr.close();
    }
  });
});

// --- read APIs: search / show / usage ----------------------------------------------

describe("conversations native read APIs", () => {
  t("search matches case-insensitively with session/event refs and filters", async () => {
    const { claudeHome, extra } = setupEnv();
    writeTranscript(claudeHome, CWD, SESSION_ID, needleFixtureLines());
    writeTranscript(claudeHome, "/other", SESSION_ID_2, [userLine("x-1", "unrelated chatter", 1)]);
    const herdr = await herdrFor([
      { pane: "w1:p1", sessionId: SESSION_ID },
      { pane: "w1:p2", sessionId: SESSION_ID_2, cwd: "/other" },
    ]);
    try {
      const idx = await runCliAsync(["index", "--socket", herdr.socketPath], extra);
      expect(idx.status).toBe(0);
      expect(((idx.json?.indexed ?? []) as unknown[]).length).toBe(2);

      const all = await runCliAsync(["search", "NEEDLE", "--offline"], extra);
      expect(all.status).toBe(0);
      const results = all.json?.results as Record<string, unknown>[];
      const texts = results.map((r) => (r.event as Record<string, unknown>).text);
      expect(texts).toContain("please fix the needle bug");
      expect(texts).toContain("working on the needle");
      expect(texts).toContain("done with needle cleanup");
      expect(results.every((r) => r.session_key === `claude:${SESSION_ID}`)).toBe(true);
      expect(results[0]?.matched_fields).toContain("text");

      const typed = await runCliAsync(
        ["search", "needle", "--type", "assistant_message", "--offline"],
        extra,
      );
      const typedResults = typed.json?.results as Record<string, unknown>[];
      expect(typedResults.length).toBe(2);
      expect(typedResults.every((r) => r.type === "assistant_message")).toBe(true);

      const scoped = await runCliAsync(
        ["search", "needle", "--session-id", `claude:${SESSION_ID}`, "--offline"],
        extra,
      );
      expect(((scoped.json?.results ?? []) as unknown[]).length).toBe(3);
      const byPane = await runCliAsync(["search", "needle", "--pane", "w1:p2", "--offline"], extra);
      expect(((byPane.json?.results ?? []) as unknown[]).length).toBe(0);
      expect(byPane.json?.selector_note).toBe("filtered_by_pane");

      const tool = await runCliAsync(["search", "Bash", "--offline"], extra);
      const toolResults = tool.json?.results as Record<string, unknown>[];
      expect(toolResults.some((r) => r.type === "tool_call")).toBe(true);

      const bad = runCli(["search", "x", "--type", "bogus", "--offline"], extra);
      expect(bad.status).toBe(2);
      expect(runCli(["search", "--offline"], extra).status).toBe(2);
    } finally {
      await herdr.close();
    }
  });

  t("search + history cursors survive an index update without dup/miss", async () => {
    const { claudeHome, extra } = setupEnv();
    const file = writeTranscript(claudeHome, CWD, SESSION_ID, [
      userLine("u-1", "needle one", 1),
      userLine("u-2", "needle two", 2),
      userLine("u-3", "needle three", 3),
      userLine("u-4", "needle four", 4),
    ]);
    const herdr = await herdrFor([{ pane: "w1:p1", sessionId: SESSION_ID }]);
    try {
      await runCliAsync(["index", "--socket", herdr.socketPath], extra);

      const h1 = await runCliAsync(
        ["history", "--session-id", `claude:${SESSION_ID}`, "--limit", "3", "--offline"],
        extra,
      );
      const page1 = h1.json?.page as Record<string, unknown>;
      expect(page1.has_more).toBe(true);
      appendFileSync(file, JSON.stringify(userLine("u-5", "needle five", 5)) + "\n");
      const idx2 = await runCliAsync(["refresh", "--socket", herdr.socketPath], extra);
      expect(idx2.status).toBe(0);
      const h2 = await runCliAsync(
        [
          "history",
          "--session-id",
          `claude:${SESSION_ID}`,
          "--limit",
          "10",
          "--cursor",
          String(page1.next_cursor),
          "--offline",
        ],
        extra,
      );
      const allEvents = [
        ...((h1.json?.events ?? []) as Record<string, unknown>[]),
        ...((h2.json?.events ?? []) as Record<string, unknown>[]),
      ];
      const ids = allEvents.map((e) => e.event_id);
      expect(new Set(ids).size).toBe(ids.length);
      expect(allEvents.length).toBe(10);

      const s1 = await runCliAsync(["search", "needle", "--limit", "2", "--offline"], extra);
      const sp1 = s1.json?.page as Record<string, unknown>;
      expect(sp1.has_more).toBe(true);
      appendFileSync(file, JSON.stringify(userLine("u-6", "needle six", 6)) + "\n");
      await runCliAsync(["index", "--socket", herdr.socketPath], extra);
      const s2 = await runCliAsync(
        ["search", "needle", "--limit", "10", "--cursor", String(sp1.next_cursor), "--offline"],
        extra,
      );
      const allMatches = [
        ...((s1.json?.results ?? []) as Record<string, unknown>[]),
        ...((s2.json?.results ?? []) as Record<string, unknown>[]),
      ];
      const matchIds = allMatches.map((r) => r.event_id);
      expect(new Set(matchIds).size).toBe(matchIds.length);
      expect(matchIds.length).toBe(6);
      expect(allMatches.every((r) => r.session_key === `claude:${SESSION_ID}`)).toBe(true);

      const badCursor = runCli(["search", "needle", "--cursor", "!!!", "--offline"], extra);
      expect(badCursor.status).toBe(2);
    } finally {
      await herdr.close();
    }
  });

  t("show resolves event_id, tool:<id>, and turn:<n>", async () => {
    const { claudeHome, extra } = setupEnv();
    writeTranscript(claudeHome, CWD, SESSION_ID, needleFixtureLines());
    const herdr = await herdrFor([{ pane: "w1:p1", sessionId: SESSION_ID }]);
    try {
      await runCliAsync(["index", "--socket", herdr.socketPath], extra);

      const byEvent = await runCliAsync(["show", "u-1:message", "--offline"], extra);
      expect(byEvent.status).toBe(0);
      expect(byEvent.json?.resolved).toEqual({ kind: "event_id" });
      expect(byEvent.json?.matched).toBe(1);
      const ev = ((byEvent.json?.results ?? []) as Record<string, unknown>[])[0]?.event as Record<
        string,
        unknown
      >;
      expect(ev.text).toBe("please fix the needle bug");

      const byTool = await runCliAsync(["show", "tool:tool-a-1", "--offline"], extra);
      expect(byTool.status).toBe(0);
      expect(byTool.json?.resolved).toEqual({ kind: "tool_use_id" });
      const toolResults = byTool.json?.results as Record<string, unknown>[];
      expect(toolResults.length).toBe(2);
      expect(toolResults.map((r) => (r.event as Record<string, unknown>).type).sort()).toEqual([
        "tool_call",
        "tool_result",
      ]);

      const byTurn = await runCliAsync(["show", "turn:2", "--offline"], extra);
      expect(byTurn.status).toBe(0);
      const turnResults = byTurn.json?.results as Record<string, unknown>[];
      expect(turnResults.length).toBeGreaterThan(0);
      expect(turnResults.every((r) => (r.event as Record<string, unknown>).turn_index === 2)).toBe(
        true,
      );

      const miss = await runCliAsync(["show", "no-such-id", "--offline"], extra);
      expect(miss.status).toBe(0);
      expect(miss.json?.matched).toBe(0);
      expect(((miss.json?.results ?? []) as unknown[]).length).toBe(0);
    } finally {
      await herdr.close();
    }
  });

  t("usage pages only events carrying provider usage", async () => {
    const { claudeHome, extra } = setupEnv();
    writeTranscript(claudeHome, CWD, SESSION_ID, needleFixtureLines());
    const herdr = await herdrFor([{ pane: "w1:p1", sessionId: SESSION_ID }]);
    try {
      await runCliAsync(["index", "--socket", herdr.socketPath], extra);
      const r = await runCliAsync(["usage", "--offline"], extra);
      expect(r.status).toBe(0);
      const results = r.json?.results as Record<string, unknown>[];
      expect(results.length).toBe(2);
      expect(results.every((r2) => r2.type === "assistant_message")).toBe(true);
      expect(
        results.every(
          (r2) =>
            typeof (r2.event as Record<string, unknown>).usage === "object" &&
            (r2.event as Record<string, unknown>).usage !== null,
        ),
      ).toBe(true);
      const scoped = await runCliAsync(
        ["usage", "--session-id", `claude:${SESSION_ID}`, "--offline"],
        extra,
      );
      expect(((scoped.json?.results ?? []) as unknown[]).length).toBe(2);
    } finally {
      await herdr.close();
    }
  });

  t("stale/missing are explicit: events file, transcript, and empty index", async () => {
    const { stateDir, claudeHome, extra } = setupEnv();
    const transcript = writeTranscript(claudeHome, CWD, SESSION_ID, needleFixtureLines());
    const herdr = await herdrFor([{ pane: "w1:p1", sessionId: SESSION_ID }]);
    try {
      await runCliAsync(["index", "--socket", herdr.socketPath], extra);

      rmSync(transcript, { force: true });
      const s = await runCliAsync(["search", "needle", "--offline"], extra);
      const sess = ((s.json?.sessions ?? []) as Record<string, unknown>[])[0];
      expect(sess?.transcript_present).toBe(false);
      expect(((s.json?.results ?? []) as unknown[]).length).toBe(3);

      rmSync(path.join(stateDir, "events", `claude_${SESSION_ID}.jsonl`), { force: true });
      const s2 = await runCliAsync(["search", "needle", "--offline"], extra);
      const sess2 = ((s2.json?.sessions ?? []) as Record<string, unknown>[])[0];
      expect(sess2?.events_present).toBe(false);
      expect(((s2.json?.results ?? []) as unknown[]).length).toBe(0);
      const sh = await runCliAsync(["show", "u-1", "--offline"], extra);
      expect(((sh.json?.results ?? []) as unknown[]).length).toBe(0);
      expect(((sh.json?.sessions ?? []) as Record<string, unknown>[])[0]?.events_present).toBe(
        false,
      );

      const list = await runCliAsync(["sessions", "--offline"], extra);
      const entry = ((list.json?.sessions ?? []) as Record<string, unknown>[])[0];
      expect(entry?.transcript_present).toBe(false);
      expect(entry?.events_present).toBeUndefined();
    } finally {
      await herdr.close();
    }
  });

  t("empty index and unknown selectors are explicit not_indexed", async () => {
    const { extra } = setupEnv();
    const empty = await runCliAsync(["search", "anything", "--offline"], extra);
    expect(empty.status).toBe(0);
    expect(((empty.json?.results ?? []) as unknown[]).length).toBe(0);
    expect(empty.json?.hint).toContain("index");

    const miss = runCli(["search", "x", "--session-id", "claude:nope", "--offline"], extra);
    expect(miss.status).toBe(1);
    expect(miss.error?.kind).toBe("session_not_found");
    const missShow = runCli(["show", "abc", "--pane", "w9:p9", "--offline"], extra);
    expect(missShow.status).toBe(1);
    expect(missShow.error?.kind).toBe("session_not_found");
  });
});

// --- #306 paging-cursor regression -------------------------------------------------

describe("conversations native #306 cursor fix", () => {
  t(
    "usage export terminates without duplicates when sessions finish at different pages",
    async () => {
      const { claudeHome, extra } = setupEnv();
      // Two sessions: small one finishes on page 1, large one spans pages.
      // Before the fix, the finished session dropped out of next_cursor and
      // was rescanned from byte 0 on every later page → duplicate results.
      const small: unknown[] = [];
      for (let i = 0; i < 3; i += 1) {
        small.push(userLine(`s-u-${i}`, `small prompt ${i}`, i));
        small.push(assistantLine(`s-a-${i}`, `small answer ${i}`, i + 1));
      }
      const large: unknown[] = [];
      for (let i = 0; i < 12; i += 1) {
        large.push(userLine(`l-u-${i}`, `large prompt ${i}`, i));
        large.push(assistantLine(`l-a-${i}`, `large answer ${i}`, i + 1));
      }
      writeTranscript(claudeHome, CWD, SESSION_ID, small);
      writeTranscript(claudeHome, "/other", SESSION_ID_2, large);
      const herdr = await herdrFor([
        { pane: "w1:p1", sessionId: SESSION_ID },
        { pane: "w1:p2", sessionId: SESSION_ID_2, cwd: "/other" },
      ]);
      try {
        const idx = await runCliAsync(["index", "--socket", herdr.socketPath], extra);
        expect(idx.status).toBe(0);

        // Page through `usage` with a tiny limit; every assistant_message
        // carries usage so all 15 events match.
        const seen = new Set<string>();
        let cursor: string | null = null;
        let pages = 0;
        for (;;) {
          const args = ["usage", "--limit", "4", "--offline"];
          if (cursor !== null) args.push("--cursor", cursor);
          const r = await runCliAsync(args, extra);
          expect(r.status).toBe(0);
          const results = (r.json?.results ?? []) as Record<string, unknown>[];
          for (const m of results) {
            const id = String(m.event_id);
            expect(seen.has(id)).toBe(false); // no cross-page duplicates
            seen.add(id);
          }
          const page = r.json?.page as Record<string, unknown>;
          pages += 1;
          expect(pages).toBeLessThan(50); // terminates
          if (page.has_more !== true) break;
          cursor = String(page.next_cursor);
          expect(cursor).not.toBe("null");
        }
        expect(seen.size).toBe(15); // 3 + 12 assistant events, each once
      } finally {
        await herdr.close();
      }
    },
  );
});

// --- TUI entrypoint -----------------------------------------------------------------

describe("conversations native tui", () => {
  t("prints CLI fallback notice and exits 0 without a TTY", () => {
    const result = spawnSync(CONV_BIN, ["tui", "main"], {
      encoding: "utf8",
      env: testEnv(),
      timeout: 15_000,
    });
    expect(result.status).toBe(0);
    expect(result.stdout).toContain("requires an interactive terminal");
    expect(result.stdout).toContain("conversations <capability");
  });

  t("tui --help falls through to the non-TTY notice like the TS build", () => {
    // TS parity: `tui.ts` only special-cases `--open`; any other args
    // (including --help) reach the TTY check and print the notice.
    const result = spawnSync(CONV_BIN, ["tui", "--help"], {
      encoding: "utf8",
      env: testEnv(),
      timeout: 15_000,
    });
    expect(result.status).toBe(0);
    expect(result.stdout).toContain("requires an interactive terminal");
    expect(result.stdout).not.toContain("Usage:");
  });
});
