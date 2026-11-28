/**
 * TS↔Rust output-parity diff harness for rebuildup.conversations
 * (Issue #293).
 *
 * The TypeScript implementation is retired by the same change that
 * ships `plugins/conversations/bin/conversations`, so this harness
 * reconstructs it on the fly: `git archive` extracts
 * `plugins/conversations` + `packages` at the pinned pre-removal
 * commit (`--ts-commit`, default: the commit recorded in TS_REMOVAL)
 * into a temp dir, then runs BOTH `bun <tmp>/plugins/conversations/
 * src/cli.ts` (or `src/tui.ts` for the TUI fallback step) and the
 * shipped Rust binary against the same fake herdr socket, the same
 * transcript fixtures, and identical fresh plugin state/config dirs.
 *
 * Every step's normalized stdout + exit code must match, and the
 * resulting `state.json` + `events/*.jsonl` must match after
 * normalization — evidencing wire-contract AND on-disk schema parity.
 *
 * Usage: bun scripts/diff-conversations.ts [--ts-commit SHA] [--keep]
 */

import { Database } from "bun:sqlite";
import { spawnSync } from "node:child_process";
import {
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  realpathSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { conversationsBinary } from "../tests/helpers/conversations-bin";
import { herdrSocketEndpoint, testEnv } from "../tests/helpers/test-env";

const REPO_ROOT = path.join(import.meta.dir, "..");

function git(...argv: string[]): string {
  const r = spawnSync("git", argv, { cwd: REPO_ROOT, encoding: "utf8" });
  if (r.status !== 0) {
    throw new Error(`git ${argv.join(" ")} failed: ${r.stderr}`);
  }
  return r.stdout.trim();
}

// --- args -----------------------------------------------------------------

const args = process.argv.slice(2);
function argValue(flag: string): string | undefined {
  const i = args.indexOf(flag);
  return i === -1 ? undefined : args[i + 1];
}
const keepTmp = args.includes("--keep");

// The last commit that still ships plugins/conversations/src/*.ts —
// parent of the TS-removal commit (4d6e08c~1 = 8ddb7db). Override with
// --ts-commit.
const tsCommit = argValue("--ts-commit") ?? git("rev-parse", "4d6e08c~1");

// --- extract TS snapshot ----------------------------------------------------

const work = mkdtempSync(path.join(tmpdir(), "conversations-diff-"));
const tsRoot = path.join(work, "ts");
mkdirSync(tsRoot, { recursive: true });
{
  const archive = spawnSync("git", ["archive", tsCommit, "plugins/conversations", "packages"], {
    cwd: REPO_ROOT,
    encoding: null,
    maxBuffer: 64 * 1024 * 1024,
  });
  if (archive.status !== 0) {
    throw new Error(`git archive ${tsCommit} failed: ${archive.stderr}`);
  }
  const tar = spawnSync("tar", ["-x", "-C", tsRoot], {
    input: archive.stdout as Buffer,
    encoding: null,
  });
  if (tar.status !== 0) throw new Error(`tar extract failed: ${tar.stderr}`);
}
const tsCli = path.join(tsRoot, "plugins/conversations/src/cli.ts");
const tsTui = path.join(tsRoot, "plugins/conversations/src/tui.ts");
const rustBin = conversationsBinary();

// --- fixtures (shared read-only inputs for BOTH implementations) ------------

const SESSION_ID = "3bc481ff-60ac-491a-abd0-07c408c85e71";
const CODEX_SESSION = "01a1f843-77fa-7d30-b3c1-19f822a13215";
const OPENCODE_SESSION = "ses_test123abc";
const CWD = "/repo/proj";

function encodeClaudeCwd(cwd: string): string {
  return cwd.replace(/[^A-Za-z0-9]/g, "-");
}

const fixtureRoot = mkdtempSync(path.join(tmpdir(), "conversations-diff-fixtures-"));
const claudeHome = path.join(fixtureRoot, "claude");
const codexHome = path.join(fixtureRoot, "codex");
const opencodeDir = path.join(fixtureRoot, "opencode");
mkdirSync(opencodeDir, { recursive: true });

function writeJsonl(file: string, lines: unknown[]): void {
  mkdirSync(path.dirname(file), { recursive: true });
  writeFileSync(
    file,
    `${lines.map((l) => (typeof l === "string" ? l : JSON.stringify(l))).join("\n")}\n`,
  );
}

// claude transcript: two turns, tool call/result, usage, one malformed line.
const claudeLines: unknown[] = [
  { type: "mode", mode: "normal", sessionId: SESSION_ID },
  {
    type: "user",
    uuid: "u-1",
    timestamp: "2026-01-01T00:00:01.000Z",
    sessionId: SESSION_ID,
    cwd: CWD,
    message: { role: "user", content: "please fix the needle bug" },
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
        { type: "text", text: "working on the needle" },
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
  {
    type: "user",
    uuid: "u-3",
    timestamp: "2026-01-01T00:00:05.000Z",
    message: { role: "user", content: "second prompt about haystack" },
  },
  {
    type: "assistant",
    uuid: "a-2",
    timestamp: "2026-01-01T00:00:06.000Z",
    message: {
      id: "msg-2",
      role: "assistant",
      model: "claude-test-1",
      content: [{ type: "text", text: "done with needle cleanup" }],
      usage: { input_tokens: 7, output_tokens: 3 },
    },
  },
];
writeJsonl(
  path.join(claudeHome, "projects", encodeClaudeCwd(CWD), `${SESSION_ID}.jsonl`),
  claudeLines,
);

// codex rollout fixture.
const codexTs = (s: number) => `2026-01-01T00:00:${String(s).padStart(2, "0")}.000Z`;
const codexLines: unknown[] = [
  {
    timestamp: codexTs(0),
    ordinal: 0,
    type: "session_meta",
    payload: { id: CODEX_SESSION, cwd: CWD, cli_version: "0.169.0" },
  },
  {
    timestamp: codexTs(1),
    ordinal: 1,
    type: "turn_context",
    payload: { turn_id: "t1", model: "gpt-6-test", cwd: CWD },
  },
  {
    timestamp: codexTs(2),
    ordinal: 2,
    type: "event_msg",
    payload: { type: "task_started", turn_id: "t1" },
  },
  {
    timestamp: codexTs(4),
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
    timestamp: codexTs(5),
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
    timestamp: codexTs(6),
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
    timestamp: codexTs(8),
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
    timestamp: codexTs(9),
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
    timestamp: codexTs(10),
    ordinal: 10,
    type: "event_msg",
    payload: { type: "task_complete", turn_id: "t1" },
  },
  "{not valid json",
];
writeJsonl(
  path.join(
    codexHome,
    "sessions",
    "2026",
    "01",
    "01",
    `rollout-2026-01-01T00-00-00-${CODEX_SESSION}.jsonl`,
  ),
  codexLines,
);

// opencode v2 SQLite fixture.
const OC_T0 = 1767225600000;
{
  const dbPath = path.join(opencodeDir, "opencode.db");
  const db = new Database(dbPath);
  db.run("CREATE TABLE session_v2 (id TEXT PRIMARY KEY, directory TEXT, time_created INTEGER)");
  db.run(
    "CREATE TABLE session_message (id TEXT PRIMARY KEY, session_id TEXT, type TEXT, seq INTEGER, time_created INTEGER, time_updated INTEGER, data TEXT)",
  );
  db.query("INSERT INTO session_v2 (id, directory, time_created) VALUES (?, ?, ?)").run(
    OPENCODE_SESSION,
    CWD,
    OC_T0,
  );
  const insertMsg = db.query(
    "INSERT INTO session_message (id, session_id, type, seq, time_created, time_updated, data) VALUES (?, ?, ?, ?, ?, ?, ?)",
  );
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
    JSON.stringify({
      time: { created: OC_T0 + 1, completed: OC_T0 + 2 },
      agent: "build",
      model: { id: "gpt-5.4-mini", providerID: "openai" },
      content: [
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
      ],
      finish: "stop",
      cost: 0.01,
      tokens: { input: 100, output: 20, reasoning: 5, cache: { read: 50, write: 0 } },
    }),
  );
  // Edge cases pinned by review: `output: null` must fall back to
  // `content` (TS `??`), empty `summary`/`modelID` emit nothing, and
  // valid-JSON non-object `data` is skipped without counting malformed.
  insertMsg.run(
    "msg_c1",
    OPENCODE_SESSION,
    "compaction",
    3,
    OC_T0 + 4,
    OC_T0 + 4,
    JSON.stringify({ time: { created: OC_T0 + 4 }, summary: "" }),
  );
  insertMsg.run(
    "msg_a2",
    OPENCODE_SESSION,
    "assistant",
    4,
    OC_T0 + 5,
    OC_T0 + 5,
    JSON.stringify({
      time: { created: OC_T0 + 5 },
      modelID: "",
      content: [{ type: "text", text: "edge case" }],
    }),
  );
  insertMsg.run(
    "msg_a3",
    OPENCODE_SESSION,
    "assistant",
    5,
    OC_T0 + 6,
    OC_T0 + 6,
    JSON.stringify({
      time: { created: OC_T0 + 6 },
      content: [
        {
          type: "tool",
          callID: "call-null-out",
          name: "bash",
          state: {
            status: "completed",
            input: { command: "pwd" },
            output: null,
            content: [{ type: "text", text: "via-content-fallback" }],
          },
        },
      ],
    }),
  );
  // Valid JSON but not an object — skipped silently (no malformed++).
  insertMsg.run("msg_x1", OPENCODE_SESSION, "assistant", 6, OC_T0 + 7, OC_T0 + 7, "123");
  // Invalid JSON — counted malformed on both sides.
  insertMsg.run("msg_x2", OPENCODE_SESSION, "assistant", 7, OC_T0 + 8, OC_T0 + 8, "{broken json");
  db.close();
}

// --- fake herdr socket ---------------------------------------------------------

const PANES = [
  { pane_id: "w1:p1", workspace_id: "w1", tab_id: "w1:t1", cwd: CWD },
  { pane_id: "w1:p2", workspace_id: "w1", tab_id: "w1:t1", cwd: CWD },
  { pane_id: "w1:p3", workspace_id: "w1", tab_id: "w1:t1", cwd: CWD },
];
const AGENTS = [
  {
    pane_id: "w1:p1",
    agent: "claude",
    agent_status: "idle",
    state_change_seq: 7,
    cwd: CWD,
    agent_session: { agent: "claude", kind: "session", value: SESSION_ID },
  },
  {
    pane_id: "w1:p2",
    agent: "codex",
    agent_status: "idle",
    state_change_seq: 3,
    cwd: CWD,
    agent_session: { agent: "codex", kind: "session", value: CODEX_SESSION },
  },
  {
    pane_id: "w1:p3",
    agent: "opencode",
    agent_status: "idle",
    state_change_seq: 1,
    cwd: CWD,
    agent_session: { agent: "opencode", kind: "session", value: OPENCODE_SESSION },
  },
];

const sockDir = mkdtempSync(path.join(tmpdir(), "conversations-diff-sock-"));
const socketPath = path.join(sockDir, "herdr.sock");
const server = net.createServer((conn) => {
  let buffer = "";
  conn.on("data", (chunk: Buffer) => {
    buffer += chunk.toString("utf8");
    let nl = buffer.indexOf("\n");
    while (nl !== -1) {
      const line = buffer.slice(0, nl);
      buffer = buffer.slice(nl + 1);
      nl = buffer.indexOf("\n");
      if (!line.trim()) continue;
      const msg = JSON.parse(line) as { id?: string; method?: string };
      const result =
        msg.method === "session.snapshot"
          ? {
              snapshot: {
                session: { name: "test-session" },
                panes: PANES,
                agents: AGENTS,
              },
            }
          : {};
      conn.write(`${JSON.stringify({ id: msg.id, result })}\n`);
    }
  });
});
await new Promise<void>((res, rej) => {
  server.once("error", rej);
  server.listen(herdrSocketEndpoint(socketPath), res);
});

// --- runners ---------------------------------------------------------------

interface RunResult {
  status: number | null;
  stdout: string;
  stderr: string;
}

interface Step {
  /** argv for the Rust binary */
  rs: string[];
  /** argv for `bun <tsScript> ...` — defaults to the same args via cli.ts */
  ts?: string[];
  /** TS entry script (default cli.ts; tui fallback uses tui.ts) */
  tsScript?: "cli" | "tui";
  /** top-level JSON keys stripped before compare (documented divergences) */
  strip?: string[];
  /** stdout post-map applied to BOTH sides before compare (documented divergences) */
  mapStdout?: (s: string) => string;
}

function runTs(step: Step, env: Record<string, string>): Promise<RunResult> {
  const script = step.tsScript === "tui" ? tsTui : tsCli;
  const argv = step.ts ?? step.rs;
  return runProc(["bun", script, ...argv], env);
}
function runRust(step: Step, env: Record<string, string>): Promise<RunResult> {
  return runProc([rustBin, ...step.rs], env);
}
async function runProc(cmd: string[], env: Record<string, string>): Promise<RunResult> {
  const proc = Bun.spawn(cmd, { env, stdout: "pipe", stderr: "pipe" });
  const [stdout, stderr, status] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  return { status, stdout, stderr };
}

interface Dirs {
  stateDir: string;
  configDir: string;
}
function freshDirs(tag: string): Dirs {
  const dir = mkdtempSync(path.join(tmpdir(), `conversations-diff-${tag}-`));
  return { stateDir: path.join(dir, "state"), configDir: path.join(dir, "config") };
}

/** Shared fixture env (read-only transcript roots) + per-impl state dirs. */
function cliEnv(dirs: Dirs): Record<string, string> {
  return testEnv({
    HERDR_PLUGIN_STATE_DIR: dirs.stateDir,
    HERDR_PLUGIN_CONFIG_DIR: dirs.configDir,
    HERDR_SOCKET_PATH: socketPath,
    CLAUDE_CONFIG_DIR: claudeHome,
    CODEX_HOME: codexHome,
    OPENCODE_DATA_DIR: opencodeDir,
    XDG_DATA_HOME: path.join(fixtureRoot, "xdg-none"),
  });
}

// --- normalization ----------------------------------------------------------

const VOLATILE_KEYS = new Set([
  "generated_at",
  "last_indexed_at",
  "first_event_at",
  "last_event_at",
]);
const ISO_TS = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}/;

