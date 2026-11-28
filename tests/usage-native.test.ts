import { afterAll, describe, expect, test } from "bun:test";
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import http from "node:http";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { conversationsBinary, conversationsPluginRootFixture } from "./helpers/conversations-bin";
import { herdrSocketEndpoint, testEnv } from "./helpers/test-env";
import { usageBinary } from "./helpers/usage-bin";

/**
 * Contract tests for the NATIVE rebuildup.usage implementation (Rust,
 * Issue #294). These spawn the shipped binary —
 * `plugins/usage/bin/usage` (committed prebuilt, ADR-0035) with a
 * cargo-built fallback — against the same fixture shapes the retired TS
 * suite used: conversations index via the real rebuildup.conversations
 * binary, fake herdr socket RPC, mock quota/pricing/hub HTTP servers,
 * and fake credential files. Wire behavior is verified end-to-end
 * without Bun or a live session.
 *
 * Pure-function coverage (window mappers, divergence builder, pricing
 * rate table/lookup/overrides, cost estimation, cache-token statuses,
 * state GC, TUI helpers) lives in the crate's Rust unit tests
 * (`cargo test -p usage`); TS↔Rust output parity is evidenced by the
 * diff harness documented in docs/research/usage-native-rust.md.
 */

const USAGE_BIN = usageBinary();
const CONV_BIN = conversationsBinary();
const CONV_ROOT = conversationsPluginRootFixture();

const dirs: string[] = [];
const savedEnv: Record<string, string | undefined> = {};
for (const key of [
  "HERDR_PLUGIN_STATE_DIR",
  "HERDR_PLUGIN_CONFIG_DIR",
  "HERDR_SOCKET_PATH",
  "HERDR_SESSION",
  "HERDR_PLUGIN_ROOT",
  "HERDR_USAGE_CONVERSATIONS_ROOT",
  "HERDR_USAGE_CONVERSATIONS_STATE_DIR",
  "HERDR_USAGE_CONVERSATIONS_CONFIG_DIR",
  "HERDR_USAGE_QUOTA_CLAUDE_URL",
  "HERDR_USAGE_QUOTA_CODEX_USAGE_URL",
  "HERDR_USAGE_QUOTA_CODEX_CREDITS_URL",
  "HERDR_USAGE_QUOTA_HUB_KEY",
  "HERDR_USAGE_PRICING_URL",
  "CLAUDE_CONFIG_DIR",
  "CODEX_HOME",
  "OPENCODE_DATA_DIR",
  "XDG_DATA_HOME",
  "HOME",
]) {
  savedEnv[key] = process.env[key];
}

function tempDir(): string {
  const dir = mkdtempSync(path.join(tmpdir(), "usage-native-"));
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

const CLAUDE_SESSION = "3bc481ff-60ac-491a-abd0-07c408c85e71";
const CODEX_SESSION = "01a1f843-77fa-7d30-b3c1-19f822a13215";
const CWD = "/repo/proj";

// Fake credential material — must never appear in any CLI output.
const CLAUDE_TOKEN = "claude-native-quota-token-7f3a9c";
const CLAUDE_REFRESH = "claude-native-refresh-DO-NOT-LEAK";
const CODEX_TOKEN = "codex-native-quota-token-51bd02";
const CODEX_REFRESH = "codex-native-refresh-DO-NOT-LEAK";
const HUB_KEY = "cliproxy-native-management-key-SECRET";

interface CliResult {
  status: number;
  stdout: string;
  stderr: string;
  json: Record<string, unknown> | null;
  error: Record<string, unknown> | null;
}

function baseEnv(extra?: Record<string, string>): Record<string, string> {
  return testEnv({ HERDR_SOCKET_PATH: DEAD_SOCKET, ...extra });
}

function parseOut(stdout: string, stderr: string, status: number): CliResult {
  let json: Record<string, unknown> | null = null;
  let error: Record<string, unknown> | null = null;
  try {
    json = JSON.parse(stdout) as Record<string, unknown>;
  } catch {
    /* non-JSON stdout */
  }
  try {
    error = (JSON.parse(stderr) as { error?: Record<string, unknown> }).error ?? null;
  } catch {
    /* empty stderr */
  }
  return { status, stdout, stderr, json, error };
}

function runUsageSync(args: string[], extraEnv?: Record<string, string>): CliResult {
  const result = spawnSync(USAGE_BIN, args, {
    encoding: "utf8",
    env: baseEnv(extraEnv),
    timeout: 60_000,
  });
  return parseOut(result.stdout, result.stderr, result.status ?? -1);
}

/** Async: required while in-process mock HTTP/socket servers serve. */
function runUsageAsync(args: string[], extraEnv?: Record<string, string>): Promise<CliResult> {
  return new Promise((resolve, reject) => {
    const child = spawn(USAGE_BIN, args, { env: baseEnv(extraEnv) });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString("utf8");
    });
    child.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
    });
    child.on("error", reject);
    child.on("close", (code) => resolve(parseOut(stdout, stderr, code ?? -1)));
  });
}

function runConversationsAsync(
  args: string[],
  extraEnv?: Record<string, string>,
): Promise<CliResult> {
  return new Promise((resolve, reject) => {
    const child = spawn(CONV_BIN, args, { env: baseEnv(extraEnv) });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString("utf8");
    });
    child.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
    });
    child.on("error", reject);
    child.on("close", (code) => resolve(parseOut(stdout, stderr, code ?? -1)));
  });
}

// --- shared fixtures -----------------------------------------------------------

function encodeClaudeCwd(cwd: string): string {
  return cwd.replace(/[^A-Za-z0-9]/g, "-");
}

function writeJson(file: string, value: unknown): void {
  mkdirSync(path.dirname(file), { recursive: true });
  writeFileSync(file, JSON.stringify(value));
}

interface FixtureEnv {
  convStateDir: string;
  usageStateDir: string;
  claudeHome: string;
  codexHome: string;
  convEnv: Record<string, string>;
  usageEnv: Record<string, string>;
}

