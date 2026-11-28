/**
 * TS↔Rust output-parity diff harness for rebuildup.usage (Issue #294).
 *
 * The TypeScript implementation is retired by the same change that
 * ships `plugins/usage/bin/usage`, so this harness reconstructs it on
 * the fly: `git archive` extracts `plugins/usage` + `plugins/conversations`
 * + `packages` at the pinned pre-removal commit (`--ts-commit`,
 * default: the commit recorded in TS_REMOVAL) into a temp dir, then
 * runs BOTH `bun <tmp>/plugins/usage/src/cli.ts` (or `src/tui.ts` for
 * the TUI fallback step) and the shipped Rust binary against:
 *
 *   - one shared conversations index (built once by the native
 *     conversations binary; both usage implementations read it through
 *     the public `usage` contract — TS usage resolves the TS
 *     conversations snapshot, Rust usage resolves the shipped binary),
 *   - identical mock quota/pricing/hub HTTP servers,
 *   - identical fake credential fixtures,
 *   - identical fresh plugin state/config dirs per implementation.
 *
 * Every step's normalized stdout + exit code must match. Volatile
 * fields (timestamps, cache ages, fetched/checked-at readings) are
 * normalized on both sides; credential material is asserted absent.
 *
 * Usage: bun scripts/diff-usage.ts [--ts-commit SHA] [--keep]
 */

import { spawnSync } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  readdirSync,
  readFileSync,
  realpathSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import http from "node:http";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { conversationsBinary } from "../tests/helpers/conversations-bin";
import { herdrSocketEndpoint, testEnv } from "../tests/helpers/test-env";
import { usageBinary } from "../tests/helpers/usage-bin";

const REPO_ROOT = path.join(import.meta.dir, "..");

// --- args -----------------------------------------------------------------

const args = process.argv.slice(2);
function argValue(flag: string): string | undefined {
  const i = args.indexOf(flag);
  return i === -1 ? undefined : args[i + 1];
}
const keepTmp = args.includes("--keep");

// The last commit that still ships plugins/usage/src/*.ts — parent of
// the #294 TS-removal commit (6614811). Override with --ts-commit.
const tsCommit = argValue("--ts-commit") ?? "6614811";

// --- extract TS snapshot ----------------------------------------------------

const work = mkdtempSync(path.join(tmpdir(), "usage-diff-"));
const tsRoot = path.join(work, "ts");
mkdirSync(tsRoot, { recursive: true });
{
  const archive = spawnSync(
    "git",
    ["archive", tsCommit, "plugins/usage", "plugins/conversations", "packages"],
    { cwd: REPO_ROOT, encoding: null, maxBuffer: 64 * 1024 * 1024 },
  );
  if (archive.status !== 0) {
    throw new Error(`git archive ${tsCommit} failed: ${archive.stderr}`);
  }
  const tar = spawnSync("tar", ["-x", "-C", tsRoot], {
    input: archive.stdout as Buffer,
    encoding: null,
  });
  if (tar.status !== 0) throw new Error(`tar extract failed: ${tar.stderr}`);
}
const tsCli = path.join(tsRoot, "plugins/usage/src/cli.ts");
const tsTui = path.join(tsRoot, "plugins/usage/src/tui.ts");
const tsConvRoot = path.join(tsRoot, "plugins/conversations");
const rsConvRoot = path.join(REPO_ROOT, "plugins/conversations");
const rustBin = usageBinary();
const convBin = conversationsBinary();

// --- shared fixtures (read-only inputs for BOTH implementations) ------------

const CLAUDE_SESSION = "3bc481ff-60ac-491a-abd0-07c408c85e71";
const CODEX_SESSION = "01a1f843-77fa-7d30-b3c1-19f822a13215";
const CWD = "/repo/proj";

// Fake credential material — must never appear in compared output.
const CLAUDE_TOKEN = "diff-claude-token-7f3a9c";
const CLAUDE_REFRESH = "diff-claude-refresh-DO-NOT-LEAK";
const CODEX_TOKEN = "diff-codex-token-51bd02";
const CODEX_REFRESH = "diff-codex-refresh-DO-NOT-LEAK";
const HUB_KEY = "diff-cliproxy-key-SECRET";

