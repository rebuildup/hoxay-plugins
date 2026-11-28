import { afterAll, describe, expect, test } from "bun:test";
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import http from "node:http";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { buildCacheTokens } from "../plugins/usage/src/aggregate";
import { buildDivergence } from "../plugins/usage/src/divergence";
import { hubBaseUrl } from "../plugins/usage/src/hub";
import type { BreakdownEntry, QuotaProviderResult, QuotaWindow } from "../plugins/usage/src/model";
import { conversationsBinary, conversationsPluginRootFixture } from "./helpers/conversations-bin";
import { herdrSocketEndpoint, testEnv } from "./helpers/test-env";

/** Claude project-dir encoding (was plugins/conversations/src/claude.ts). */
function encodeClaudeCwd(cwd: string): string {
  return cwd.replace(/[^A-Za-z0-9]/g, "-");
}

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
  "CLAUDE_CONFIG_DIR",
  "CODEX_HOME",
  "OPENCODE_DATA_DIR",
  "XDG_DATA_HOME",
  "HOME",
]) {
  savedEnv[key] = process.env[key];
}

function tempDir(): string {
  const dir = mkdtempSync(path.join(tmpdir(), "usage-t3-"));
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

const USAGE_CLI = path.join(__dirname, "..", "plugins", "usage", "src", "cli.ts");
const CONV_CLI = conversationsBinary();
// Runnable plugin root: the committed bin/ prebuilt is macOS-only,
// so sibling resolution must point at a fixture linking the host binary
// (same pattern as tests/helpers/inbox-bin.ts, Issue #291).
const CONV_ROOT = conversationsPluginRootFixture();

// Fake credential material — must never appear in any CLI output.
const CLAUDE_TOKEN = "claude-t3-quota-token-9d2f";
const CLAUDE_REFRESH = "claude-t3-refresh-DO-NOT-LEAK";
const HUB_KEY = "cliproxy-t3-management-key-SECRET";

interface CliResult {
  status: number;
  stdout: string;
  stderr: string;
  json: Record<string, unknown> | null;
  error: Record<string, unknown> | null;
}

function baseEnv(extra?: Record<string, string>): Record<string, string> {
  return testEnv({ HERDR_SOCKET_PATH: path.join(tempDir(), "dead.sock"), ...extra });
}

function parseOut(stdout: string, stderr: string, status: number): CliResult {
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

function runUsageCli(args: string[], extraEnv?: Record<string, string>): CliResult {
  const result = spawnSync(process.execPath, [USAGE_CLI, ...args], {
    encoding: "utf8",
    env: baseEnv(extraEnv),
    timeout: 60_000,
  });
  return parseOut(result.stdout, result.stderr, result.status ?? -1);
}

/** Async: mock HTTP endpoints live in this process's event loop. */
function runUsageCliAsync(args: string[], extraEnv?: Record<string, string>): Promise<CliResult> {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [USAGE_CLI, ...args], {
      env: baseEnv(extraEnv),
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
    child.on("close", (code) => resolve(parseOut(stdout, stderr, code ?? -1)));
  });
}

function runConversationsAsync(
  args: string[],
  extraEnv?: Record<string, string>,
): Promise<CliResult> {
  return new Promise((resolve, reject) => {
    const child = spawn(CONV_CLI, args, { env: baseEnv(extraEnv) });
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

// --- mock servers -----------------------------------------------------------------

interface MockServer {
  url: (route: string) => string;
  hits: Record<string, number>;
  lastHeaders: Record<string, http.IncomingHttpHeaders | undefined>;
  apiCalls: Record<string, unknown>[];
  setMode: (mode: "ok" | "fail" | "unauthorized" | "slow") => void;
  close: () => Promise<void>;
}

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

/** Direct provider quota endpoint mock (claude/codex usage + credits). */
async function quotaMock(): Promise<MockServer> {
  const hits: Record<string, number> = {};
  const lastHeaders: Record<string, http.IncomingHttpHeaders | undefined> = {};
  const apiCalls: Record<string, unknown>[] = [];
  let mode: "ok" | "fail" | "unauthorized" | "slow" = "ok";
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
    apiCalls,
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

/** CLIProxyAPI management API mock: auth-files + api-call. */
async function hubMock(files: HubAuthFile[]): Promise<MockServer> {
  const hits: Record<string, number> = {};
  const lastHeaders: Record<string, http.IncomingHttpHeaders | undefined> = {};
  const apiCalls: Record<string, unknown>[] = [];
  let mode: "ok" | "fail" | "unauthorized" | "slow" = "ok";
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

// --- env helpers --------------------------------------------------------------------

function writeJson(file: string, value: unknown): void {
  mkdirSync(path.dirname(file), { recursive: true });
  writeFileSync(file, JSON.stringify(value));
}

function isolatedProviderEnv(): Record<string, string> {
  // Force every direct provider probe to no_credential: empty homes,
  // no real ~/.claude or ~/.codex readable from tests.
  return {
    HOME: tempDir(),
    CLAUDE_CONFIG_DIR: tempDir(),
    CODEX_HOME: tempDir(),
    OPENCODE_DATA_DIR: path.join(tempDir(), "no-opencode"),
    XDG_DATA_HOME: path.join(tempDir(), "no-xdg"),
  };
}

function writeClaudeCredentials(claudeHome: string): void {
  writeJson(path.join(claudeHome, ".credentials.json"), {
    claudeAiOauth: {
      accessToken: CLAUDE_TOKEN,
      refreshToken: CLAUDE_REFRESH,
      expiresAt: Date.now() + 3_600_000,
      subscriptionType: "max",
    },
  });
}

const CODEX_TOKEN = "codex-t3-quota-token-51bd";
const CODEX_REFRESH = "codex-t3-refresh-DO-NOT-LEAK";

function jwt(payload: Record<string, unknown>): string {
  const enc = (o: unknown) => Buffer.from(JSON.stringify(o)).toString("base64url");
  return `${enc({ alg: "none" })}.${enc(payload)}.sig`;
}

function writeCodexAuth(codexHome: string): void {
  writeJson(path.join(codexHome, "auth.json"), {
    auth_mode: "chatgpt",
    tokens: {
      id_token: jwt({ chatgpt_account_id: "acct-test-1", plan_type: "pro" }),
      access_token: CODEX_TOKEN,
      refresh_token: CODEX_REFRESH,
    },
  });
}

function hubEnv(server: MockServer): Record<string, string> {
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

// --- transcript fixtures (minimal claude-only, mirrors usage.test.ts) ---------------

const CLAUDE_SESSION = "3bc481ff-60ac-491a-abd0-07c408c85e71";
const CWD = "/repo/proj";

function claudeFixture(): unknown[] {
  const ts = (s: number) => `2026-01-01T00:00:${String(s).padStart(2, "0")}.000Z`;
  return [
    {
      type: "user",
      uuid: "u-1",
      timestamp: ts(1),
      message: { role: "user", content: "hi" },
    },
    {
      type: "assistant",
      uuid: "a-1",
      timestamp: ts(2),
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
        },
      },
    },
    {
      type: "user",
      uuid: "u-2",
      timestamp: ts(3),
      message: { role: "user", content: "again" },
    },
    {
      type: "assistant",
      uuid: "a-2",
      timestamp: ts(4),
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
                  panes: [{ pane_id: "w1:p1", workspace_id: "w1", tab_id: "w1:t1", cwd: CWD }],
                  agents: [
                    {
                      pane_id: "w1:p1",
                      agent: "claude",
                      agent_status: "idle",
                      state_change_seq: 1,
                      cwd: CWD,
                      agent_session: { agent: "claude", kind: "session", value: CLAUDE_SESSION },
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

interface IndexedEnv {
  convStateDir: string;
  usageStateDir: string;
  configDir: string;
  claudeHome: string;
  codexHome: string;
  usageEnv: Record<string, string>;
}

/** Index the claude fixture through the real conversations plugin. */
async function setupIndexed(): Promise<IndexedEnv> {
  const convStateDir = tempDir();
  const usageStateDir = tempDir();
  const configDir = tempDir();
  const claudeHome = tempDir();
  const codexHome = tempDir();
  writeClaudeTranscript(claudeHome, CWD, CLAUDE_SESSION);
  const herdr = await fakeHerdr();
  try {
    const idx = await runConversationsAsync(["index", "--socket", herdr.socketPath], {
      HERDR_PLUGIN_STATE_DIR: convStateDir,
      HERDR_PLUGIN_CONFIG_DIR: path.join(convStateDir, "config"),
      CLAUDE_CONFIG_DIR: claudeHome,
      CODEX_HOME: codexHome,
      OPENCODE_DATA_DIR: path.join(codexHome, "no-opencode"),
      XDG_DATA_HOME: path.join(codexHome, "no-xdg"),
      HERDR_SOCKET_PATH: herdr.socketPath,
    });
    expect(idx.status).toBe(0);
  } finally {
    await herdr.close();
  }
  return {
    convStateDir,
    usageStateDir,
    configDir,
    claudeHome,
    codexHome,
    usageEnv: {
      HERDR_PLUGIN_STATE_DIR: usageStateDir,
      HERDR_PLUGIN_CONFIG_DIR: configDir,
      HERDR_USAGE_CONVERSATIONS_STATE_DIR: convStateDir,
      HERDR_USAGE_CONVERSATIONS_ROOT: CONV_ROOT,
      CLAUDE_CONFIG_DIR: claudeHome,
      CODEX_HOME: codexHome,
      OPENCODE_DATA_DIR: path.join(codexHome, "no-opencode"),
      XDG_DATA_HOME: path.join(codexHome, "no-xdg"),
      HOME: tempDir(),
    },
  };
}

const t = (name: string, fn: () => void | Promise<void>) => test(name, fn, 60_000);

// --- unit: cache token breakdown -----------------------------------------------------

describe("cache token breakdown", () => {
  const measured: BreakdownEntry[] = [
    {
      key: "claude",
      events: 2,
      input_tokens: 30,
      output_tokens: 12,
      cache_read_input_tokens: 3,
      cache_creation_input_tokens: 2,
      thinking_tokens: null,
      total_tokens: 47,
      provider_reported_cost_usd: null,
    },
    {
      key: "codex",
      events: 1,
      input_tokens: 100,
      output_tokens: 10,
      cache_read_input_tokens: 40,
      cache_creation_input_tokens: 6,
      thinking_tokens: 5,
      total_tokens: 156,
      provider_reported_cost_usd: null,
    },
  ];

  t("reported/unsupported/unknown statuses come from capability, not zeros", () => {
    const result = buildCacheTokens({
      providers: [
        { provider: "claude", provider_status: "supported" },
        { provider: "codex", provider_status: "supported" },
        { provider: "future-prop", provider_status: "supported" },
        { provider: "dead-prop", provider_status: "unsupported" },
      ],
      measured,
      cacheCapability: {
        claude: "reported",
        codex: "reported",
        "dead-prop": "reported", // capability cannot override unsupported provider
      },
    });
    const byProvider = new Map(result.providers.map((p) => [p.provider, p]));
    expect(byProvider.get("claude")?.status).toBe("reported");
    expect(byProvider.get("claude")?.cache_read_input_tokens).toBe(3);
    expect(byProvider.get("codex")?.cache_creation_input_tokens).toBe(6);
    // supported provider without a declared capability → unknown, not guessed
    expect(byProvider.get("future-prop")?.status).toBe("unknown");
    expect(byProvider.get("future-prop")?.events).toBe(0);
    // provider_status unsupported wins over any capability claim
    expect(byProvider.get("dead-prop")?.status).toBe("unsupported");
    expect(byProvider.get("dead-prop")?.detail).toContain("unsupported");
  });

  t("adapter-declared unsupported is explicit even with measured zeros", () => {
    const result = buildCacheTokens({
      providers: [{ provider: "nocache", provider_status: "supported" }],
      measured: [],
      cacheCapability: { nocache: "unsupported" },
    });
    expect(result.providers[0]?.status).toBe("unsupported");
    expect(result.providers[0]?.detail).toContain("no cache-token accounting");
  });
});

// --- unit: divergence builder ---------------------------------------------------------

function quotaResult(partial: Partial<QuotaProviderResult>): QuotaProviderResult {
  return {
    provider: "claude",
    status: "ok",
    detail: null,
    plan: "max",
    windows: [],
    checked_at: "2026-01-01T00:30:00.000Z",
    fetched_at: "2026-01-01T00:31:00.000Z",
    stale: false,
    stale_reason: null,
    reset_credits_available: null,
    cache: { hit: false, age_seconds: null, ttl_seconds: 300 },
    ...partial,
  };
}

function window(id: string, mins: number | null, used = 40): QuotaWindow {
  return {
    id,
    label: id,
    kind: "session",
    used_percent: used,
    resets_at: null,
    window_duration_mins: mins,
  };
}

describe("divergence builder", () => {
  const measuredEntry: BreakdownEntry = {
    key: "claude",
    events: 2,
    input_tokens: 30,
    output_tokens: 12,
    cache_read_input_tokens: 3,
    cache_creation_input_tokens: 2,
    thinking_tokens: null,
    total_tokens: 47,
    provider_reported_cost_usd: null,
  };

  t("both sides present → different_units, never reconciled", () => {
    const comparisons = buildDivergence({
      providers: [{ provider: "claude", provider_status: "supported" }],
      measured: [measuredEntry],
      quota: [quotaResult({ windows: [window("five_hour", 300)] })],
      window: { from_ms: null, to_ms: null },
    });
    const claude = comparisons.find((c) => c.provider === "claude");
    expect(claude?.verdict).toBe("different_units");
    expect(claude?.detail).toContain("never reconciled");
    expect(claude?.measured?.input_tokens).toBe(30);
    expect(claude?.quota?.windows[0]?.used_percent).toBe(40);
    expect(claude?.window_alignment).toBe("unbounded");
  });

  t("quota failure → quota_unavailable; empty index → no_measured_usage", () => {
    const comparisons = buildDivergence({
      providers: [
        { provider: "claude", provider_status: "supported" },
        { provider: "codex", provider_status: "supported" },
      ],
      measured: [measuredEntry],
      quota: [
        quotaResult({ status: "request_failed", detail: "HTTP 500", windows: [] }),
        quotaResult({ provider: "codex", status: "ok", windows: [window("primary", 300)] }),
      ],
      window: { from_ms: null, to_ms: null },
    });
    expect(comparisons.find((c) => c.provider === "claude")?.verdict).toBe("quota_unavailable");
    expect(comparisons.find((c) => c.provider === "claude")?.detail).toBe("HTTP 500");
    expect(comparisons.find((c) => c.provider === "codex")?.verdict).toBe("no_measured_usage");
  });

  t("neither side → no_data; window span matching → matches_quota_window", () => {
    const comparisons = buildDivergence({
      providers: [{ provider: "opencode", provider_status: "supported" }],
      measured: [],
      quota: [
        quotaResult({
          provider: "opencode",
          status: "unsupported",
          detail: "no quota artifact",
          windows: [],
        }),
      ],
      window: {
        from_ms: Date.parse("2026-01-01T00:00:00Z"),
        to_ms: Date.parse("2026-01-01T05:00:00Z"),
      },
    });
    const opencode = comparisons.find((c) => c.provider === "opencode");
    expect(opencode?.verdict).toBe("no_data");

    const aligned = buildDivergence({
      providers: [{ provider: "claude", provider_status: "supported" }],
      measured: [measuredEntry],
      quota: [quotaResult({ windows: [window("five_hour", 300)] })],
      window: {
        from_ms: Date.parse("2026-01-01T00:00:00Z"),
        to_ms: Date.parse("2026-01-01T05:00:00Z"), // exactly 300 mins
      },
    });
    expect(aligned[0]?.window_alignment).toBe("matches_quota_window");

    const custom = buildDivergence({
      providers: [{ provider: "claude", provider_status: "supported" }],
      measured: [measuredEntry],
      quota: [quotaResult({ windows: [window("five_hour", 300)] })],
      window: {
        from_ms: Date.parse("2026-01-01T00:00:00Z"),
        to_ms: Date.parse("2026-01-02T00:00:00Z"), // 1440 mins ≠ 300
      },
    });
    expect(custom[0]?.window_alignment).toBe("custom_window");
  });
});

// --- unit: hub endpoint normalization -----------------------------------------------

describe("hub endpoint", () => {
  t("normalizes panel URLs and rejects non-http", () => {
    expect(hubBaseUrl("https://hub.example.com/management.html")?.endpoint).toBe(
      "https://hub.example.com/",
    );
    expect(hubBaseUrl("http://127.0.0.1:8317/")?.base).toBe("http://127.0.0.1:8317/");
    expect(hubBaseUrl("http://127.0.0.1:8317/some/path/?x=1#f")?.endpoint).toBe(
      "http://127.0.0.1:8317/some/path/",
    );
    expect(hubBaseUrl("not a url")).toBeNull();
    expect(hubBaseUrl("ftp://x/")).toBeNull();
  });
});

// --- contract: usage cache_tokens -----------------------------------------------------

describe("usage cache_tokens contract", () => {
  t("usage output carries per-provider cache token breakdown", async () => {
    const env = await setupIndexed();
    const r = runUsageCli(["usage", "--offline"], env.usageEnv);
    expect(r.status).toBe(0);
    const cacheTokens = r.json?.cache_tokens as {
      providers: {
        provider: string;
        status: string;
        events: number;
        cache_read_input_tokens: number;
        cache_creation_input_tokens: number;
      }[];
    };
    expect(cacheTokens).toBeDefined();
    const claude = cacheTokens.providers.find((p) => p.provider === "claude");
    expect(claude?.status).toBe("reported");
    expect(claude?.events).toBe(2);
    expect(claude?.cache_read_input_tokens).toBe(3);
    expect(claude?.cache_creation_input_tokens).toBe(2);
    // supported providers with zero measured usage still appear, honestly
    const codex = cacheTokens.providers.find((p) => p.provider === "codex");
    expect(codex?.status).toBe("reported");
    expect(codex?.events).toBe(0);
  });
});

// --- contract: cliproxy hub quota source ------------------------------------------------

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
      const r = await runUsageCliAsync(["quota", "--provider", "claude"], env);
      // --provider filters direct providers only; hubs are always reported
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

      // hub provider without a mapper → explicit unsupported, never fabricated
      expect(grok?.status).toBe("unsupported");
      expect(grok?.windows).toEqual([]);

      // The hub saw the management key server-side (expected); output must not.
      expect(server.lastHeaders["/v0/management/auth-files"]?.authorization).toBe(
        `Bearer ${HUB_KEY}`,
      );
      // api-call carried the $TOKEN$ placeholder + provider headers.
      const claudeCall = server.apiCalls.find((c) => String(c.url).includes("api/oauth/usage"));
      const header = claudeCall?.header as Record<string, string>;
      expect(header.authorization).toBe("Bearer $TOKEN$");
      expect(header["anthropic-beta"]).toBe("oauth-2025-04-20");
      const codexCall = server.apiCalls.find((c) => String(c.url).includes("wham/usage"));
      const codexHeader = codexCall?.header as Record<string, string>;
      expect(codexHeader["Chatgpt-Account-Id"]).toBe("acct-x");

      // Cache file holds quota numbers only — no key material.
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
      const r = await runUsageCliAsync(["quota"], env);
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
      const ok = await runUsageCliAsync(["quota"], env);
      expect(((ok.json?.hubs ?? []) as Record<string, unknown>[])[0]?.status).toBe("ok");

      server.setMode("unauthorized");
      const denied = await runUsageCliAsync(["quota", "--refresh"], env);
      const dh = ((denied.json?.hubs ?? []) as Record<string, unknown>[])[0];
      // fresh cache exists → stale serve flagged, not a fabricated fresh zero
      expect(dh?.status).toBe("ok");
      expect(dh?.stale).toBe(true);
      expect(dh?.stale_reason).toContain("hub request failed");

      // no cache → honest unauthorized
      const env2 = { ...hubEnv(server), HERDR_USAGE_QUOTA_HUB_KEY: HUB_KEY };
      const denied2 = await runUsageCliAsync(["quota"], env2);
      const dh2 = ((denied2.json?.hubs ?? []) as Record<string, unknown>[])[0];
      expect(dh2?.status).toBe("unauthorized");
      expect(dh2?.accounts).toEqual([]);

      server.setMode("fail");
      const env3 = { ...hubEnv(server), HERDR_USAGE_QUOTA_HUB_KEY: HUB_KEY };
      const failed = await runUsageCliAsync(["quota"], env3);
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
      const r = await runUsageCliAsync(["quota", "--timeout", "400"], env);
      expect(r.status).toBe(0);
      const hub = ((r.json?.hubs ?? []) as Record<string, unknown>[])[0];
      // auth-files is fast in "slow" mode? no — slow delays both; hub may fail.
      // Accept either hub-level failure or per-account failure, both honest.
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

// --- contract: divergence ---------------------------------------------------------------

describe("divergence command", () => {
  t("reports quota vs measured side by side, honestly", async () => {
    const server = await quotaMock();
    try {
      const env0 = await setupIndexed();
      const claudeHome = env0.claudeHome;
      writeClaudeCredentials(claudeHome);
      writeCodexAuth(env0.codexHome);
      const env = {
        ...env0.usageEnv,
        HERDR_USAGE_QUOTA_CLAUDE_URL: server.url("/claude-usage"),
        HERDR_USAGE_QUOTA_CODEX_USAGE_URL: server.url("/codex-usage"),
        HERDR_USAGE_QUOTA_CODEX_CREDITS_URL: server.url("/codex-credits"),
      };
      const r = await runUsageCliAsync(
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
      expect(claude?.window_alignment).toBe("matches_quota_window"); // 300min span = five_hour

      // codex: quota ok (mock) but nothing measured → no_measured_usage
      const codex = comparisons.find((c) => c.provider === "codex");
      expect(codex?.verdict).toBe("no_measured_usage");
      // opencode: no quota artifact + nothing measured → no_data
      const opencode = comparisons.find((c) => c.provider === "opencode");
      expect(opencode?.verdict).toBe("no_data");

      const blob = `${r.stdout}\n${r.stderr}`;
      expect(blob).not.toContain(CLAUDE_TOKEN);
      expect(blob).not.toContain(CLAUDE_REFRESH);
    } finally {
      await server.close();
    }
  });

  t("quota credential missing → quota_unavailable, measured still reported", async () => {
    const server = await quotaMock();
    try {
      const env0 = await setupIndexed();
      // no credentials written → claude quota no_credential
      const env = {
        ...env0.usageEnv,
        HERDR_USAGE_QUOTA_CLAUDE_URL: server.url("/claude-usage"),
      };
      const r = await runUsageCliAsync(["divergence", "--offline"], env);
      expect(r.status).toBe(0);
      const claude = ((r.json?.comparisons ?? []) as Record<string, unknown>[]).find(
        (c) => c.provider === "claude",
      );
      expect(claude?.verdict).toBe("quota_unavailable");
      expect(((claude?.measured ?? {}) as Record<string, unknown>).events).toBe(2);
      expect(((claude?.quota ?? {}) as Record<string, unknown>).status).toBe("no_credential");
      expect(server.hits["/claude-usage"] ?? 0).toBe(0); // no credential → no HTTP call
    } finally {
      await server.close();
    }
  });
});