function setupEnv(): FixtureEnv {
  const convStateDir = tempDir();
  const usageStateDir = tempDir();
  const claudeHome = tempDir();
  const codexHome = tempDir();
  const convEnv = {
    HERDR_PLUGIN_STATE_DIR: convStateDir,
    HERDR_PLUGIN_CONFIG_DIR: path.join(convStateDir, "config"),
    CLAUDE_CONFIG_DIR: claudeHome,
    CODEX_HOME: codexHome,
    OPENCODE_DATA_DIR: path.join(codexHome, "no-opencode"),
    XDG_DATA_HOME: path.join(codexHome, "no-xdg"),
  };
  const usageEnv = {
    HERDR_PLUGIN_STATE_DIR: usageStateDir,
    HERDR_PLUGIN_CONFIG_DIR: path.join(usageStateDir, "config"),
    HERDR_USAGE_CONVERSATIONS_STATE_DIR: convStateDir,
    HERDR_USAGE_CONVERSATIONS_ROOT: CONV_ROOT,
    CLAUDE_CONFIG_DIR: claudeHome,
    CODEX_HOME: codexHome,
    OPENCODE_DATA_DIR: path.join(codexHome, "no-opencode"),
    XDG_DATA_HOME: path.join(codexHome, "no-xdg"),
    HOME: tempDir(),
  };
  return { convStateDir, usageStateDir, claudeHome, codexHome, convEnv, usageEnv };
}