const fixtureRoot = mkdtempSync(path.join(tmpdir(), "usage-diff-fixtures-"));
const claudeHome = path.join(fixtureRoot, "claude");
const codexHome = path.join(fixtureRoot, "codex");
mkdirSync(claudeHome, { recursive: true });
mkdirSync(codexHome, { recursive: true });

function encodeClaudeCwd(cwd: string): string {
  return cwd.replace(/[^A-Za-z0-9]/g, "-");
}
function writeJsonl(file: string, lines: unknown[]): void {
  mkdirSync(path.dirname(file), { recursive: true });
  writeFileSync(
    file,
    `${lines.map((l) => (typeof l === "string" ? l : JSON.stringify(l))).join("\n")}\n`,
  );
}

// claude transcript: two usage events incl. cache tokens + thinking + cost.
const claudeTs = (s: number) => `2026-01-01T00:00:${String(s).padStart(2, "0")}.000Z`;
writeJsonl(path.join(claudeHome, "projects", encodeClaudeCwd(CWD), `${CLAUDE_SESSION}.jsonl`), [
  { type: "user", uuid: "u-1", timestamp: claudeTs(1), message: { role: "user", content: "hi" } },
  {
    type: "assistant",
    uuid: "a-1",
    timestamp: claudeTs(2),
    costUSD: 0.0123,
    message: {
      id: "msg-a1",
      role: "assistant",
      model: "claude-test-1",
      content: [{ type: "text", text: "ok" }],
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
    timestamp: claudeTs(3),
    message: { role: "user", content: "again" },
  },
  {
    type: "assistant",
    uuid: "a-2",
    timestamp: claudeTs(4),
    message: {
      id: "msg-a2",
      role: "assistant",
      model: "claude-test-1",
      content: [{ type: "text", text: "ok" }],
      usage: {
        input_tokens: 20,
        output_tokens: 7,
        cache_read_input_tokens: 0,
        cache_creation_input_tokens: 0,
      },
    },
  },
]);

// codex rollout fixture.
const codexTs = (s: number) => `2026-01-02T00:00:${String(s).padStart(2, "0")}.000Z`;
writeJsonl(
  path.join(
    codexHome,
    "sessions",
    "2026",
    "01",
    "02",
    `rollout-2026-01-02T00-00-00-${CODEX_SESSION}.jsonl`,
  ),
  [
    {
      timestamp: codexTs(0),
      ordinal: 0,
      type: "session_meta",
      payload: { id: CODEX_SESSION, cwd: CWD },
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
      payload: {
        type: "item_completed",
        turn_id: "t1",
        item: { type: "UserMessage", id: "item-u1", content: [{ type: "text", text: "fix" }] },
      },
    },
    {
      timestamp: codexTs(3),
      ordinal: 3,
      type: "event_msg",
      payload: {
        type: "item_completed",
        turn_id: "t1",
        item: { type: "AgentMessage", id: "item-a1", content: [{ type: "text", text: "done" }] },
      },
    },
    {
      timestamp: codexTs(4),
      ordinal: 4,
      type: "event_msg",
      payload: {
        type: "token_count",
        info: {
          last_token_usage: {
            input_tokens: 100,
            cached_input_tokens: 40,
            cache_write_input_tokens: 6,
            output_tokens: 10,
            reasoning_output_tokens: 5,
            total_tokens: 110,
          },
        },
      },
    },
  ],
);

// Fake herdr socket for the one-time index build.
const sockDir = mkdtempSync(path.join(tmpdir(), "usage-diff-sock-"));
const socketPath = path.join(sockDir, "herdr.sock");
const PANES = [
  { pane_id: "w1:p1", workspace_id: "w1", tab_id: "w1:t1", cwd: CWD },
  { pane_id: "w1:p2", workspace_id: "w1", tab_id: "w1:t1", cwd: CWD },
];
const AGENTS = [
  {
    pane_id: "w1:p1",
    agent: "claude",
    agent_status: "idle",
    state_change_seq: 1,
    cwd: CWD,
    agent_session: { agent: "claude", kind: "session", value: CLAUDE_SESSION },
  },
  {
    pane_id: "w1:p2",
    agent: "codex",
    agent_status: "idle",
    state_change_seq: 1,
    cwd: CWD,
    agent_session: { agent: "codex", kind: "session", value: CODEX_SESSION },
  },
];

// --- mock servers (shared by both implementations) ----------------------------

const CLAUDE_QUOTA_BODY = {
  five_hour: { utilization: 42.5, resets_at: "2026-01-01T05:00:00.000Z" },
  seven_day: { utilization: 10, resets_at: "2026-01-08T00:00:00.000Z" },
};
const CODEX_QUOTA_BODY = {
  plan_type: "pro",
  rate_limit: {
    primary_window: { used_percent: 12.5, reset_at: 1_767_225_600, limit_window_seconds: 18_000 },
    secondary_window: { used_percent: 30, reset_at: 1_767_657_600 },
  },
};
const CODEX_CREDITS_BODY = {
  credits: [
    {
      id: "c1",
      status: "available",
      reset_type: "codex_rate_limits",
      expires_at: "2999-01-01T00:00:00.000Z",
    },
  ],
};
const PRICING_BODY = {
  "claude-test-1": {
    input_cost_per_token: 0.000003,
    output_cost_per_token: 0.000015,
    cache_read_input_token_cost: 0.0000003,
    cache_creation_input_token_cost: 0.00000375,
  },
};

interface MockServers {
  quotaBase: string;
  pricingUrl: string;
  hubBase: string;
  close: () => Promise<void>;
}

async function startMocks(): Promise<MockServers> {
  const servers: http.Server[] = [];
  const listen = (handler: http.RequestListener): Promise<number> =>
    new Promise((resolve, reject) => {
      const s = http.createServer(handler);
      s.once("error", reject);
      s.listen(0, "127.0.0.1", () => {
        const addr = s.address();
        resolve(typeof addr === "object" && addr !== null ? addr.port : 0);
        servers.push(s);
      });
    });
  const json = (res: http.ServerResponse, body: unknown, status = 200) => {
    res.writeHead(status, { "content-type": "application/json" });
    res.end(JSON.stringify(body));
  };

  const quotaPort = await listen((req, res) => {
    const route = req.url ?? "/";
    if (route === "/claude-usage") return json(res, CLAUDE_QUOTA_BODY);
    if (route === "/codex-usage") return json(res, CODEX_QUOTA_BODY);
    if (route === "/codex-credits") return json(res, CODEX_CREDITS_BODY);
    json(res, {}, 404);
  });
  const pricingPort = await listen((_req, res) => json(res, PRICING_BODY));
  const hubPort = await listen((req, res) => {
    const route = req.url ?? "/";
    res.setHeader("content-type", "application/json");
    if (route === "/v0/management/auth-files") {
      return json(res, {
        files: [
          { id: "acct-claude-1", auth_index: "idx-1", provider: "claude" },
          {
            id: "acct-codex-1",
            auth_index: "idx-2",
            provider: "codex",
            id_token: { chatgpt_account_id: "acct-x", plan_type: "pro" },
          },
        ],
      });
    }
    if (route === "/v0/management/api-call" && req.method === "POST") {
      let raw = "";
      req.on("data", (c: Buffer) => {
        raw += c.toString("utf8");
      });
      req.on("end", () => {
        const parsed = JSON.parse(raw) as { url?: string };
        const upstream = String(parsed.url ?? "");
        const body = upstream.includes("api/oauth/usage")
          ? CLAUDE_QUOTA_BODY
          : upstream.includes("rate-limit-reset-credits")
            ? CODEX_CREDITS_BODY
            : upstream.includes("wham/usage")
              ? CODEX_QUOTA_BODY
              : {};
        json(res, { status_code: 200, body: JSON.stringify(body) });
      });
      return;
    }
    json(res, {}, 404);
  });

  const base = (port: number) => `http://127.0.0.1:${port}`;
  return {
    quotaBase: base(quotaPort),
    pricingUrl: `${base(pricingPort)}/prices.json`,
    hubBase: base(hubPort),
    close: () =>
      Promise.all(servers.map((s) => new Promise((r) => s.close(() => r(null))))).then(
        () => undefined,
      ),
  };
}

// --- credentials ---------------------------------------------------------------

function jwt(payload: Record<string, unknown>): string {
  const enc = (o: unknown) => Buffer.from(JSON.stringify(o)).toString("base64url");
  return `${enc({ alg: "none" })}.${enc(payload)}.sig`;
}
writeFileSync(
  path.join(claudeHome, ".credentials.json"),
  JSON.stringify({
    claudeAiOauth: {
      accessToken: CLAUDE_TOKEN,
      refreshToken: CLAUDE_REFRESH,
      expiresAt: Date.now() + 3_600_000,
      subscriptionType: "max",
    },
  }),
);
writeFileSync(
  path.join(codexHome, "auth.json"),
  JSON.stringify({
    auth_mode: "chatgpt",
    tokens: {
      id_token: jwt({ chatgpt_account_id: "acct-test-1", plan_type: "pro" }),
      access_token: CODEX_TOKEN,
      refresh_token: CODEX_REFRESH,
    },
  }),
);

// --- one shared conversations index (native binary, read-only for both) ---------

const convStateDir = path.join(fixtureRoot, "conv-state");
mkdirSync(convStateDir, { recursive: true });
const convIndexEnv = testEnv({
  HERDR_PLUGIN_STATE_DIR: convStateDir,
  HERDR_PLUGIN_CONFIG_DIR: path.join(convStateDir, "config"),
  HERDR_SOCKET_PATH: socketPath,
  CLAUDE_CONFIG_DIR: claudeHome,
  CODEX_HOME: codexHome,
  OPENCODE_DATA_DIR: path.join(fixtureRoot, "no-opencode"),
  XDG_DATA_HOME: path.join(fixtureRoot, "no-xdg"),
});
const herdrServer = net.createServer((conn) => {
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
          ? { snapshot: { session: { name: "test-session" }, panes: PANES, agents: AGENTS } }
          : {};
      conn.write(`${JSON.stringify({ id: msg.id, result })}\n`);
    }
  });
});
await new Promise<void>((res, rej) => {
  herdrServer.once("error", rej);
  herdrServer.listen(herdrSocketEndpoint(socketPath), res);
});
{
  const proc = Bun.spawn([convBin, "index", "--socket", socketPath], {
    env: convIndexEnv,
    stdout: "pipe",
    stderr: "pipe",
  });
  const [out, err, status] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  if (status !== 0) {
    throw new Error(`conversations index failed (${status}): ${err.slice(0, 300)}`);
  }
  const parsed = JSON.parse(out) as { indexed?: unknown[] };
  if ((parsed.indexed ?? []).length !== 2) {
    throw new Error(`expected 2 indexed sessions, got ${JSON.stringify(parsed).slice(0, 300)}`);
  }
}
herdrServer.close();

