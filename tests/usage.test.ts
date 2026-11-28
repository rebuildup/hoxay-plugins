import { afterAll, describe, expect, test } from "bun:test";
import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { aggregateUsage } from "../plugins/usage/src/aggregate";
import type { SessionCoverageEntry, UsageEventRecord } from "../plugins/usage/src/model";
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
  "CLAUDE_CONFIG_DIR",
  "CODEX_HOME",
  "OPENCODE_DATA_DIR",
  "XDG_DATA_HOME",
]) {
  savedEnv[key] = process.env[key];
}

function tempDir(): string {
  const dir = mkdtempSync(path.join(tmpdir(), "usage-t1-"));
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
const DEAD_SOCKET = path.join(tempDir(), "no-such-herdr.sock");

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
  return testEnv({ HERDR_SOCKET_PATH: DEAD_SOCKET, ...extra });
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

/** Async variant: required while a same-process fake herdr socket serves. */
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

// --- fixtures -----------------------------------------------------------------

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
    lines.map((l) => (typeof l === "string" ? l : JSON.stringify(l))).join("\n") + "\n",
  );
  return file;
}

function claudeUser(uuid: string, text: string, second: number): unknown {
  return {
    type: "user",
    uuid,
    timestamp: `2026-01-01T00:00:${String(second).padStart(2, "0")}.000Z`,
    message: { role: "user", content: text },
  };
}

function claudeAssistant(
  uuid: string,
  second: number,
  usage: Record<string, unknown>,
  costUsd?: number,
): unknown {
  return {
    type: "assistant",
    uuid,
    timestamp: `2026-01-01T00:00:${String(second).padStart(2, "0")}.000Z`,
    ...(costUsd !== undefined ? { costUSD: costUsd } : {}),
    message: {
      id: `msg-${uuid}`,
      role: "assistant",
      model: "claude-test-1",
      content: [{ type: "text", text: "ok" }],
      usage,
    },
  };
}

/** Claude fixture: two usage events incl. cache tokens, thinking, cost. */
function claudeFixture(): unknown[] {
  return [
    claudeUser("u-1", "hi", 1),
    claudeAssistant(
      "a-1",
      2,
      {
        input_tokens: 10,
        output_tokens: 5,
        cache_read_input_tokens: 3,
        cache_creation_input_tokens: 2,
        output_tokens_details: { thinking_tokens: 1 },
      },
      0.0123,
    ),
    claudeUser("u-2", "again", 3),
    claudeAssistant("a-2", 4, {
      input_tokens: 20,
      output_tokens: 7,
      cache_read_input_tokens: 0,
      cache_creation_input_tokens: 0,
    }),
  ];
}

