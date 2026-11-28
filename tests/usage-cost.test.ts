import { afterAll, describe, expect, test } from "bun:test";
import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import http from "node:http";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { computeModelCosts } from "../plugins/usage/src/cost";
import type { BreakdownEntry } from "../plugins/usage/src/model";
import {
  collectPricing,
  createOverrideRateTable,
  estimateBucketCostUsd,
  lookupRate,
  PRICING_CACHE_RETENTION_SECONDS,
  parseRateTable,
} from "../plugins/usage/src/pricing";
import { collectQuota } from "../plugins/usage/src/quota";
import { pluginRoot } from "../plugins/usage/src/source";
import { gcPluginStateCache, PLUGIN_CACHE_RETENTION_SECONDS } from "../plugins/usage/src/state";
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
  "HERDR_USAGE_PRICING_URL",
  "CLAUDE_CONFIG_DIR",
  "CODEX_HOME",
  "OPENCODE_DATA_DIR",
  "XDG_DATA_HOME",
]) {
  savedEnv[key] = process.env[key];
}

function tempDir(): string {
  const dir = mkdtempSync(path.join(tmpdir(), "usage-t4-"));
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

const CLAUDE_SESSION = "3bc481ff-60ac-491a-abd0-07c408c85e71";
const CODEX_SESSION = "01a1f843-77fa-7d30-b3c1-19f822a13215";
const CWD = "/repo/proj";

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

function runUsage(args: string[], extraEnv?: Record<string, string>): CliResult {
  const result = spawnSync(process.execPath, [USAGE_CLI, ...args], {
    encoding: "utf8",
    env: baseEnv(extraEnv),
    timeout: 60_000,
  });
  return parseOut(result.stdout, result.stderr, result.status ?? -1);
}

/** Async variant: required while an in-process HTTP server serves pricing. */
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
    const child = spawn(CONV_CLI, args, {
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

// --- fixtures (same shapes as tests/usage.test.ts) -------------------------------

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
  };
  return { convStateDir, usageStateDir, claudeHome, codexHome, convEnv, usageEnv };
}

function writeClaudeTranscript(
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
    `${lines.map((l) => (typeof l === "string" ? l : JSON.stringify(l))).join("\n")}\n`,
  );
  return file;
}

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
  const file = path.join(dir, `rollout-2026-01-02T00-00-00-${CODEX_SESSION}.jsonl`);
  writeFileSync(file, `${lines.map((l) => JSON.stringify(l)).join("\n")}\n`);
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