// --- runners ---------------------------------------------------------------

interface RunResult {
  status: number | null;
  stdout: string;
  stderr: string;
}

interface Step {
  rs: string[];
  ts?: string[];
  tsScript?: "cli" | "tui";
  strip?: string[];
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
  const dir = mkdtempSync(path.join(tmpdir(), `usage-diff-${tag}-`));
  return { stateDir: path.join(dir, "state"), configDir: path.join(dir, "config") };
}

/** Shared read-only inputs + per-impl state dirs + mock endpoints. */
function cliEnv(dirs: Dirs, convRoot: string, mocks: MockServers): Record<string, string> {
  return testEnv({
    HERDR_PLUGIN_STATE_DIR: dirs.stateDir,
    HERDR_PLUGIN_CONFIG_DIR: dirs.configDir,
    HERDR_SOCKET_PATH: path.join(sockDir, "dead.sock"),
    HERDR_USAGE_CONVERSATIONS_STATE_DIR: convStateDir,
    HERDR_USAGE_CONVERSATIONS_ROOT: convRoot,
    HERDR_USAGE_QUOTA_CLAUDE_URL: `${mocks.quotaBase}/claude-usage`,
    HERDR_USAGE_QUOTA_CODEX_USAGE_URL: `${mocks.quotaBase}/codex-usage`,
    HERDR_USAGE_QUOTA_CODEX_CREDITS_URL: `${mocks.quotaBase}/codex-credits`,
    HERDR_USAGE_PRICING_URL: mocks.pricingUrl,
    CLAUDE_CONFIG_DIR: claudeHome,
    CODEX_HOME: codexHome,
    OPENCODE_DATA_DIR: path.join(fixtureRoot, "no-opencode"),
    XDG_DATA_HOME: path.join(fixtureRoot, "no-xdg"),
    HOME: path.join(fixtureRoot, "home-none"),
  });
}