// Paths scrubbed from every compared string: the fake socket dir, the
// per-scenario state/config dirs, and the shared fixture root (embedded
// in transcript_path/cwd inside state.json and session entries).
let scrubPaths: string[] = [];
function scrubPathsInit(tsDirs: Dirs, rsDirs: Dirs) {
  const candidates = [
    sockDir,
    path.dirname(socketPath),
    realpathSync(sockDir),
    tsDirs.stateDir,
    tsDirs.configDir,
    rsDirs.stateDir,
    rsDirs.configDir,
    fixtureRoot,
    realpathSync(fixtureRoot),
  ];
  scrubPaths = [...new Set(candidates.filter(Boolean))].sort((a, b) => b.length - a.length);
}

function scrubString(s: string): string {
  let out = s;
  for (const p of scrubPaths) {
    if (p && out.includes(p)) out = out.split(p).join("<tmp>");
  }
  // The retired TS entrypoint is invoked as `bun run src/cli.ts`; the
  // native binary is invoked as `conversations`. Only the non-TTY
  // notice embeds the entrypoint name.
  out = out.split("bun run src/cli.ts").join("conversations");
  return out;
}

function normalize(value: unknown, key?: string, strip?: ReadonlySet<string>): unknown {
  if (typeof value === "string") {
    if (key !== undefined && VOLATILE_KEYS.has(key)) return "<volatile>";
    if (ISO_TS.test(value)) return "<volatile>";
    return scrubString(value);
  }
  if (Array.isArray(value)) {
    return value.map((v) => normalize(v, undefined, strip));
  }
  if (value !== null && typeof value === "object") {
    const src = value as Record<string, unknown>;
    const keys = Object.keys(src).sort();
    const out: Record<string, unknown> = {};
    for (const k of keys) {
      if (strip?.has(k)) continue;
      out[scrubString(k)] = normalize(src[k], k, strip);
    }
    return out;
  }
  return value;
}