function codexFixtureLines(): unknown[] {
  const ts = (s: number) => `2026-01-02T00:00:${String(s).padStart(2, "0")}.000Z`;
  return [
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
}

function writeCodexHome(codexHome: string): void {
  const dir = path.join(codexHome, "sessions", "2026", "01", "02");
  mkdirSync(dir, { recursive: true });
  const file = path.join(dir, `rollout-2026-01-02T00-00-00-${CODEX_SESSION}.jsonl`);
  writeFileSync(
    file,
    codexFixtureLines()
      .map((l) => (typeof l === "string" ? l : JSON.stringify(l)))
      .join("\n") + "\n",
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

/** Index both fixtures through the real conversations plugin. */
async function indexFixtures(env: FixtureEnv, herdrSocket: string): Promise<void> {
  const idx = await runConversationsAsync(["index", "--socket", herdrSocket], {
    ...env.convEnv,
    HERDR_SOCKET_PATH: herdrSocket,
  });
  expect(idx.status).toBe(0);
  expect(((idx.json?.indexed ?? []) as unknown[]).length).toBe(2);
}

const t = (name: string, fn: () => void | Promise<void>) => test(name, fn, 60_000);

// --- unit: aggregate core ------------------------------------------------------

describe("usage aggregate core", () => {
  const sessions: SessionCoverageEntry[] = [
    { key: "claude:s1", provider: "claude", cwd: "/repo/a", status: "ok", events_present: true },
    { key: "codex:s2", provider: "codex", cwd: "/repo/b", status: "ok", events_present: true },
    { key: "claude:s3", provider: "claude", cwd: null, status: "missing", events_present: false },
  ];

  function ev(
    sessionKey: string,
    eventId: string,
    usage: Record<string, unknown>,
    extra: Record<string, unknown> = {},
  ): UsageEventRecord {
    return {
      session_key: sessionKey,
      event: {
        event_id: eventId,
        type: "assistant_message",
        timestamp: "2026-01-01T00:00:00.000Z",
        model: "m1",
        usage,
        identity: { host: "h1", pane_id: "p1" },
        ...extra,
      },
    };
  }

  t("sums tokens, dedups by (session_key,event_id), breaks down dimensions", () => {
    const result = aggregateUsage({
      sessions,
      window: { from_ms: null, to_ms: null },
      events: [
        ev("claude:s1", "e1", { input_tokens: 10, output_tokens: 5, cache_read_input_tokens: 3 }),
        ev("claude:s1", "e1", { input_tokens: 10, output_tokens: 5 }), // duplicate
        ev("claude:s1", "e2", {
          input_tokens: 1,
          output_tokens: 1,
          cache_creation_input_tokens: 2,
          thinking_tokens: 4,
        }),
        ev("codex:s2", "e9", { input_tokens: 100, output_tokens: 10 }, { model: "gpt-x" }),
      ],
    });
    expect(result.totals.events).toBe(3);
    expect(result.totals.duplicate_events_skipped).toBe(1);
    expect(result.totals.input_tokens).toBe(111);
    expect(result.totals.output_tokens).toBe(16);
    expect(result.totals.cache_read_input_tokens).toBe(3);
    expect(result.totals.cache_creation_input_tokens).toBe(2);
    expect(result.totals.thinking_tokens).toBe(4);
    expect(result.totals.provider_reported_cost_usd).toBeNull();
    expect(result.breakdowns.by_model.map((b) => b.key).sort()).toEqual(["gpt-x", "m1"]);
    expect(result.breakdowns.by_project.map((b) => b.key).sort()).toEqual(["/repo/a", "/repo/b"]);
    expect(result.coverage.sessions_total).toBe(3);
    expect(result.coverage.sessions_with_issues).toBe(1);
    expect(result.coverage.providers.find((p) => p.provider === "claude")?.sessions).toBe(2);
  });

  t("window excludes out-of-range and untimestamped events honestly", () => {
    const result = aggregateUsage({
      sessions,
      window: {
        from_ms: Date.parse("2026-01-01T00:00:00Z"),
        to_ms: Date.parse("2026-01-01T01:00:00Z"),
      },
      events: [
        ev("claude:s1", "e1", { input_tokens: 10, output_tokens: 5 }),
        ev(
          "claude:s1",
          "e2",
          { input_tokens: 99, output_tokens: 99 },
          {
            timestamp: "2026-06-01T00:00:00.000Z",
          },
        ),
        ev("claude:s1", "e3", { input_tokens: 50, output_tokens: 50 }, { timestamp: null }),
      ],
    });
    expect(result.totals.events).toBe(1);
    expect(result.totals.input_tokens).toBe(10);
    expect(result.totals.excluded_untimestamped_events).toBe(1);

    const open = aggregateUsage({
      sessions,
      window: { from_ms: null, to_ms: null },
      events: [ev("claude:s1", "e3", { input_tokens: 50, output_tokens: 50 }, { timestamp: null })],
    });
    expect(open.totals.events).toBe(1);
    expect(open.totals.excluded_untimestamped_events).toBe(0);
  });

  t("missing usage fields stay null, never zero-fabricated", () => {
    const result = aggregateUsage({
      sessions,
      window: { from_ms: null, to_ms: null },
      events: [
        ev("claude:s1", "e1", { input_tokens: 5, output_tokens: 5 }),
        // event without usage object is dropped by the defensive filter
        { session_key: "claude:s1", event: { event_id: "e2", usage: null } },
        // malformed usage (all zeros) also dropped
        ev("claude:s1", "e3", { input_tokens: 0, output_tokens: 0 }),
      ],
    });
    expect(result.totals.events).toBe(1);
    expect(result.totals.thinking_tokens).toBeNull();
    expect(result.totals.provider_reported_cost_usd).toBeNull();
    expect(result.totals.events_without_event_id).toBe(0);
  });
});

// --- contract: CLI ---------------------------------------------------------------

describe("usage plugin contract", () => {
  t("capability reports read-only contract + source matrix", () => {
    const { usageEnv } = setupEnv();
    const r = runUsage(["capability"], usageEnv);
    expect(r.status).toBe(0);
    expect(r.json?.contract).toBe("herdr-plugins.usage/v1");
    expect(r.json?.read_only).toBe(true);
    const sources = r.json?.sources as Record<string, unknown>[];
    const index = sources.find((s) => s.id === "conversations-index");
    expect(index?.status).toBe("available");
    expect(index?.contract).toBe("herdr-plugins.conversations/v1");
    // T3 (#92): the HTTP quota hub slot is implemented, not a placeholder.
    const hub = sources.find((s) => s.id === "cliproxy-api");
    expect(hub?.status).toBe("available");
    expect(hub?.credential_boundary).toContain("0015");
    expect(((r.json?.commands ?? []) as string[]).includes("divergence")).toBe(true);
  });

  t("aggregates claude+codex fixtures end-to-end with cache tokens", async () => {
    const env = setupEnv();
    writeClaudeTranscript(env.claudeHome, CWD, CLAUDE_SESSION, claudeFixture());
    writeCodexHome(env.codexHome);
    const herdr = await fakeHerdr();
    try {
      await indexFixtures(env, herdr.socketPath);

      const r = runUsage(["usage", "--offline"], env.usageEnv);
      expect(r.status).toBe(0);
      expect(r.json?.contract).toBe("herdr-plugins.usage/v1");
      const totals = r.json?.totals as Record<string, unknown>;
      expect(totals.events).toBe(3); // 2 claude + 1 codex usage events
      // claude: 10+20 in, 5+7 out, 3+0 cache read, 2+0 cache create
      // codex: 100 in, 10 out, 40 cache read, 6 cache create
      expect(totals.input_tokens).toBe(130);
      expect(totals.output_tokens).toBe(22);
      expect(totals.cache_read_input_tokens).toBe(43);
      expect(totals.cache_creation_input_tokens).toBe(8);
      expect(totals.thinking_tokens).toBe(6); // claude 1 + codex reasoning 5
      expect(totals.provider_reported_cost_usd).toBeCloseTo(0.0123, 6);
      expect(totals.total_tokens).toBe(203);

      interface Bucket {
        key: string | null;
        events: number;
        input_tokens: number;
      }
      const breakdowns = r.json?.breakdowns as Record<string, Bucket[]>;
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
    } finally {
      await herdr.close();
    }
  });

  t("selectors and time window filter the aggregation", async () => {
    const env = setupEnv();
    writeClaudeTranscript(env.claudeHome, CWD, CLAUDE_SESSION, claudeFixture());
    writeCodexHome(env.codexHome);
    const herdr = await fakeHerdr();
    try {
      await indexFixtures(env, herdr.socketPath);

      const byProvider = runUsage(["usage", "--provider", "claude", "--offline"], env.usageEnv);
      expect(((byProvider.json?.totals ?? {}) as Record<string, unknown>).events).toBe(2);
      expect(((byProvider.json?.totals ?? {}) as Record<string, unknown>).input_tokens).toBe(30);

      const byPane = runUsage(["usage", "--pane", "w1:p2", "--offline"], env.usageEnv);
      expect(((byPane.json?.totals ?? {}) as Record<string, unknown>).input_tokens).toBe(100);

      // claude events are 2026-01-01, codex 2026-01-02
      const windowed = runUsage(
        ["usage", "--from", "2026-01-02T00:00:00Z", "--to", "2026-01-03T00:00:00Z", "--offline"],
        env.usageEnv,
      );
      const wt = windowed.json?.totals as Record<string, unknown>;
      expect(wt.events).toBe(1);
      expect(wt.input_tokens).toBe(100);

      const badFrom = runUsage(["usage", "--from", "not-a-date", "--offline"], env.usageEnv);
      expect(badFrom.status).toBe(2);
      const inverted = runUsage(
        ["usage", "--from", "2026-02-01T00:00:00Z", "--to", "2026-01-01T00:00:00Z", "--offline"],
        env.usageEnv,
      );
      expect(inverted.status).toBe(2);
    } finally {
      await herdr.close();
    }
  });

  t("empty index and missing source are explicit, never fabricated", async () => {
    const env = setupEnv();
    // nothing indexed → honest empty aggregate + hint
    const empty = runUsage(["usage", "--offline"], env.usageEnv);
    expect(empty.status).toBe(0);
    expect(((empty.json?.totals ?? {}) as Record<string, unknown>).events).toBe(0);
    expect(empty.json?.hint).toContain("index");
    expect(((empty.json?.coverage ?? {}) as Record<string, unknown>).sessions_total).toBe(0);

    // unresolvable conversations root → structured error
    const missing = runUsage(
      ["usage", "--offline", "--conversations-root", path.join(tempDir(), "nope")],
      env.usageEnv,
    );
    expect(missing.status).toBe(1);
    expect(missing.error?.kind).toBe("conversations_unavailable");
  });
});