/** hub config identical on both sides (config dirs are per-impl). */
function writeHubConfig(configDir: string, hubBase: string): void {
  mkdirSync(configDir, { recursive: true });
  writeFileSync(
    path.join(configDir, "config.json"),
    JSON.stringify({
      quotaHubs: [{ id: "test-hub", url: `${hubBase}/`, keyEnv: "HERDR_USAGE_QUOTA_HUB_KEY" }],
    }),
  );
}

// --- normalization ----------------------------------------------------------

const VOLATILE_KEYS = new Set([
  "generated_at",
  "checked_at",
  "fetched_at",
  "last_indexed_at",
  "first_event_at",
  "last_event_at",
]);
const NUMERIC_VOLATILE_KEYS = new Set(["age_seconds"]);
const ISO_TS = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}/;

let scrubPaths: string[] = [];
function scrubPathsInit(tsDirs: Dirs, rsDirs: Dirs) {
  const candidates = [
    sockDir,
    tsDirs.stateDir,
    tsDirs.configDir,
    rsDirs.stateDir,
    rsDirs.configDir,
    fixtureRoot,
    realpathSync(fixtureRoot),
    tsRoot,
    REPO_ROOT,
    convStateDir,
  ];
  scrubPaths = [...new Set(candidates.filter(Boolean))].sort((a, b) => b.length - a.length);
}