function parseJsonOrText(stdout: string): unknown {
  try {
    return JSON.parse(stdout);
  } catch {
    return stdout;
  }
}

// --- scenarios ---------------------------------------------------------------

const SCENARIOS: { name: string; steps: Step[] }[] = [
  {
    name: "usage-errors",
    steps: [
      { rs: ["bogus"] },
      { rs: ["history", "--socket", socketPath] }, // no selector → usage
      { rs: ["search", "--socket", socketPath] }, // no query → usage
      { rs: ["search", "x", "--type", "bogus", "--offline"] },
      { rs: ["history", "--session-id", "claude:nope", "--offline"] },
      { rs: ["show", "abc", "--pane", "w9:p9", "--offline"] },
    ],
  },
  {
    name: "capability",
    steps: [
      {
        rs: ["capability"],
        // `version` is the intentional 0.6.0 -> 0.7.0 bump.
        strip: ["version"],
      },
    ],
  },
  {
    name: "index-all-providers",
    steps: [
      { rs: ["index", "--socket", socketPath] },
      { rs: ["sessions", "--socket", socketPath] },
      { rs: ["sessions", "--offline"] },
      // refresh is a no-op rescan for file providers and a rewrite for opencode
      { rs: ["refresh", "--socket", socketPath] },
    ],
  },
  {
    name: "read-claude",
    steps: [
      { rs: ["index", "--socket", socketPath] },
      { rs: ["history", "--session-id", `claude:${SESSION_ID}`, "--offline"] },
      {
        rs: ["history", "--session-id", `claude:${SESSION_ID}`, "--limit", "4", "--offline"],
      },
      {
        rs: ["search", "needle", "--type", "assistant_message", "--offline"],
      },
      { rs: ["show", "u-1:message", "--offline"] },
      { rs: ["show", "tool:tool-1", "--offline"] },
      { rs: ["show", "turn:2", "--offline"] },
      { rs: ["usage", "--offline"] },
      { rs: ["usage", "--session-id", `claude:${SESSION_ID}`, "--limit", "1", "--offline"] },
    ],
  },
  {
    name: "read-codex-opencode",
    steps: [
      { rs: ["index", "--socket", socketPath] },
      { rs: ["history", "--session-id", `codex:${CODEX_SESSION}`, "--offline"] },
      {
        rs: ["history", "--session-id", `codex:${CODEX_SESSION}`, "--limit", "2", "--offline"],
      },
      { rs: ["history", "--session-id", `opencode:${OPENCODE_SESSION}`, "--offline"] },
      { rs: ["search", "fix the bug", "--provider", "codex", "--offline"] },
      { rs: ["usage", "--provider", "opencode", "--offline"] },
    ],
  },
  {
    name: "tui-fallback",
    steps: [
      // TS ships the TUI as a separate entry (src/tui.ts); Rust folds it
      // into `conversations tui`. Non-TTY both print the fallback notice;
      // the CLI-equivalent hint differs by construction (documented).
      {
        rs: ["tui", "main"],
        ts: ["main"],
        tsScript: "tui",
        mapStdout: (s) => s.replace("bun run src/cli.ts", "conversations"),
      },
      {
        rs: ["tui", "--help"],
        ts: ["--help"],
        tsScript: "tui",
      },
    ],
  },
];