async function indexFixtures(env: FixtureEnv, herdrSocket: string): Promise<void> {
  const idx = await runConversationsAsync(["index", "--socket", herdrSocket], {
    ...env.convEnv,
    HERDR_SOCKET_PATH: herdrSocket,
  });
  expect(idx.status).toBe(0);
  expect(((idx.json?.indexed ?? []) as unknown[]).length).toBe(2);
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

const T = (name: string, fn: () => void | Promise<void>) => test(name, fn, 60_000);

// --- unit: rate table parsing & lookup -------------------------------------------

describe("pricing rate table", () => {
  const doc = {
    "claude-test-1": {
      input_cost_per_token: 0.000003,
      output_cost_per_token: 0.000015,
      cache_read_input_token_cost: 0.0000003,
      cache_creation_input_token_cost: 0.00000375,
    },
    "gpt-6-test": {
      input_cost_per_token: 0.0000025,
      output_cost_per_token: 0.00001,
      // cache rates omitted → fall back to input rate
    },
    "partial-model": { input_cost_per_token: 0.000001 }, // no output → dropped
    "anthropic.claude-opus-5-5-20251101-v1:0": {
      input_cost_per_token: 0.000005,
      output_cost_per_token: 0.000025,
    },
    "openai/gpt-6.1-sol-2026-09-29": {
      input_cost_per_token: 0.000002,
      output_cost_per_token: 0.000008,
    },
    "gpt-6.1-sol": {
      input_cost_per_token: 0.000002,
      output_cost_per_token: 0.000008,
    },
  };

  T("parseRateTable keeps standard rates, drops partial entries", () => {
    const table = parseRateTable(doc);
    expect(table.has("claude-test-1")).toBe(true);
    expect(table.has("partial-model")).toBe(false);
    const gpt = table.get("gpt-6-test");
    expect(gpt?.cacheReadCostPerToken).toBe(0.0000025); // input fallback
    expect(gpt?.cacheCreationCostPerToken).toBe(0.0000025);
    expect(parseRateTable(null).size).toBe(0);
    expect(parseRateTable("nope").size).toBe(0);
  });

  T("lookupRate resolves exact, bare, and date-stripped names only", () => {
    const table = parseRateTable(doc);
    expect(lookupRate(table, "Claude-Test-1")?.outputCostPerToken).toBe(0.000015);
    // provider prefix dropped
    expect(lookupRate(table, "anthropic/claude-test-1")?.inputCostPerToken).toBe(0.000003);
    // date suffix stripped on the query side → undated alias
    expect(lookupRate(table, "openai/gpt-6.1-sol-2026-10-01")).not.toBeNull();
    expect(lookupRate(table, "gpt-6.1-sol-2026-09-29")).not.toBeNull();
    // unknown → null, never guessed
    expect(lookupRate(table, "never-heard-of-this-9")).toBeNull();
    expect(lookupRate(table, "")).toBeNull();
    expect(lookupRate(table, "opus")).toBeNull();
  });

  T("createOverrideRateTable honors per-million USD and fallbacks", () => {
    const table = createOverrideRateTable({
      "my-model": {
        inputCostPerMillionTokens: 3,
        outputCostPerMillionTokens: 15,
        cacheWriteCostPerMillionTokens: 3.75,
      },
      bad: { inputCostPerMillionTokens: -1, outputCostPerMillionTokens: 2 },
    });
    const rate = table.get("my-model");
    expect(rate?.inputCostPerToken).toBeCloseTo(0.000003, 12);
    expect(rate?.outputCostPerToken).toBeCloseTo(0.000015, 12);
    expect(rate?.cacheReadCostPerToken).toBeCloseTo(0.000003, 12); // input fallback
    expect(rate?.cacheCreationCostPerToken).toBeCloseTo(0.00000375, 12);
    expect(table.has("bad")).toBe(false);
    expect(createOverrideRateTable(undefined).size).toBe(0);
  });

  T("estimateBucketCostUsd prices four buckets, never thinking", () => {
    const rate = parseRateTable(doc).get("claude-test-1");
    expect(rate).toBeDefined();
    if (rate === undefined) throw new Error("fixture rate missing");
    const cost = estimateBucketCostUsd(rate, {
      input_tokens: 10,
      output_tokens: 5,
      cache_read_input_tokens: 3,
      cache_creation_input_tokens: 2,
    });
    // 10*3e-6 + 5*15e-6 + 3*0.3e-6 + 2*3.75e-6
    expect(cost).toBeCloseTo(0.0001134, 10);
  });
});

// --- unit: computeModelCosts -------------------------------------------------------

describe("computeModelCosts", () => {
  const bucket = (key: string | null, over: Partial<BreakdownEntry> = {}): BreakdownEntry => ({
    key,
    events: 1,
    input_tokens: 100,
    output_tokens: 10,
    cache_read_input_tokens: 40,
    cache_creation_input_tokens: 6,
    thinking_tokens: null,
    total_tokens: 156,
    provider_reported_cost_usd: null,
    ...over,
  });

  const pricingTable = async () =>
    collectPricing({
      stateDir: tempDir(),
      env: {},
      fetchImpl: async () =>
        new Response(
          JSON.stringify({
            "priced-model": {
              input_cost_per_token: 0.000001,
              output_cost_per_token: 0.000002,
            },
          }),
        ),
    });

  T("priced models get estimates; unpriced stay null and are listed", async () => {
    const pricing = await pricingTable();
    const result = computeModelCosts(
      [bucket("priced-model"), bucket("mystery-model"), bucket(null)],
      pricing,
    );
    const priced = result.by_model.find((m) => m.key === "priced-model");
    // 100*1e-6 + 10*2e-6 + 40*1e-6 + 6*1e-6 = 0.000166
    expect(priced?.estimated_cost_usd).toBeCloseTo(0.000166, 10);
    expect(priced?.cost_source).toBe("model_priced");
    const unpriced = result.by_model.find((m) => m.key === "mystery-model");
    expect(unpriced?.estimated_cost_usd).toBeNull();
    expect(unpriced?.cost_source).toBe("unpriced");
    expect(result.by_model.find((m) => m.key === null)?.cost_source).toBe("unpriced");
    expect(result.estimate_complete).toBe(false);
    expect(result.priced_models).toBe(1);
    expect(result.unpriced_models.length).toBe(2);
    expect(result.estimated_cost_usd).toBeCloseTo(0.000166, 10);
  });

  T("provider_reported_cost_usd stays separate from the estimate", async () => {
    const pricing = await pricingTable();
    const result = computeModelCosts(
      [bucket("priced-model", { provider_reported_cost_usd: 9.99 })],
      pricing,
    );
    expect(result.by_model[0]?.provider_reported_cost_usd).toBe(9.99);
    expect(result.by_model[0]?.estimated_cost_usd).toBeCloseTo(0.000166, 10);
  });

  T("unavailable table → all unpriced, null total, never zero", async () => {
    const pricing = await collectPricing({
      stateDir: tempDir(),
      env: {},
      fetchImpl: async () => {
        throw new Error("offline");
      },
    });
    expect(pricing.info.status).toBe("unavailable");
    const result = computeModelCosts([bucket("priced-model")], pricing);
    expect(result.estimated_cost_usd).toBeNull();
    expect(result.by_model[0]?.estimated_cost_usd).toBeNull();
    expect(result.by_model[0]?.cost_source).toBe("unpriced");
    expect(result.unpriced_models.length).toBe(1);
  });
});

// --- unit: pricing table load / cache ---------------------------------------------

describe("collectPricing cache", () => {
  const doc = {
    "m-a": { input_cost_per_token: 0.000001, output_cost_per_token: 0.000002 },
  };

  T("fresh fetch writes cache; second call hits cache without refetch", async () => {
    const stateDir = tempDir();
    let calls = 0;
    const fetchImpl = async () => {
      calls += 1;
      return new Response(JSON.stringify(doc));
    };
    const first = await collectPricing({ stateDir, env: {}, fetchImpl });
    expect(first.info.status).toBe("ok");
    expect(first.info.cache.hit).toBe(false);
    expect(first.rates.size).toBe(1);
    expect(calls).toBe(1);

    const second = await collectPricing({ stateDir, env: {}, fetchImpl });
    expect(second.info.status).toBe("ok");
    expect(second.info.cache.hit).toBe(true);
    expect(calls).toBe(1); // no refetch within TTL
  });

  T("fetch failure serves stale cache within TTL, flagged", async () => {
    const stateDir = tempDir();
    await collectPricing({
      stateDir,
      env: {},
      fetchImpl: async () => new Response(JSON.stringify(doc)),
    });
    const stale = await collectPricing({
      stateDir,
      env: {},
      ttlSeconds: 0, // force past fresh TTL but inside retention
      fetchImpl: async () => {
        throw new Error("down");
      },
    });
    expect(stale.info.status).toBe("stale");
    expect(stale.info.detail).toContain("serving cached table");
    expect(stale.rates.size).toBe(1);
  });

  T("cache older than retention is GC'd → unavailable, not served", async () => {
    const stateDir = tempDir();
    // Seed a cache file whose fetched_at is beyond retention.
    const old = new Date(Date.now() - (PRICING_CACHE_RETENTION_SECONDS + 3600) * 1000);
    const dir = path.join(stateDir, "pricing-cache");
    mkdirSync(dir, { recursive: true });
    writeFileSync(
      path.join(dir, "litellm.json"),
      JSON.stringify({
        fetched_at: old.toISOString(),
        source: "test",
        rates: { "m-a": [0.000001, 0.000002, 0.000001, 0.000001] },
      }),
    );
    const result = await collectPricing({
      stateDir,
      env: {},
      fetchImpl: async () => {
        throw new Error("down");
      },
    });
    expect(result.info.status).toBe("unavailable");
    // GC removed the expired file.
    const { existsSync } = await import("node:fs");
    expect(existsSync(path.join(dir, "litellm.json"))).toBe(false);
  });
});

// --- unit: GC / retention ------------------------------------------------------------

describe("plugin state GC", () => {
  function writeCache(stateDir: string, rel: string, payload: unknown): string {
    const file = path.join(stateDir, rel);
    mkdirSync(path.dirname(file), { recursive: true });
    writeFileSync(file, JSON.stringify(payload));
    return file;
  }

  T("keeps referenced entries, removes expired + tmp + corrupt", async () => {
    const stateDir = tempDir();
    const now = Date.now();
    const fresh = new Date(now - 3600_000).toISOString();
    const expired = new Date(now - (PLUGIN_CACHE_RETENTION_SECONDS + 3600) * 1000).toISOString();

    const keepQuota = writeCache(stateDir, "quota-cache/claude.json", {
      checked_at: fresh,
      windows: [],
    });
    const keepHub = writeCache(stateDir, "quota-cache/hub-home.json", {
      checked_at: fresh,
      accounts: [],
    });
    const keepPricing = writeCache(stateDir, "pricing-cache/litellm.json", {
      fetched_at: fresh,
      rates: {},
    });
    const dropQuota = writeCache(stateDir, "quota-cache/codex.json", {
      checked_at: expired,
      windows: [],
    });
    const dropHub = writeCache(stateDir, "quota-cache/hub-old.json", {
      checked_at: expired,
      accounts: [],
    });
    const dropPricing = writeCache(stateDir, "pricing-cache/litellm-old.json", {
      fetched_at: expired,
      rates: {},
    });
    const tmp = writeCache(stateDir, "quota-cache/claude.json.tmp-123", { checked_at: fresh });
    const corrupt = path.join(stateDir, "quota-cache", "broken.json");
    writeFileSync(corrupt, "{not json");

    const result = await gcPluginStateCache(stateDir);
    const { existsSync } = await import("node:fs");
    for (const kept of [keepQuota, keepHub, keepPricing]) {
      expect(existsSync(kept)).toBe(true);
    }
    for (const dropped of [dropQuota, dropHub, dropPricing, tmp, corrupt]) {
      expect(existsSync(dropped)).toBe(false);
    }
    expect(result.removed.length).toBe(5);
  });

  T("entry just inside retention is kept (boundary is exclusive)", async () => {
    const stateDir = tempDir();
    // A few seconds inside the window — the exact boundary cannot be
    // hit deterministically across the GC call's own clock read.
    const edge = new Date(Date.now() - (PLUGIN_CACHE_RETENTION_SECONDS - 60) * 1000).toISOString();
    const file = writeCache(stateDir, "quota-cache/claude.json", {
      checked_at: edge,
      windows: [],
    });
    await gcPluginStateCache(stateDir);
    const { existsSync } = await import("node:fs");
    expect(existsSync(file)).toBe(true);
  });

  T("collectQuota runs GC: expired cache removed even without credential", async () => {
    const stateDir = tempDir();
    const expired = new Date(Date.now() - (PLUGIN_CACHE_RETENTION_SECONDS + 60) * 1000);
    const file = writeCache(stateDir, "quota-cache/claude.json", {
      checked_at: expired.toISOString(),
      windows: [],
    });
    // Empty CLAUDE_CONFIG_DIR → no_credential, but GC still ran.
    const results = await collectQuota({
      stateDir,
      env: { CLAUDE_CONFIG_DIR: tempDir(), HOME: tempDir() },
      providers: ["claude"],
    });
    expect(results[0]?.status).toBe("no_credential");
    const { existsSync } = await import("node:fs");
    expect(existsSync(file)).toBe(false);
  });
});

// --- source resolution ------------------------------------------------------------

describe("conversations source resolution", () => {
  T("pluginRoot resolves plugins/usage so sibling discovery finds plugins/conversations", () => {
    const saved = process.env.HERDR_PLUGIN_ROOT;
    delete process.env.HERDR_PLUGIN_ROOT;
    try {
      // Regression: pluginRoot() must be the plugin dir itself, not its
      // parent — otherwise `../conversations` escapes plugins/ entirely
      // and `bun src/cli.ts` (no HERDR_PLUGIN_ROOT) cannot auto-discover
      // the sibling plugin.
      expect(pluginRoot()).toBe(path.join(__dirname, "..", "plugins", "usage"));
    } finally {
      if (saved !== undefined) process.env.HERDR_PLUGIN_ROOT = saved;
    }
  });
});

// --- contract: cost command ---------------------------------------------------------

describe("cost command", () => {
  T("capability lists cost + model-pricing source", () => {
    const { usageEnv } = setupEnv();
    const r = runUsage(["capability"], usageEnv);
    expect(r.status).toBe(0);
    expect(((r.json?.commands ?? []) as string[]).includes("cost")).toBe(true);
    const sources = r.json?.sources as Record<string, unknown>[];
    const pricing = sources.find((s) => s.id === "model-pricing");
    expect(pricing?.kind).toBe("price-table");
  });

  T("end-to-end: estimates per model, unpriced listed, estimate labeled", async () => {
    const env = setupEnv();
    writeClaudeTranscript(env.claudeHome, CWD, CLAUDE_SESSION, claudeFixture());
    writeCodexHome(env.codexHome);
    const herdr = await fakeHerdr();
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
      await indexFixtures(env, herdr.socketPath);
      const r = await runUsageCliAsync(["cost", "--offline"], {
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

      // claude-test-1: in 30, out 12, cr 3, cc 2 → 0.0002784
      const byModel = cost.by_model as Record<string, unknown>[];
      const claude = byModel.find((m) => m.key === "claude-test-1");
      expect(claude?.cost_source).toBe("model_priced");
      expect(claude?.estimated_cost_usd as number).toBeCloseTo(0.0002784, 10);
      // provider-reported cost stays a separate field (0.0123 from fixture)
      expect(claude?.provider_reported_cost_usd as number).toBeCloseTo(0.0123, 6);

      const codex = byModel.find((m) => m.key === "gpt-6-test");
      expect(codex?.cost_source).toBe("unpriced");
      expect(codex?.estimated_cost_usd).toBeNull();

      expect(cost.priced_models).toBe(1);
      expect((cost.unpriced_models as unknown[]).length).toBe(1);
      expect(cost.estimate_complete).toBe(false);
      expect(cost.estimated_cost_usd as number).toBeCloseTo(0.0002784, 10);
      // estimate and provider-reported are different figures, both present
      expect(cost.provider_reported_cost_usd as number).toBeCloseTo(0.0123, 6);
      expect(((r.json?.warnings ?? []) as string[]).includes("unpriced_models")).toBe(true);
    } finally {
      await herdr.close();
      await server.close();
    }
  });

  T("pricing source down → pricing_unavailable, all unpriced, exit 0", async () => {
    const env = setupEnv();
    writeClaudeTranscript(env.claudeHome, CWD, CLAUDE_SESSION, claudeFixture());
    writeCodexHome(env.codexHome);
    const herdr = await fakeHerdr();
    const server = await pricingServer("nope", 500);
    try {
      await indexFixtures(env, herdr.socketPath);
      const r = await runUsageCliAsync(["cost", "--offline"], {
        ...env.usageEnv,
        HERDR_USAGE_PRICING_URL: server.url,
      });
      expect(r.status).toBe(0);
      const cost = r.json?.cost as Record<string, unknown>;
      expect((cost.pricing as Record<string, unknown>).status).toBe("unavailable");
      expect(cost.estimated_cost_usd).toBeNull();
      expect(cost.estimate).toBe(true); // still labeled an estimate attempt
      const byModel = cost.by_model as Record<string, unknown>[];
      expect(byModel.every((m) => m.cost_source === "unpriced")).toBe(true);
      expect(((r.json?.warnings ?? []) as string[]).includes("pricing_unavailable")).toBe(true);
    } finally {
      await herdr.close();
      await server.close();
    }
  });

  T("config modelPrices override prices an unlisted model", async () => {
    const env = setupEnv();
    writeClaudeTranscript(env.claudeHome, CWD, CLAUDE_SESSION, claudeFixture());
    writeCodexHome(env.codexHome);
    const herdr = await fakeHerdr();
    const server = await pricingServer({}); // empty table
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
      await indexFixtures(env, herdr.socketPath);
      const r = await runUsageCliAsync(["cost", "--offline"], {
        ...env.usageEnv,
        HERDR_USAGE_PRICING_URL: server.url,
      });
      expect(r.status).toBe(0);
      const cost = r.json?.cost as Record<string, unknown>;
      const pricing = cost.pricing as Record<string, unknown>;
      expect(pricing.user_overrides).toBe(1);
      const byModel = cost.by_model as Record<string, unknown>[];
      const codex = byModel.find((m) => m.key === "gpt-6-test");
      // 100*2.5e-6 + 10*1e-5 + 40*2.5e-6 + 6*2.5e-6 = 0.000465
      expect(codex?.cost_source).toBe("model_priced");
      expect(codex?.estimated_cost_usd as number).toBeCloseTo(0.000465, 10);
      // claude-test-1 still unpriced → partial estimate disclosed
      expect(cost.estimate_complete).toBe(false);
      expect(
        (cost.unpriced_models as { key: string }[]).some((m) => m.key === "claude-test-1"),
      ).toBe(true);
    } finally {
      await herdr.close();
      await server.close();
    }
  });
});