function scrubString(s: string): string {
  let out = s;
  for (const p of scrubPaths) {
    if (p && out.includes(p)) out = out.split(p).join("<tmp>");
  }
  // The retired TS entrypoints are invoked as `bun run src/cli.ts` /
  // `bun run src/tui.ts`; the native binary folds both into `usage`.
  out = out.split("bun run src/cli.ts").join("usage");
  out = out.split("bun run src/tui.ts").join("usage tui");
  return out;
}

function normalize(value: unknown, key?: string, strip?: ReadonlySet<string>): unknown {
  if (typeof value === "string") {
    if (key !== undefined && VOLATILE_KEYS.has(key)) return "<volatile>";
    if (ISO_TS.test(value)) return "<volatile>";
    return scrubString(value);
  }
  if (typeof value === "number") {
    if (key !== undefined && NUMERIC_VOLATILE_KEYS.has(key)) return "<volatile>";
    return value;
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

function assertNoSecrets(label: string, ...results: RunResult[]): void {
  for (const r of results) {
    const blob = `${r.stdout}\n${r.stderr}`;
    for (const secret of [
      CLAUDE_TOKEN,
      CLAUDE_REFRESH,
      CODEX_TOKEN,
      CODEX_REFRESH,
      HUB_KEY,
      "accessToken",
      "access_token",
      "refreshToken",
      "refresh_token",
    ]) {
      if (blob.includes(secret)) {
        console.log(`SECRET LEAK in ${label}: ${secret}`);
        process.exitCode = 1;
      }
    }
  }
}

// --- scenarios ---------------------------------------------------------------

const SCENARIOS: { name: string; steps: Step[]; hub?: boolean }[] = [
  {
    name: "usage-errors",
    steps: [
      { rs: ["bogus"] },
      { rs: ["usage", "--from", "not-a-date", "--offline"] },
      {
        rs: [
          "usage",
          "--from",
          "2026-02-01T00:00:00Z",
          "--to",
          "2026-01-01T00:00:00Z",
          "--offline",
        ],
      },
      { rs: ["usage", "--offline", "--conversations-root", path.join(fixtureRoot, "nope")] },
    ],
  },
  {
    name: "capability",
    steps: [{ rs: ["capability"] }],
  },
  {
    name: "usage-aggregation",
    steps: [
      { rs: ["usage", "--offline"] },
      { rs: ["usage", "--provider", "claude", "--offline"] },
      { rs: ["usage", "--pane", "w1:p2", "--offline"] },
      {
        rs: [
          "usage",
          "--from",
          "2026-01-02T00:00:00Z",
          "--to",
          "2026-01-03T00:00:00Z",
          "--offline",
        ],
      },
      { rs: ["usage", "--session-id", `claude:${CLAUDE_SESSION}`, "--offline"] },
    ],
  },
  {
    name: "quota",
    steps: [
      { rs: ["quota"] },
      { rs: ["quota", "--provider", "claude"] },
      { rs: ["quota", "--provider", "codex", "--refresh"] },
      { rs: ["quota", "--provider", "opencode"] },
      { rs: ["quota", "--provider", "bogus-provider"] },
    ],
  },
  {
    name: "cost",
    steps: [
      { rs: ["cost", "--offline"] },
      {
        rs: ["cost", "--from", "2026-01-01T00:00:00Z", "--to", "2026-01-01T05:00:00Z", "--offline"],
      },
    ],
  },
  {
    name: "divergence",
    steps: [
      {
        rs: [
          "divergence",
          "--offline",
          "--from",
          "2026-01-01T00:00:00Z",
          "--to",
          "2026-01-01T05:00:00Z",
        ],
      },
      { rs: ["divergence", "--offline"] },
      { rs: ["divergence", "--provider", "claude", "--offline"] },
    ],
  },
  {
    name: "hub-quota",
    hub: true,
    steps: [
      { rs: ["quota"] },
      { rs: ["quota", "--provider", "claude"] },
      { rs: ["quota", "--refresh"] },
    ],
  },
  {
    name: "tui-fallback",
    steps: [
      // TS ships the TUI as a separate entry (src/tui.ts); Rust folds it
      // into `usage tui`. Non-TTY both print the fallback notice.
      {
        rs: ["tui", "main"],
        ts: ["main"],
        tsScript: "tui",
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
const mocks = await startMocks();

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
  if (scenario.hub) {
    writeHubConfig(tsDirs.configDir, mocks.hubBase);
    writeHubConfig(rsDirs.configDir, mocks.hubBase);
  }
  const tsEnv = {
    ...cliEnv(tsDirs, tsConvRoot, mocks),
    HERDR_USAGE_QUOTA_HUB_KEY: HUB_KEY,
  };
  const rsEnv = {
    ...cliEnv(rsDirs, rsConvRoot, mocks),
    HERDR_USAGE_QUOTA_HUB_KEY: HUB_KEY,
  };

  for (let i = 0; i < scenario.steps.length; i++) {
    const step = scenario.steps[i] as Step;
    const label = `${scenario.name} #${i}: usage ${step.rs.join(" ")}`;
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
      assertNoSecrets(label, ts, rs);
    } else {
      report(
        false,
        label,
        `  status ts=${ts.status} rs=${rs.status}\n${stdoutDiff ?? ""}\n  ts.stderr: ${ts.stderr.slice(0, 300)}\n  rs.stderr: ${rs.stderr.slice(0, 300)}`,
      );
    }
  }

  // Quota/pricing cache parity: same files, same normalized contents.
  const readCacheDir = (stateDir: string): Record<string, unknown> => {
    const out: Record<string, unknown> = {};
    for (const sub of ["quota-cache", "pricing-cache"]) {
      const dir = path.join(stateDir, sub);
      if (!existsSync(dir)) continue;
      for (const f of readdirSyncSafe(dir).sort()) {
        try {
          out[`${sub}/${f}`] = normalize(JSON.parse(readFileSync(path.join(dir, f), "utf8")));
        } catch {
          out[`${sub}/${f}`] = "<unparseable>";
        }
      }
    }
    return out;
  };
  const cacheDiff = diffJson(readCacheDir(tsDirs.stateDir), readCacheDir(rsDirs.stateDir));
  report(cacheDiff === null, `${scenario.name}: cache files parity`, cacheDiff ?? undefined);
}

function readdirSyncSafe(dir: string): string[] {
  try {
    return readdirSync(dir);
  } catch {
    return [];
  }
}

// --- teardown ---------------------------------------------------------------

await mocks.close();
herdrServer.close();
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