function claudeFixture(): unknown[] {
  const ts = (s: number) => `2026-01-01T00:00:${String(s).padStart(2, "0")}.000Z`;
  return [
    { type: "user", uuid: "u-1", timestamp: ts(1), message: { role: "user", content: "hi" } },
    {
      type: "assistant",
      uuid: "a-1",
      timestamp: ts(2),
      costUSD: 0.0123,
      message: {
        id: "msg-a-1",
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
    { type: "user", uuid: "u-2", timestamp: ts(3), message: { role: "user", content: "again" } },
    {
      type: "assistant",
      uuid: "a-2",
      timestamp: ts(4),
      message: {
        id: "msg-a-2",
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
  ];
}

function writeClaudeTranscript(claudeHome: string, cwd: string, sessionId: string): void {
  const dir = path.join(claudeHome, "projects", encodeClaudeCwd(cwd));
  mkdirSync(dir, { recursive: true });
  writeFileSync(
    path.join(dir, `${sessionId}.jsonl`),
    `${claudeFixture()
      .map((l) => (typeof l === "string" ? l : JSON.stringify(l)))
      .join("\n")}\n`,
  );
}

function writeCodexHome(codexHome: string): void {
  const ts = (s: number) => `2026-01-02T00:00:${String(s).padStart(2, "0")}.000Z`;
  const dir = path.join(codexHome, "sessions", "2026", "01", "02");
  mkdirSync(dir, { recursive: true });
  const lines = [
    {
      timestamp: ts(0),
      ordinal: 0,
      type: "session_meta",
      payload: { id: CODEX_SESSION, cwd: CWD },
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
      payload: {
        type: "item_completed",
        turn_id: "t1",
        item: { type: "UserMessage", id: "item-u1", content: [{ type: "text", text: "fix" }] },
      },
    },
    {
      timestamp: ts(3),
      ordinal: 3,
      type: "event_msg",
      payload: {
        type: "item_completed",
        turn_id: "t1",
        item: { type: "AgentMessage", id: "item-a1", content: [{ type: "text", text: "done" }] },
      },
    },
    {
      timestamp: ts(4),
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
  ];
  writeFileSync(
    path.join(dir, `rollout-2026-01-02T00-00-00-${CODEX_SESSION}.jsonl`),
    `${lines.map((l) => JSON.stringify(l)).join("\n")}\n`,
  );
}

/** Full fixture env: claude + codex transcripts written, not yet indexed. */
function setupIndexedFixtures(): FixtureEnv {
  const env = setupEnv();
  writeClaudeTranscript(env.claudeHome, CWD, CLAUDE_SESSION);
  writeCodexHome(env.codexHome);
  return env;
}

async function fakeHerdr(): Promise<{ socketPath: string; close: () => Promise<void> }> {
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
                  panes: [
                    { pane_id: "w1:p1", workspace_id: "w1", tab_id: "w1:t1", cwd: CWD },
                    { pane_id: "w1:p2", workspace_id: "w1", tab_id: "w1:t1", cwd: CWD },
                  ],
                  agents: [
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
                  ],
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
    close: () => new Promise((resolveClose) => server.close(() => resolveClose())),
  };
}

/** Index fixtures through the real conversations native binary. */
async function indexFixtures(env: FixtureEnv, herdrSocket: string): Promise<void> {
  const idx = await runConversationsAsync(["index", "--socket", herdrSocket], {
    ...env.convEnv,
    HERDR_SOCKET_PATH: herdrSocket,
  });
  expect(idx.status).toBe(0);
  expect(((idx.json?.indexed ?? []) as unknown[]).length).toBe(2);
}

/** Setup env with both providers indexed via a fake herdr socket. */
async function setupIndexed(): Promise<FixtureEnv> {
  const env = setupIndexedFixtures();
  const herdr = await fakeHerdr();
  try {
    await indexFixtures(env, herdr.socketPath);
  } finally {
    await herdr.close();
  }
  return env;
}

// --- mock HTTP servers -----------------------------------------------------------

type QuotaMode = "ok" | "fail" | "unauthorized" | "slow";

interface QuotaMock {
  url: (route: string) => string;
  hits: Record<string, number>;
  lastHeaders: Record<string, http.IncomingHttpHeaders | undefined>;
  setMode: (mode: QuotaMode) => void;
  close: () => Promise<void>;
}

const CLAUDE_QUOTA_BODY = {
  five_hour: { utilization: 42.5, resets_at: "2026-01-01T05:00:00.000Z" },
  seven_day: { utilization: 10, resets_at: "2026-01-08T00:00:00.000Z" },
  limits: [
    {
      kind: "weekly_scoped",
      percent: 55,
      resets_at: "2026-01-08T00:00:00.000Z",
      scope: { model: { display_name: "Fable" } },
    },
  ],
  extra_usage: { is_enabled: true, utilization: 3 },
};

const CODEX_QUOTA_BODY = {
  plan_type: "pro",
  rate_limit: {
    primary_window: {
      used_percent: 12.5,
      reset_at: 1_767_225_600,
      limit_window_seconds: 18_000,
    },
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
    {
      id: "c2",
      status: "available",
      reset_type: "codex_rate_limits",
      expires_at: "2000-01-01T00:00:00.000Z",
    },
    {
      id: "c3",
      status: "redeemed",
      reset_type: "codex_rate_limits",
      expires_at: "2999-01-01T00:00:00.000Z",
    },
  ],
};

/** Direct provider quota endpoint mock (claude/codex usage + credits). */
async function quotaMock(): Promise<QuotaMock> {
  const hits: Record<string, number> = {};
  const lastHeaders: Record<string, http.IncomingHttpHeaders | undefined> = {};
  let mode: QuotaMode = "ok";
  const server = http.createServer((req, res) => {
    const route = req.url ?? "/";
    hits[route] = (hits[route] ?? 0) + 1;
    lastHeaders[route] = req.headers;
    res.setHeader("content-type", "application/json");
    if (mode === "fail") {
      res.statusCode = 500;
      res.end(JSON.stringify({ error: "boom" }));
      return;
    }
    if (mode === "unauthorized") {
      res.statusCode = 401;
      res.end(JSON.stringify({ error: "unauthorized" }));
      return;
    }
    if (mode === "slow") {
      setTimeout(() => {
        res.statusCode = 200;
        res.end(JSON.stringify(CLAUDE_QUOTA_BODY));
      }, 5_000);
      return;
    }
    if (route === "/claude-usage") res.end(JSON.stringify(CLAUDE_QUOTA_BODY));
    else if (route === "/codex-usage") res.end(JSON.stringify(CODEX_QUOTA_BODY));
    else if (route === "/codex-credits") res.end(JSON.stringify(CODEX_CREDITS_BODY));
    else {
      res.statusCode = 404;
      res.end("{}");
    }
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => resolve());
  });
  const address = server.address();
  const port = typeof address === "object" && address !== null ? address.port : 0;
  return {
    url: (route: string) => `http://127.0.0.1:${port}${route}`,
    hits,
    lastHeaders,
    setMode: (m) => {
      mode = m;
    },
    close: () => new Promise((resolve) => server.close(() => resolve())),
  };
}

interface HubAuthFile {
  id: string;
  auth_index: string;
  provider: string;
  disabled?: boolean;
  id_token?: Record<string, unknown>;
}

interface HubMock extends QuotaMock {
  apiCalls: Record<string, unknown>[];
}

/** CLIProxyAPI management API mock: auth-files + api-call. */
async function hubMock(files: HubAuthFile[]): Promise<HubMock> {
  const hits: Record<string, number> = {};
  const lastHeaders: Record<string, http.IncomingHttpHeaders | undefined> = {};
  const apiCalls: Record<string, unknown>[] = [];
  let mode: QuotaMode = "ok";
  const respondFor = (upstreamUrl: string): { status: number; body: unknown } => {
    if (upstreamUrl.includes("api/oauth/usage")) {
      return { status: 200, body: CLAUDE_QUOTA_BODY };
    }
    if (upstreamUrl.includes("rate-limit-reset-credits")) {
      return { status: 200, body: CODEX_CREDITS_BODY };
    }
    if (upstreamUrl.includes("wham/usage")) {
      return { status: 200, body: CODEX_QUOTA_BODY };
    }
    return { status: 404, body: { error: "unknown upstream" } };
  };
  const server = http.createServer((req, res) => {
    const route = req.url ?? "/";
    hits[route] = (hits[route] ?? 0) + 1;
    lastHeaders[route] = req.headers;
    res.setHeader("content-type", "application/json");
    if (mode === "unauthorized") {
      res.statusCode = 401;
      res.end(JSON.stringify({ error: "bad key" }));
      return;
    }
    if (mode === "fail") {
      res.statusCode = 500;
      res.end(JSON.stringify({ error: "boom" }));
      return;
    }
    if (route === "/v0/management/auth-files" && req.method === "GET") {
      res.end(JSON.stringify({ files }));
      return;
    }
    if (route === "/v0/management/api-call" && req.method === "POST") {
      let raw = "";
      req.on("data", (chunk: Buffer) => {
        raw += chunk.toString("utf8");
      });
      req.on("end", () => {
        let parsed: Record<string, unknown> = {};
        try {
          parsed = JSON.parse(raw) as Record<string, unknown>;
        } catch {
          parsed = { parse_error: raw.slice(0, 200) };
        }
        apiCalls.push(parsed);
        const respond = () => {
          const upstream = respondFor(typeof parsed.url === "string" ? parsed.url : "");
          res.end(
            JSON.stringify({ status_code: upstream.status, body: JSON.stringify(upstream.body) }),
          );
        };
        // "slow" delays only api-call — auth-files stays fast so the
        // per-account timeout path is what gets exercised.
        if (mode === "slow") setTimeout(respond, 5_000);
        else respond();
      });
      return;
    }
    res.statusCode = 404;
    res.end("{}");
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", () => resolve());
  });
  const address = server.address();
  const port = typeof address === "object" && address !== null ? address.port : 0;
  return {
    url: (route: string) => `http://127.0.0.1:${port}${route}`,
    hits,
    lastHeaders,
    apiCalls,
    setMode: (m) => {
      mode = m;
    },
    close: () => new Promise((resolve) => server.close(() => resolve())),
  };
}

/** Tiny LiteLLM-shaped price server for the `cost` command. */
async function pricingServer(
  document: unknown,
  status = 200,
): Promise<{ url: string; close: () => Promise<void> }> {
  const server = http.createServer((_req, res) => {
    res.writeHead(status, { "content-type": "application/json" });
    res.end(typeof document === "string" ? document : JSON.stringify(document));
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  const port = typeof address === "object" && address !== null ? address.port : 0;
  return {
    url: `http://127.0.0.1:${port}/prices.json`,
    close: () => new Promise((resolve) => server.close(() => resolve())),
  };
}

// --- credential fixtures ---------------------------------------------------------

function jwt(payload: Record<string, unknown>): string {
  const enc = (o: unknown) => Buffer.from(JSON.stringify(o)).toString("base64url");
  return `${enc({ alg: "none" })}.${enc(payload)}.sig`;
}

function writeClaudeCredentials(claudeHome: string, expiresAtMs?: number): void {
  writeJson(path.join(claudeHome, ".credentials.json"), {
    claudeAiOauth: {
      accessToken: CLAUDE_TOKEN,
      refreshToken: CLAUDE_REFRESH,
      expiresAt: expiresAtMs ?? Date.now() + 3_600_000,
      subscriptionType: "max",
      scopes: ["user:profile"],
      rateLimitTier: "default_claude_max_20x",
    },
  });
}

function writeCodexAuth(codexHome: string, authMode = "chatgpt"): void {
  writeJson(path.join(codexHome, "auth.json"), {
    auth_mode: authMode,
    OPENAI_API_KEY: authMode === "apikey" ? "sk-test-not-a-real-key" : null,
    tokens:
      authMode === "chatgpt"
        ? {
            id_token: jwt({
              chatgpt_account_id: "acct-test-1",
              plan_type: "pro",
              email: "someone@example.com",
            }),
            access_token: CODEX_TOKEN,
            refresh_token: CODEX_REFRESH,
          }
        : null,
    last_refresh: "2026-01-01T00:00:00Z",
  });
}

interface QuotaEnv {
  stateDir: string;
  claudeHome: string;
  codexHome: string;
  env: Record<string, string>;
}

function quotaEnv(server: QuotaMock): QuotaEnv {
  const stateDir = tempDir();
  const claudeHome = tempDir();
  const codexHome = tempDir();
  writeClaudeCredentials(claudeHome);
  writeCodexAuth(codexHome);
  return {
    stateDir,
    claudeHome,
    codexHome,
    env: {
      HERDR_PLUGIN_STATE_DIR: stateDir,
      HERDR_PLUGIN_CONFIG_DIR: path.join(stateDir, "config"),
      HOME: tempDir(),
      CLAUDE_CONFIG_DIR: claudeHome,
      CODEX_HOME: codexHome,
      HERDR_USAGE_QUOTA_CLAUDE_URL: server.url("/claude-usage"),
      HERDR_USAGE_QUOTA_CODEX_USAGE_URL: server.url("/codex-usage"),
      HERDR_USAGE_QUOTA_CODEX_CREDITS_URL: server.url("/codex-credits"),
    },
  };
}

function isolatedProviderEnv(): Record<string, string> {
  return {
    HOME: tempDir(),
    CLAUDE_CONFIG_DIR: tempDir(),
    CODEX_HOME: tempDir(),
    OPENCODE_DATA_DIR: path.join(tempDir(), "no-opencode"),
    XDG_DATA_HOME: path.join(tempDir(), "no-xdg"),
  };
}

function hubEnv(server: HubMock): Record<string, string> {
  const stateDir = tempDir();
  const configDir = tempDir();
  writeJson(path.join(configDir, "config.json"), {
    quotaHubs: [{ id: "test-hub", url: server.url(""), keyEnv: "HERDR_USAGE_QUOTA_HUB_KEY" }],
  });
  return {
    HERDR_PLUGIN_STATE_DIR: stateDir,
    HERDR_PLUGIN_CONFIG_DIR: configDir,
    ...isolatedProviderEnv(),
  };
}

interface ProviderEntry {
  provider: string;
  status: string;
  detail: string | null;
  plan: string | null;
  windows: {
    id: string;
    kind: string;
    used_percent: number;
    resets_at: string | null;
    window_duration_mins: number | null;
  }[];
  checked_at: string | null;
  stale: boolean;
  stale_reason: string | null;
  reset_credits_available: number | null;
  cache: { hit: boolean; age_seconds: number | null; ttl_seconds: number };
}

function providersOf(json: Record<string, unknown> | null): ProviderEntry[] {
  return (json?.providers ?? []) as ProviderEntry[];
}

function expectNoSecrets(r: CliResult): void {
  const blob = `${r.stdout}\n${r.stderr}`;
  for (const secret of [
    CLAUDE_TOKEN,
    CLAUDE_REFRESH,
    CODEX_TOKEN,
    CODEX_REFRESH,
    "refreshToken",
    "refresh_token",
    "accessToken",
    "access_token",
    "someone@example.com",
    HUB_KEY,
    "$TOKEN$",
  ]) {
    expect(blob).not.toContain(secret);
  }
}

const t = (name: string, fn: () => void | Promise<void>) => test(name, fn, 60_000);

// --- contract: capability --------------------------------------------------------

describe("capability", () => {
  t("reports read-only contract, sources, and all commands", () => {
    const { usageEnv } = setupEnv();
    const r = runUsageSync(["capability"], usageEnv);
    expect(r.status).toBe(0);
    expect(r.json?.contract).toBe("herdr-plugins.usage/v1");
    expect(r.json?.read_only).toBe(true);
    const sources = r.json?.sources as Record<string, unknown>[];
    const index = sources.find((s) => s.id === "conversations-index");
    expect(index?.status).toBe("available");
    expect(index?.contract).toBe("herdr-plugins.conversations/v1");
    const hub = sources.find((s) => s.id === "cliproxy-api");
    expect(hub?.status).toBe("available");
    expect(hub?.credential_boundary).toContain("0015");
    const quota = sources.find((s) => s.id === "subscription-quota-api");
    expect(quota?.status).toBe("available");
    expect(quota?.credential_boundary).toContain("0014");
    const pricing = sources.find((s) => s.id === "model-pricing");
    expect(pricing?.kind).toBe("price-table");
    const commands = r.json?.commands as string[];
    for (const cmd of ["usage", "quota", "cost", "divergence"]) {
      expect(commands).toContain(cmd);
    }
  });
});

// --- contract: usage aggregation ---------------------------------------------------

describe("usage aggregation", () => {
  t("aggregates claude+codex fixtures end-to-end with cache tokens", async () => {
    const env = await setupIndexed();
    const r = runUsageSync(["usage", "--offline"], env.usageEnv);
    expect(r.status).toBe(0);
    expect(r.json?.contract).toBe("herdr-plugins.usage/v1");
    const totals = r.json?.totals as Record<string, unknown>;
    expect(totals.events).toBe(3);
    expect(totals.input_tokens).toBe(130);
    expect(totals.output_tokens).toBe(22);
    expect(totals.cache_read_input_tokens).toBe(43);
    expect(totals.cache_creation_input_tokens).toBe(8);
    expect(totals.thinking_tokens).toBe(6);
    expect(totals.provider_reported_cost_usd as number).toBeCloseTo(0.0123, 6);
    expect(totals.total_tokens).toBe(203);

    const breakdowns = r.json?.breakdowns as Record<
      string,
      { key: string | null; events: number; input_tokens: number }[]
    >;
    const models = breakdowns.by_model ?? [];
    expect(models.some((b) => b.key === "claude-test-1" && b.events === 2)).toBe(true);
    expect(models.some((b) => b.key === "gpt-6-test" && b.events === 1)).toBe(true);
    const providers = breakdowns.by_provider ?? [];
    expect(providers.find((b) => b.key === "claude")?.input_tokens).toBe(30);
    expect(providers.find((b) => b.key === "codex")?.input_tokens).toBe(100);
    const projects = breakdowns.by_project ?? [];
    expect(projects.every((b) => b.key === CWD)).toBe(true);

    const coverage = r.json?.coverage as Record<string, unknown>;
    expect(coverage.sessions_total).toBe(2);
    expect(coverage.sessions_with_issues).toBe(0);
    const provCoverage = coverage.providers as Record<string, unknown>[];
    expect(provCoverage.find((p) => p.provider === "claude")?.provider_status).toBe("supported");
    expect(provCoverage.find((p) => p.provider === "codex")?.usage_events).toBe(1);

    const source = r.json?.source as Record<string, unknown>;
    expect(source.contract).toBe("herdr-plugins.conversations/v1");
    expect(source.complete).toBe(true);
    expect(source.resolved_via).toBe("override");
    expect(((r.json?.warnings ?? []) as unknown[]).length).toBe(0);

    // Per-provider cache-token breakdown rides the usage payload.
    const cacheTokens = r.json?.cache_tokens as {
      providers: {
        provider: string;
        status: string;
        events: number;
        cache_read_input_tokens: number;
        cache_creation_input_tokens: number;
      }[];
    };
    const claudeCache = cacheTokens.providers.find((p) => p.provider === "claude");
    expect(claudeCache?.status).toBe("reported");
    expect(claudeCache?.events).toBe(2);
    expect(claudeCache?.cache_read_input_tokens).toBe(3);
    expect(claudeCache?.cache_creation_input_tokens).toBe(2);
    const codexCache = cacheTokens.providers.find((p) => p.provider === "codex");
    expect(codexCache?.status).toBe("reported");
    expect(codexCache?.events).toBe(1);
  });

  t("selectors and time window filter the aggregation", async () => {
    const env = await setupIndexed();

    const byProvider = runUsageSync(["usage", "--provider", "claude", "--offline"], env.usageEnv);
    expect(((byProvider.json?.totals ?? {}) as Record<string, unknown>).events).toBe(2);
    expect(((byProvider.json?.totals ?? {}) as Record<string, unknown>).input_tokens).toBe(30);

    const byPane = runUsageSync(["usage", "--pane", "w1:p2", "--offline"], env.usageEnv);
    expect(((byPane.json?.totals ?? {}) as Record<string, unknown>).input_tokens).toBe(100);

    const windowed = runUsageSync(
      ["usage", "--from", "2026-01-02T00:00:00Z", "--to", "2026-01-03T00:00:00Z", "--offline"],
      env.usageEnv,
    );
    const wt = windowed.json?.totals as Record<string, unknown>;
    expect(wt.events).toBe(1);
    expect(wt.input_tokens).toBe(100);

    const badFrom = runUsageSync(["usage", "--from", "not-a-date", "--offline"], env.usageEnv);
    expect(badFrom.status).toBe(2);
    const inverted = runUsageSync(
      ["usage", "--from", "2026-02-01T00:00:00Z", "--to", "2026-01-01T00:00:00Z", "--offline"],
      env.usageEnv,
    );
    expect(inverted.status).toBe(2);
  });

  t("empty index and missing source are explicit, never fabricated", () => {
    const env = setupEnv();
    const empty = runUsageSync(["usage", "--offline"], env.usageEnv);
    expect(empty.status).toBe(0);
    expect(((empty.json?.totals ?? {}) as Record<string, unknown>).events).toBe(0);
    expect(empty.json?.hint).toContain("index");
    expect(((empty.json?.coverage ?? {}) as Record<string, unknown>).sessions_total).toBe(0);

    const missing = runUsageSync(
      ["usage", "--offline", "--conversations-root", path.join(tempDir(), "nope")],
      env.usageEnv,
    );
    expect(missing.status).toBe(1);
    expect(missing.error?.kind).toBe("conversations_unavailable");
  });
});

// --- contract: quota -----------------------------------------------------------------

describe("usage quota contract", () => {
  t("claude+codex quota report windows, plans, credits; no secrets leak", async () => {
    const server = await quotaMock();
    try {
      const q = quotaEnv(server);
      const r = await runUsageAsync(["quota"], q.env);
      expect(r.status).toBe(0);
      expect(r.json?.contract).toBe("herdr-plugins.usage/v1");
      expect(r.json?.command).toBe("quota");
      expectNoSecrets(r);

      const providers = providersOf(r.json);
      const claude = providers.find((p) => p.provider === "claude");
      const codex = providers.find((p) => p.provider === "codex");
      const opencode = providers.find((p) => p.provider === "opencode");

      expect(claude?.status).toBe("ok");
      expect(claude?.plan).toBe("max");
      expect(claude?.windows.find((w) => w.id === "five_hour")?.used_percent).toBe(42.5);
      expect(claude?.cache.hit).toBe(false);
      expect(claude?.checked_at).not.toBeNull();

      expect(codex?.status).toBe("ok");
      expect(codex?.plan).toBe("Pro");
      expect(codex?.windows.find((w) => w.id === "primary")?.used_percent).toBe(12.5);
      expect(codex?.reset_credits_available).toBe(1); // c1 only: c2 expired, c3 redeemed

      expect(opencode?.status).toBe("unsupported");
      expect(opencode?.windows).toEqual([]);

      // The mock saw the bearer tokens server-side (expected); the
      // plugin output must not contain them (asserted above).
      expect(server.lastHeaders["/claude-usage"]?.authorization).toBe(`Bearer ${CLAUDE_TOKEN}`);
      const codexHeaders = server.lastHeaders["/codex-usage"];
      expect(codexHeaders?.authorization).toBe(`Bearer ${CODEX_TOKEN}`);
      expect(codexHeaders?.["chatgpt-account-id"]).toBe("acct-test-1");
      expect(codexHeaders?.["openai-beta"]).toBe("codex-1");
    } finally {
      await server.close();
    }
  });

  t("fresh cache hits skip the endpoint; --refresh refetches", async () => {
    const server = await quotaMock();
    try {
      const q = quotaEnv(server);
      const first = await runUsageAsync(["quota", "--provider", "claude"], q.env);
      expect(first.status).toBe(0);
      expect(server.hits["/claude-usage"]).toBe(1);

      const second = await runUsageAsync(["quota", "--provider", "claude"], q.env);
      const claude = providersOf(second.json).find((p) => p.provider === "claude");
      expect(claude?.status).toBe("ok");
      expect(claude?.cache.hit).toBe(true);
      expect(claude?.cache.age_seconds).not.toBeNull();
      expect(claude?.cache.ttl_seconds).toBe(300);
      expect(server.hits["/claude-usage"]).toBe(1); // no new request

      const refreshed = await runUsageAsync(["quota", "--provider", "claude", "--refresh"], q.env);
      const rc = providersOf(refreshed.json).find((p) => p.provider === "claude");
      expect(rc?.cache.hit).toBe(false);
      expect(server.hits["/claude-usage"]).toBe(2);

      // Cache file holds quota numbers only — no credential material.
      const cacheFile = path.join(q.stateDir, "quota-cache", "claude.json");
      expect(existsSync(cacheFile)).toBe(true);
      const cacheBlob = readFileSync(cacheFile, "utf8");
      expect(cacheBlob).not.toContain(CLAUDE_TOKEN);
      expect(cacheBlob).not.toContain(CLAUDE_REFRESH);
      expect(cacheBlob).not.toContain("accessToken");
      expectNoSecrets(refreshed);
    } finally {
      await server.close();
    }
  });

  t("endpoint failure → request_failed; cached reading served stale", async () => {
    const server = await quotaMock();
    try {
      const q = quotaEnv(server);
      const ok = await runUsageAsync(["quota", "--provider", "claude"], q.env);
      expect(providersOf(ok.json).find((p) => p.provider === "claude")?.status).toBe("ok");

      server.setMode("fail");
      const q2 = quotaEnv(server);
      const failed = await runUsageAsync(["quota", "--provider", "claude"], q2.env);
      const fc = providersOf(failed.json).find((p) => p.provider === "claude");
      expect(fc?.status).toBe("request_failed");
      expect(fc?.detail).toContain("HTTP 500");
      expect(fc?.windows).toEqual([]);
      expect(fc?.checked_at).toBeNull();
      expectNoSecrets(failed);

      const stale = await runUsageAsync(["quota", "--provider", "claude", "--refresh"], q.env);
      const sc = providersOf(stale.json).find((p) => p.provider === "claude");
      expect(sc?.status).toBe("ok");
      expect(sc?.stale).toBe(true);
      expect(sc?.stale_reason).toContain("fetch failed");
      expect(sc?.windows.length ?? 0).toBeGreaterThan(0);
      expectNoSecrets(stale);
    } finally {
      await server.close();
    }
  });

  t("expired claude token → credential_expired without any HTTP call", async () => {
    const server = await quotaMock();
    try {
      const q = quotaEnv(server);
      writeClaudeCredentials(q.claudeHome, Date.now() - 60_000);
      const r = await runUsageAsync(["quota", "--provider", "claude"], q.env);
      const claude = providersOf(r.json).find((p) => p.provider === "claude");
      expect(claude?.status).toBe("credential_expired");
      expect(claude?.windows).toEqual([]);
      expect(claude?.detail).toContain("expired");
      expect(server.hits["/claude-usage"] ?? 0).toBe(0);
      expectNoSecrets(r);
    } finally {
      await server.close();
    }
  });

  t("missing credentials → no_credential; apikey codex → unsupported", async () => {
    const server = await quotaMock();
    try {
      const stateDir = tempDir();
      const emptyClaude = tempDir();
      const emptyCodex = tempDir();
      const env = {
        HERDR_PLUGIN_STATE_DIR: stateDir,
        HERDR_PLUGIN_CONFIG_DIR: path.join(stateDir, "config"),
        HOME: tempDir(),
        CLAUDE_CONFIG_DIR: emptyClaude,
        CODEX_HOME: emptyCodex,
        HERDR_USAGE_QUOTA_CLAUDE_URL: server.url("/claude-usage"),
        HERDR_USAGE_QUOTA_CODEX_USAGE_URL: server.url("/codex-usage"),
      };
      const r = await runUsageAsync(["quota"], env);
      expect(r.status).toBe(0);
      const providers = providersOf(r.json);
      expect(providers.find((p) => p.provider === "claude")?.status).toBe("no_credential");
      expect(providers.find((p) => p.provider === "codex")?.status).toBe("no_credential");
      expect(server.hits["/claude-usage"] ?? 0).toBe(0);
      expect(server.hits["/codex-usage"] ?? 0).toBe(0);

      writeCodexAuth(emptyCodex, "apikey");
      const r2 = await runUsageAsync(["quota", "--provider", "codex"], env);
      const codex = providersOf(r2.json).find((p) => p.provider === "codex");
      expect(codex?.status).toBe("unsupported");
      expect(codex?.detail).toContain("apikey");

      const r3 = await runUsageAsync(["quota", "--provider", "bogus-provider"], env);
      const bogus = providersOf(r3.json).find((p) => p.provider === "bogus-provider");
      expect(bogus?.status).toBe("unsupported");
      expectNoSecrets(r);
      expectNoSecrets(r2);
      expectNoSecrets(r3);
    } finally {
      await server.close();
    }
  });
});

// --- contract: cost -------------------------------------------------------------------

describe("cost command", () => {
  t("end-to-end: estimates per model, unpriced listed, estimate labeled", async () => {
    const env = await setupIndexed();
    const server = await pricingServer({
      "claude-test-1": {
        input_cost_per_token: 0.000003,
        output_cost_per_token: 0.000015,
        cache_read_input_token_cost: 0.0000003,
        cache_creation_input_token_cost: 0.00000375,
      },
      // gpt-6-test deliberately absent → unpriced
    });
    try {
      const r = await runUsageAsync(["cost", "--offline"], {
        ...env.usageEnv,
        HERDR_USAGE_PRICING_URL: server.url,
      });
      expect(r.status).toBe(0);
      expect(r.json?.contract).toBe("herdr-plugins.usage/v1");
      expect(r.json?.command).toBe("cost");

      const cost = r.json?.cost as Record<string, unknown>;
      expect(cost.estimate).toBe(true);
      expect(String(cost.disclaimer)).toContain("not a bill");
      const pricing = cost.pricing as Record<string, unknown>;
      expect(pricing.status).toBe("ok");
      expect(pricing.source).toBe(server.url);
      expect(typeof pricing.known_models).toBe("number");

      const byModel = cost.by_model as Record<string, unknown>[];
      const claude = byModel.find((m) => m.key === "claude-test-1");
      expect(claude?.cost_source).toBe("model_priced");
      expect(claude?.estimated_cost_usd as number).toBeCloseTo(0.0002784, 10);
      expect(claude?.provider_reported_cost_usd as number).toBeCloseTo(0.0123, 6);

      const codex = byModel.find((m) => m.key === "gpt-6-test");
      expect(codex?.cost_source).toBe("unpriced");
      expect(codex?.estimated_cost_usd).toBeNull();

      expect(cost.priced_models).toBe(1);
      expect((cost.unpriced_models as unknown[]).length).toBe(1);
      expect(cost.estimate_complete).toBe(false);
      expect(cost.estimated_cost_usd as number).toBeCloseTo(0.0002784, 10);
      expect(cost.provider_reported_cost_usd as number).toBeCloseTo(0.0123, 6);
      expect(((r.json?.warnings ?? []) as string[]).includes("unpriced_models")).toBe(true);
    } finally {
      await server.close();
    }
  });

  t("pricing source down → pricing_unavailable, all unpriced, exit 0", async () => {
    const env = await setupIndexed();
    const server = await pricingServer("nope", 500);
    try {
      const r = await runUsageAsync(["cost", "--offline"], {
        ...env.usageEnv,
        HERDR_USAGE_PRICING_URL: server.url,
      });
      expect(r.status).toBe(0);
      const cost = r.json?.cost as Record<string, unknown>;
      expect((cost.pricing as Record<string, unknown>).status).toBe("unavailable");
      expect(cost.estimated_cost_usd).toBeNull();
      expect(cost.estimate).toBe(true);
      const byModel = cost.by_model as Record<string, unknown>[];
      expect(byModel.every((m) => m.cost_source === "unpriced")).toBe(true);
      expect(((r.json?.warnings ?? []) as string[]).includes("pricing_unavailable")).toBe(true);
    } finally {
      await server.close();
    }
  });

  t("config modelPrices override prices an unlisted model", async () => {
    const env = await setupIndexed();
    const server = await pricingServer({});
    try {
      const configDir = path.join(env.usageStateDir, "config");
      mkdirSync(configDir, { recursive: true });
      writeFileSync(
        path.join(configDir, "config.json"),
        JSON.stringify({
          modelPrices: {
            "gpt-6-test": {
              inputCostPerMillionTokens: 2.5,
              outputCostPerMillionTokens: 10,
            },
          },
        }),
      );
      const r = await runUsageAsync(["cost", "--offline"], {
        ...env.usageEnv,
        HERDR_USAGE_PRICING_URL: server.url,
      });
      expect(r.status).toBe(0);
      const cost = r.json?.cost as Record<string, unknown>;
      const pricing = cost.pricing as Record<string, unknown>;
      expect(pricing.user_overrides).toBe(1);
      const byModel = cost.by_model as Record<string, unknown>[];
      const codex = byModel.find((m) => m.key === "gpt-6-test");
      expect(codex?.cost_source).toBe("model_priced");
      expect(codex?.estimated_cost_usd as number).toBeCloseTo(0.000465, 10);
      expect(cost.estimate_complete).toBe(false);
      expect(
        (cost.unpriced_models as { key: string }[]).some((m) => m.key === "claude-test-1"),
      ).toBe(true);
    } finally {
      await server.close();
    }
  });
});

// --- contract: divergence ----------------------------------------------------------------

describe("divergence command", () => {
  t("reports quota vs measured side by side, honestly", async () => {
    const server = await quotaMock();
    try {
      const env0 = await setupIndexed();
      writeClaudeCredentials(env0.claudeHome);
      writeCodexAuth(env0.codexHome);
      const env = {
        ...env0.usageEnv,
        HERDR_USAGE_QUOTA_CLAUDE_URL: server.url("/claude-usage"),
        HERDR_USAGE_QUOTA_CODEX_USAGE_URL: server.url("/codex-usage"),
        HERDR_USAGE_QUOTA_CODEX_CREDITS_URL: server.url("/codex-credits"),
      };
      const r = await runUsageAsync(
        [
          "divergence",
          "--offline",
          "--from",
          "2026-01-01T00:00:00Z",
          "--to",
          "2026-01-01T05:00:00Z",
        ],
        env,
      );
      expect(r.status).toBe(0);
      expect(r.json?.command).toBe("divergence");
      const comparisons = r.json?.comparisons as Record<string, unknown>[];
      const claude = comparisons.find((c) => c.provider === "claude");
      expect(claude?.verdict).toBe("different_units");
      const measured = claude?.measured as Record<string, unknown>;
      expect(measured.input_tokens).toBe(30);
      expect(measured.cache_read_input_tokens).toBe(3);
      const quota = claude?.quota as Record<string, unknown>;
      expect(quota.status).toBe("ok");
      expect((quota.windows as { used_percent: number }[])[0]?.used_percent).toBe(42.5);
      expect(claude?.window_alignment).toBe("matches_quota_window");

      const codex = comparisons.find((c) => c.provider === "codex");
      expect(codex?.verdict).toBe("no_measured_usage");
      const opencode = comparisons.find((c) => c.provider === "opencode");
      expect(opencode?.verdict).toBe("no_data");

      const blob = `${r.stdout}\n${r.stderr}`;
      expect(blob).not.toContain(CLAUDE_TOKEN);
      expect(blob).not.toContain(CLAUDE_REFRESH);
      expectNoSecrets(r);
    } finally {
      await server.close();
    }
  });

  t("quota credential missing → quota_unavailable, measured still reported", async () => {
    const server = await quotaMock();
    try {
      const env0 = await setupIndexed();
      const env = {
        ...env0.usageEnv,
        HERDR_USAGE_QUOTA_CLAUDE_URL: server.url("/claude-usage"),
      };
      const r = await runUsageAsync(["divergence", "--offline"], env);
      expect(r.status).toBe(0);
      const claude = ((r.json?.comparisons ?? []) as Record<string, unknown>[]).find(
        (c) => c.provider === "claude",
      );
      expect(claude?.verdict).toBe("quota_unavailable");
      expect(((claude?.measured ?? {}) as Record<string, unknown>).events).toBe(2);
      expect(((claude?.quota ?? {}) as Record<string, unknown>).status).toBe("no_credential");
      expect(server.hits["/claude-usage"] ?? 0).toBe(0);
    } finally {
      await server.close();
    }
  });
});

// --- contract: cliproxy hub quota source ---------------------------------------------------

describe("cliproxy hub quota source", () => {
  const HUB_FILES: HubAuthFile[] = [
    { id: "acct-claude-1", auth_index: "idx-1", provider: "claude" },
    {
      id: "acct-codex-1",
      auth_index: "idx-2",
      provider: "codex",
      id_token: { chatgpt_account_id: "acct-x", plan_type: "pro" },
    },
    { id: "acct-grok-1", auth_index: "idx-3", provider: "xai" },
    { id: "acct-disabled", auth_index: "idx-4", provider: "claude", disabled: true },
  ];

  t("hub auth-files + api-call map provider windows; no secrets leak", async () => {
    const server = await hubMock(HUB_FILES);
    try {
      const env = { ...hubEnv(server), HERDR_USAGE_QUOTA_HUB_KEY: HUB_KEY };
      const r = await runUsageAsync(["quota", "--provider", "claude"], env);
      expect(r.status).toBe(0);
      const blob = `${r.stdout}\n${r.stderr}`;
      expect(blob).not.toContain(HUB_KEY);
      expect(blob).not.toContain("$TOKEN$");

      const hubs = r.json?.hubs as Record<string, unknown>[];
      const hub = hubs.find((h) => h.id === "test-hub");
      expect(hub?.status).toBe("ok");
      expect(hub?.kind).toBe("cliproxy-api");
      expect(hub?.endpoint).toBe(`http://127.0.0.1:${new URL(server.url("")).port}/`);
      expect(hub?.stale).toBe(false);

      const accounts = hub?.accounts as Record<string, unknown>[];
      const claude = accounts.find((a) => a.account === "acct-claude-1");
      const codex = accounts.find((a) => a.account === "acct-codex-1");
      const grok = accounts.find((a) => a.account === "acct-grok-1");
      expect(accounts.find((a) => a.account === "acct-disabled")).toBeUndefined();

      expect(claude?.status).toBe("ok");
      expect(claude?.plan).toBe("Claude Subscription");
      const windows = claude?.windows as { id: string; used_percent: number }[];
      expect(windows.find((w) => w.id === "five_hour")?.used_percent).toBe(42.5);

      expect(codex?.status).toBe("ok");
      expect(codex?.plan).toBe("Pro");
      expect(codex?.reset_credits_available).toBe(1);

      expect(grok?.status).toBe("unsupported");
      expect(grok?.windows).toEqual([]);

      expect(server.lastHeaders["/v0/management/auth-files"]?.authorization).toBe(
        `Bearer ${HUB_KEY}`,
      );
      const claudeCall = server.apiCalls.find((c) => String(c.url).includes("api/oauth/usage"));
      const header = claudeCall?.header as Record<string, string>;
      expect(header.authorization).toBe("Bearer $TOKEN$");
      expect(header["anthropic-beta"]).toBe("oauth-2025-04-20");
      const codexCall = server.apiCalls.find((c) => String(c.url).includes("wham/usage"));
      const codexHeader = codexCall?.header as Record<string, string>;
      expect(codexHeader["Chatgpt-Account-Id"]).toBe("acct-x");

      const stateDir = (env as Record<string, string>).HERDR_PLUGIN_STATE_DIR as string;
      const cacheFile = path.join(stateDir, "quota-cache", "hub-test_hub.json");
      expect(existsSync(cacheFile)).toBe(true);
      const cacheBlob = readFileSync(cacheFile, "utf8");
      expect(cacheBlob).not.toContain(HUB_KEY);
      expect(cacheBlob).not.toContain("$TOKEN$");
      expect(cacheBlob).not.toContain("authorization");
    } finally {
      await server.close();
    }
  });

  t("missing key env → no_credential without any HTTP call", async () => {
    const server = await hubMock(HUB_FILES);
    try {
      const env = hubEnv(server); // no HERDR_USAGE_QUOTA_HUB_KEY
      const r = await runUsageAsync(["quota"], env);
      expect(r.status).toBe(0);
      const hubs = r.json?.hubs as Record<string, unknown>[];
      const hub = hubs.find((h) => h.id === "test-hub");
      expect(hub?.status).toBe("no_credential");
      expect(hub?.detail).toContain("HERDR_USAGE_QUOTA_HUB_KEY");
      expect(server.hits["/v0/management/auth-files"] ?? 0).toBe(0);
      const blob = `${r.stdout}\n${r.stderr}`;
      expect(blob).not.toContain(HUB_KEY);
    } finally {
      await server.close();
    }
  });

  t("hub 401 → unauthorized; transport failure → request_failed; stale cache flagged", async () => {
    const server = await hubMock(HUB_FILES);
    try {
      const env = { ...hubEnv(server), HERDR_USAGE_QUOTA_HUB_KEY: HUB_KEY };
      const ok = await runUsageAsync(["quota"], env);
      expect(((ok.json?.hubs ?? []) as Record<string, unknown>[])[0]?.status).toBe("ok");

      server.setMode("unauthorized");
      const denied = await runUsageAsync(["quota", "--refresh"], env);
      const dh = ((denied.json?.hubs ?? []) as Record<string, unknown>[])[0];
      expect(dh?.status).toBe("ok");
      expect(dh?.stale).toBe(true);
      expect(dh?.stale_reason).toContain("hub request failed");

      const env2 = { ...hubEnv(server), HERDR_USAGE_QUOTA_HUB_KEY: HUB_KEY };
      const denied2 = await runUsageAsync(["quota"], env2);
      const dh2 = ((denied2.json?.hubs ?? []) as Record<string, unknown>[])[0];
      expect(dh2?.status).toBe("unauthorized");
      expect(dh2?.accounts).toEqual([]);

      server.setMode("fail");
      const env3 = { ...hubEnv(server), HERDR_USAGE_QUOTA_HUB_KEY: HUB_KEY };
      const failed = await runUsageAsync(["quota"], env3);
      const fh = ((failed.json?.hubs ?? []) as Record<string, unknown>[])[0];
      expect(fh?.status).toBe("request_failed");
      expect(fh?.detail).toContain("HTTP 500");
    } finally {
      await server.close();
    }
  });

  t("slow api-call → per-account request_failed, hub stays ok", async () => {
    const server = await hubMock(HUB_FILES);
    try {
      const env = { ...hubEnv(server), HERDR_USAGE_QUOTA_HUB_KEY: HUB_KEY };
      server.setMode("slow");
      const r = await runUsageAsync(["quota", "--timeout", "400"], env);
      expect(r.status).toBe(0);
      const hub = ((r.json?.hubs ?? []) as Record<string, unknown>[])[0];
      if (hub?.status === "ok") {
        const accounts = hub.accounts as Record<string, unknown>[];
        expect(accounts.every((a) => a.status !== "ok")).toBe(true);
      } else {
        expect(["request_failed", "unauthorized"]).toContain(String(hub?.status));
      }
    } finally {
      await server.close();
    }
  });
});

// --- contract: TUI non-TTY fallback --------------------------------------------------------

describe("tui entrypoint", () => {
  t("non-TTY prints CLI fallback notice and exits 0", () => {
    const result = spawnSync(USAGE_BIN, ["tui", "main"], {
      encoding: "utf8",
      env: testEnv(),
      timeout: 30_000,
    });
    expect(result.status).toBe(0);
    expect(result.stdout).toContain("requires an interactive terminal");
  });
});