// --- run --------------------------------------------------------------------

let failures = 0;
let checks = 0;

function report(ok: boolean, label: string, detail?: string) {
  checks++;
  if (!ok) {
    failures++;
    console.log(`DIFF  ${label}`);
    if (detail) console.log(detail);
  } else {
    console.log(`ok    ${label}`);
  }
}

function diffJson(a: unknown, b: unknown): string | null {
  const sa = JSON.stringify(a, null, 2);
  const sb = JSON.stringify(b, null, 2);
  if (sa === sb) return null;
  const al = sa.split("\n");
  const bl = sb.split("\n");
  const out: string[] = [];
  const max = Math.max(al.length, bl.length);
  for (let i = 0; i < max && out.length < 40; i++) {
    if (al[i] !== bl[i]) out.push(`  - ${al[i] ?? ""}\n  + ${bl[i] ?? ""}`);
  }
  return out.join("\n");
}

for (const scenario of SCENARIOS) {
  const tsDirs = freshDirs(`${scenario.name}-ts`);
  const rsDirs = freshDirs(`${scenario.name}-rs`);
  scrubPathsInit(tsDirs, rsDirs);
  const tsEnv = cliEnv(tsDirs);
  const rsEnv = cliEnv(rsDirs);

  for (let i = 0; i < scenario.steps.length; i++) {
    const step = scenario.steps[i] as Step;
    const label = `${scenario.name} #${i}: conversations ${step.rs.join(" ")}`;
    const strip = step.strip ? new Set(step.strip) : undefined;
    const [ts, rs] = await Promise.all([runTs(step, tsEnv), runRust(step, rsEnv)]);
    let tsOut = ts.stdout;
    let rsOut = rs.stdout;
    if (step.mapStdout) {
      tsOut = step.mapStdout(tsOut);
      rsOut = step.mapStdout(rsOut);
    }
    const tsNorm = normalize(parseJsonOrText(tsOut), undefined, strip);
    const rsNorm = normalize(parseJsonOrText(rsOut), undefined, strip);
    const sameStatus = ts.status === rs.status;
    const stdoutDiff = diffJson(tsNorm, rsNorm);
    if (sameStatus && stdoutDiff === null) {
      report(true, label);
    } else {
      report(
        false,
        label,
        `  status ts=${ts.status} rs=${rs.status}\n${stdoutDiff ?? ""}\n  ts.stderr: ${ts.stderr.slice(0, 300)}\n  rs.stderr: ${rs.stderr.slice(0, 300)}`,
      );
    }
  }

  // Final state parity: state.json AND the events/*.jsonl logs.
  const readState = (p: string): unknown => {
    try {
      return JSON.parse(readFileSync(p, "utf8"));
    } catch {
      return null;
    }
  };
  const tsState = readState(path.join(tsDirs.stateDir, "state.json"));
  const rsState = readState(path.join(rsDirs.stateDir, "state.json"));
  const stateDiff = diffJson(normalize(tsState), normalize(rsState));
  report(stateDiff === null, `${scenario.name}: final state.json parity`, stateDiff ?? undefined);

  const readEventsDir = (stateDir: string): Record<string, unknown> => {
    const dir = path.join(stateDir, "events");
    const out: Record<string, unknown> = {};
    let files: string[] = [];
    try {
      files = readdirSync(dir);
    } catch {
      return out;
    }
    for (const f of files.sort()) {
      out[f] = readFileSync(path.join(dir, f), "utf8")
        .split("\n")
        .filter((l) => l.trim() !== "")
        .map((l) => normalize(JSON.parse(l)));
    }
    return out;
  };
  const tsEvents = readEventsDir(tsDirs.stateDir);
  const rsEvents = readEventsDir(rsDirs.stateDir);
  const eventsDiff = diffJson(tsEvents, rsEvents);
  report(eventsDiff === null, `${scenario.name}: events/*.jsonl parity`, eventsDiff ?? undefined);
}

// --- teardown ---------------------------------------------------------------

server.close();
rmSync(sockDir, { recursive: true, force: true });
rmSync(fixtureRoot, { recursive: true, force: true });
if (keepTmp) {
  console.log(`kept TS snapshot at ${tsRoot}`);
} else {
  rmSync(work, { recursive: true, force: true });
}

console.log(
  `\n${checks - failures}/${checks} checks identical (ts=${tsCommit.slice(0, 8)} rust=${rustBin})`,
);
process.exit(failures === 0 ? 0 : 1);
