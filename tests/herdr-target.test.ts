import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import {
  ambientIdentity,
  assertAmbientPaneIdentity,
  defaultSocketPath,
  resolveTarget,
  sessionSocketPath,
  socketPathForSession,
  type TargetErrorCode,
  TargetResolutionError,
} from "../packages/herdr-client/src/index";
import { gitBinary } from "./helpers/git-bin";
import { inboxBinary } from "./helpers/inbox-bin";
import { herdrSocketEndpoint, testEnv } from "./helpers/test-env";

// ---------------------------------------------------------------------------
// Env helpers: resolveTarget/ambientIdentity read process.env, so each test
// patches and restores the HERDR_*/XDG_* surface explicitly.
// ---------------------------------------------------------------------------

const MANAGED_ENV_KEYS = [
  "HERDR_SOCKET_PATH",
  "HERDR_SESSION",
  "HERDR_PANE_ID",
  "HERDR_WORKSPACE_ID",
  "HERDR_TAB_ID",
  "HERDR_PLUGIN_CONTEXT_JSON",
  "HERDR_PLUGIN_CONFIG_DIR",
  "HERDR_PLUGIN_STATE_DIR",
  "XDG_CONFIG_HOME",
  "XDG_STATE_HOME",
] as const;

const savedEnv = new Map<string, string | undefined>();
for (const key of MANAGED_ENV_KEYS) savedEnv.set(key, process.env[key]);

function setEnv(patch: Record<string, string | undefined>): void {
  for (const key of MANAGED_ENV_KEYS) {
    if (!(key in patch)) delete process.env[key];
  }
  for (const [key, value] of Object.entries(patch)) {
    if (value === undefined) delete process.env[key];
    else process.env[key] = value;
  }
}

afterAll(() => {
  for (const [key, value] of savedEnv) {
    if (value === undefined) delete process.env[key];
    else process.env[key] = value;
  }
});

const CFG = "/tmp/herdr-target-test-config";
// sessionSocketPath = $XDG_CONFIG_HOME/herdr/sessions/<name>/herdr.sock
const sockA = path.join(CFG, "herdr", "sessions", "aaa", "herdr.sock");
const sockB = path.join(CFG, "herdr", "sessions", "bbb", "herdr.sock");
const customSock = "/tmp/herdr-target-test-custom.sock";

function expectTargetError(fn: () => unknown, code: TargetErrorCode): TargetResolutionError {
  try {
    fn();
  } catch (err) {
    expect(err).toBeInstanceOf(TargetResolutionError);
    expect((err as TargetResolutionError).code).toBe(code);
    return err as TargetResolutionError;
  }
  throw new Error(`expected TargetResolutionError(${code})`);
}

describe("resolveTarget precedence", () => {
  test("explicit --session resolves the named session socket", () => {
    setEnv({ XDG_CONFIG_HOME: CFG });
    const target = resolveTarget({ session: "bbb" });
    expect(target.socketPath).toBe(sessionSocketPath("bbb"));
    expect(target.source).toBe("flag");
    expect(target.session).toBe("bbb");
  });

  test("--session default maps to the default session socket", () => {
    setEnv({ XDG_CONFIG_HOME: CFG });
    expect(resolveTarget({ session: "default" }).socketPath).toBe(defaultSocketPath());
    expect(socketPathForSession("default")).toBe(defaultSocketPath());
  });

  test("explicit --socket wins over consistent ambient env", () => {
    setEnv({ XDG_CONFIG_HOME: CFG, HERDR_SOCKET_PATH: sockB, HERDR_SESSION: "bbb" });
    const target = resolveTarget({ socketPath: sockB });
    expect(target.socketPath).toBe(sockB);
    expect(target.source).toBe("flag");
    expect(target.session).toBe("bbb");
  });

  test("ambient HERDR_SOCKET_PATH beats ambient HERDR_SESSION (upstream order)", () => {
    setEnv({ XDG_CONFIG_HOME: CFG, HERDR_SOCKET_PATH: customSock });
    const target = resolveTarget({});
    expect(target.socketPath).toBe(customSock);
    expect(target.source).toBe("env");
    expect(target.session).toBeNull();
  });

  test("ambient HERDR_SESSION alone resolves the session socket", () => {
    setEnv({ XDG_CONFIG_HOME: CFG, HERDR_SESSION: "aaa" });
    const target = resolveTarget({});
    expect(target.socketPath).toBe(sessionSocketPath("aaa"));
    expect(target.source).toBe("env");
    expect(target.session).toBe("aaa");
  });

  test("no target falls back to the default socket for reads", () => {
    setEnv({ XDG_CONFIG_HOME: CFG });
    const target = resolveTarget({});
    expect(target.socketPath).toBe(defaultSocketPath());
    expect(target.source).toBe("default");
  });
});

describe("resolveTarget fail-closed contradictions", () => {
  test("--session B vs ambient HERDR_SOCKET_PATH pointing at A", () => {
    setEnv({ XDG_CONFIG_HOME: CFG, HERDR_SOCKET_PATH: sockA, HERDR_SESSION: "aaa" });
    const err = expectTargetError(() => resolveTarget({ session: "bbb" }), "target_conflict");
    expect(err.usage).toBe(false);
  });

  test("--session B vs ambient HERDR_SESSION A", () => {
    setEnv({ XDG_CONFIG_HOME: CFG, HERDR_SESSION: "aaa" });
    expectTargetError(() => resolveTarget({ session: "bbb" }), "target_conflict");
  });

  test("--socket custom vs ambient HERDR_SOCKET_PATH elsewhere", () => {
    setEnv({ HERDR_SOCKET_PATH: sockA });
    expectTargetError(() => resolveTarget({ socketPath: customSock }), "target_conflict");
  });

  test("--socket vs ambient HERDR_SESSION naming a different session", () => {
    setEnv({ XDG_CONFIG_HOME: CFG, HERDR_SESSION: "aaa" });
    expectTargetError(() => resolveTarget({ socketPath: sockB }), "target_conflict");
  });

  test("--socket and --session that disagree", () => {
    setEnv({ XDG_CONFIG_HOME: CFG });
    expectTargetError(
      () => resolveTarget({ socketPath: customSock, session: "bbb" }),
      "target_conflict",
    );
  });

  test("--socket and --session that agree are accepted", () => {
    setEnv({ XDG_CONFIG_HOME: CFG });
    const target = resolveTarget({ socketPath: sockB, session: "bbb" });
    expect(target.socketPath).toBe(sockB);
  });

  test("consistent ambient pair (socket == session socket) resolves", () => {
    setEnv({ XDG_CONFIG_HOME: CFG, HERDR_SOCKET_PATH: sockA, HERDR_SESSION: "aaa" });
    const target = resolveTarget({});
    expect(target.socketPath).toBe(sockA);
    expect(target.session).toBe("aaa");
  });

  test("self-contradictory ambient pair fails closed", () => {
    setEnv({ XDG_CONFIG_HOME: CFG, HERDR_SOCKET_PATH: customSock, HERDR_SESSION: "aaa" });
    expectTargetError(() => resolveTarget({}), "target_conflict");
  });

  test("consistent explicit flag + ambient env resolves as flag", () => {
    setEnv({ XDG_CONFIG_HOME: CFG, HERDR_SOCKET_PATH: sockB });
    const target = resolveTarget({ session: "bbb" });
    expect(target.source).toBe("flag");
    expect(target.socketPath).toBe(sockB);
  });
});

describe("resolveTarget mutation gating", () => {
  test("mutation without any target fails closed", () => {
    setEnv({ XDG_CONFIG_HOME: CFG });
    const err = expectTargetError(() => resolveTarget({ mutation: true }), "target_required");
    expect(err.usage).toBe(false);
  });

  test("mutation with explicit --session is allowed", () => {
    setEnv({ XDG_CONFIG_HOME: CFG });
    expect(resolveTarget({ session: "bbb", mutation: true }).socketPath).toBe(sockB);
  });

  test("mutation with consistent ambient env is allowed", () => {
    setEnv({ HERDR_SOCKET_PATH: customSock });
    expect(resolveTarget({ mutation: true }).socketPath).toBe(customSock);
  });

  test("mutation with explicit HERDR_SESSION=default is allowed", () => {
    setEnv({ XDG_CONFIG_HOME: CFG, HERDR_SESSION: "default" });
    expect(resolveTarget({ mutation: true }).socketPath).toBe(defaultSocketPath());
  });
});

describe("session name validation", () => {
  test("invalid explicit --session is a usage error", () => {
    setEnv({ XDG_CONFIG_HOME: CFG });
    const err = expectTargetError(() => resolveTarget({ session: "../escape" }), "invalid_session");
    expect(err.usage).toBe(true);
  });

  test("invalid ambient HERDR_SESSION fails closed", () => {
    setEnv({ HERDR_SESSION: "bad name!" });
    expectTargetError(() => resolveTarget({}), "invalid_session");
  });
});

describe("ambientIdentity", () => {
  test("env vars and context JSON merge; env wins per field", () => {
    setEnv({
      HERDR_PANE_ID: "w1:p1",
      HERDR_WORKSPACE_ID: "w1",
      HERDR_PLUGIN_CONTEXT_JSON: JSON.stringify({
        focused_pane_id: "w1:p1",
        workspace_id: "w9",
        tab_id: "t1",
      }),
    });
    const identity = ambientIdentity();
    expect(identity.paneId).toBe("w1:p1");
    expect(identity.workspaceId).toBe("w1"); // env beats context
    expect(identity.tabId).toBe("t1"); // context fallback
  });

  test("conflicting pane ids between env and context fail closed", () => {
    setEnv({
      HERDR_PANE_ID: "w1:p1",
      HERDR_PLUGIN_CONTEXT_JSON: JSON.stringify({ focused_pane_id: "w2:p2" }),
    });
    expectTargetError(() => ambientIdentity(), "ambient_identity_mismatch");
  });

  test("no ambient identity yields empty object", () => {
    setEnv({});
    expect(ambientIdentity()).toEqual({
      paneId: undefined,
      workspaceId: undefined,
      tabId: undefined,
    });
  });
});

describe("assertAmbientPaneIdentity", () => {
  const pane = { pane_id: "w1:p1", workspace_id: "w1", tab_id: "t1" };

  test("matching workspace/tab passes", () => {
    assertAmbientPaneIdentity(pane, { workspaceId: "w1", tabId: "t1" });
  });

  test("mismatched workspace fails closed", () => {
    expectTargetError(
      () => assertAmbientPaneIdentity(pane, { workspaceId: "w2" }),
      "ambient_identity_mismatch",
    );
  });

  test("missing pane workspace_id cannot be verified and fails closed", () => {
    expectTargetError(
      () => assertAmbientPaneIdentity({ pane_id: "w1:p1" }, { workspaceId: "w1" }),
      "ambient_identity_mismatch",
    );
  });
});

// ---------------------------------------------------------------------------
// CLI-level negative test: two fake herdr sessions on isolated sockets with
// same-named pane ids. Proves ambient-A + explicit-B produces zero side
// effects on A (issue #83 acceptance, socket-level equivalent of the
// two-real-session verification).
// ---------------------------------------------------------------------------

const IS_WINDOWS = process.platform === "win32";

interface FakeHerdr {
  socketPath: string;
  requests: Array<{ method: string; params: Record<string, unknown> }>;
  close(): Promise<void>;
}

async function startFakeHerdr(
  socketPath: string,
  snapshot: Record<string, unknown>,
): Promise<FakeHerdr> {
  mkdirSync(path.dirname(socketPath), { recursive: true });
  const requests: FakeHerdr["requests"] = [];
  const server = net.createServer((conn) => {
    let buffer = "";
    conn.on("data", (chunk: Buffer) => {
      buffer += chunk.toString("utf8");
      let newline = buffer.indexOf("\n");
      while (newline !== -1) {
        const line = buffer.slice(0, newline).trim();
        buffer = buffer.slice(newline + 1);
        newline = buffer.indexOf("\n");
        if (!line) continue;
        const message = JSON.parse(line) as {
          id: string;
          method: string;
          params: Record<string, unknown>;
        };
        requests.push({ method: message.method, params: message.params });
        const result =
          message.method === "session.snapshot"
            ? { type: "session_snapshot", snapshot }
            : { type: "ok" };
        conn.write(`${JSON.stringify({ id: message.id, result })}\n`);
      }
    });
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(herdrSocketEndpoint(socketPath), () => resolve());
  });
  return {
    socketPath,
    requests,
    close: () =>
      new Promise<void>((resolve) => {
        server.close(() => resolve());
      }),
  };
}

const REPO_ROOT = IS_WINDOWS ? "C:\\repo" : "/repo";

function sessionSnapshot(marker: string): Record<string, unknown> {
  return {
    session: { name: marker },
    workspaces: [{ workspace_id: "w1", title: marker }],
    tabs: [{ tab_id: "t1", workspace_id: "w1" }],
    panes: [
      {
        pane_id: "w1:p1",
        workspace_id: "w1",
        tab_id: "t1",
        cwd: `${REPO_ROOT}-${marker}`,
        title: marker,
      },
    ],
    agents: [
      {
        pane_id: "w1:p1",
        agent: "claude",
        agent_status: "idle",
        state_change_seq: 1,
        cwd: `${REPO_ROOT}-${marker}`,
      },
    ],
  };
}

interface CliRun {
  status: number;
  stdout: string;
  stderr: string;
  errorCode: string | undefined;
}

async function runPluginCli(
  pluginCli: string,
  args: string[],
  env: Record<string, string>,
): Promise<CliRun> {
  // Scrubbed spawn env (#199): ambient HERDR_*/XDG_* never reaches the
  // child; the explicit `env` argument is the only opt-in channel.
  // pluginDirs:"derive" — these tests assert the XDG-derived state path.
  const childEnv = testEnv(env, { pluginDirs: "derive" });
  // Async spawn: the fake herdr servers live in this same process, so a
  // blocking spawnSync would starve them and every request would time out.
  // TypeScript plugin entrypoints run under `bun run`; native binaries
  // (plugins/inbox/bin/inbox, #291) are spawned directly.
  const isTs = pluginCli.endsWith(".ts");
  const spawnArgs = isTs ? ["run", pluginCli, ...args] : args;
  const command = isTs ? process.execPath : pluginCli;
  const result = await new Promise<{
    status: number | null;
    stdout: string;
    stderr: string;
    error?: Error;
  }>((resolve) => {
    const child = spawn(command, spawnArgs, {
      env: childEnv,
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString("utf8");
    });
    child.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
    });
    const timer = setTimeout(() => child.kill("SIGKILL"), 30_000);
    child.on("error", (error) => {
      clearTimeout(timer);
      resolve({ status: null, stdout, stderr, error });
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      resolve({ status: code, stdout, stderr });
    });
  });
  let errorCode: string | undefined;
  try {
    errorCode = (JSON.parse(result.stderr) as { error?: { code?: string } }).error?.code;
  } catch {
    // non-JSON stderr; assertions below still apply
  }
  return {
    status: result.status ?? -1,
    stdout: result.stdout,
    stderr: result.stderr,
    errorCode,
  };
}

describe("CLI target isolation (fake herdr sessions)", () => {
  // macOS caps AF_UNIX paths at 104 chars; $TMPDIR is already too deep, so
  // anchor under /tmp directly.
  // NB: bun:test evaluates this callback even when skipIf skips the suite,
  // so every filesystem touch is deferred into beforeAll — otherwise
  // mkdtempSync("/tmp/…") throws ENOENT on Windows at collection time
  // (issue #225).
  let workdir = "";
  let xdgConfig = "";
  let xdgState = "";
  let sockAPath = "";
  let sockBPath = "";
  let defaultSockPath = "";
  let stateFile = "";
  // rebuildup.inbox is native since #291: spawn the shipped binary
  // directly (plugins/inbox/bin/inbox, cargo target fallback).
  const INBOX_BIN = inboxBinary();
  // rebuildup.git is native since #292: spawn the shipped binary.
  const GIT_CLI = gitBinary();

  const servers: FakeHerdr[] = [];
  const baseEnv = () => ({
    XDG_CONFIG_HOME: xdgConfig,
    XDG_STATE_HOME: xdgState,
    HOME: workdir,
  });

  function stateEntries(): Record<string, unknown> {
    if (!existsSync(stateFile)) return {};
    const parsed = JSON.parse(readFileSync(stateFile, "utf8")) as {
      sessions?: Record<string, Record<string, unknown>>;
    };
    return parsed.sessions ?? {};
  }

  beforeAll(async () => {
    // macOS caps AF_UNIX paths at 104 chars so POSIX anchors under /tmp;
    // Windows uses a named-pipe endpoint with no such limit (issue #266).
    workdir = mkdtempSync(path.join(IS_WINDOWS ? tmpdir() : "/tmp", "ht-target-"));
    xdgConfig = path.join(workdir, "xdg-config");
    xdgState = path.join(workdir, "xdg-state");
    sockAPath = path.join(xdgConfig, "herdr", "sessions", "aaa", "herdr.sock");
    sockBPath = path.join(xdgConfig, "herdr", "sessions", "bbb", "herdr.sock");
    defaultSockPath = path.join(xdgConfig, "herdr", "herdr.sock");
    stateFile = path.join(xdgState, "herdr", "plugins", "rebuildup.inbox", "state.json");
    servers.push(await startFakeHerdr(sockAPath, sessionSnapshot("aaa")));
    servers.push(await startFakeHerdr(sockBPath, sessionSnapshot("bbb")));
    servers.push(await startFakeHerdr(defaultSockPath, sessionSnapshot("default")));
  });

  test("ambient A + explicit --session B: conflict, zero side effects on A", async () => {
    const before = JSON.stringify(stateEntries());
    const run = await runPluginCli(INBOX_BIN, ["settle", "w1:p1", "--session", "bbb"], {
      ...baseEnv(),
      HERDR_SOCKET_PATH: sockAPath,
      HERDR_SESSION: "aaa",
    });
    expect(run.status).toBe(1);
    expect(run.errorCode).toBe("target_conflict");
    // Zero requests reached either fake session — resolution failed before
    // any socket I/O.
    expect(servers[0]?.requests ?? []).toHaveLength(0);
    expect(servers[1]?.requests ?? []).toHaveLength(0);
    expect(JSON.stringify(stateEntries())).toBe(before);
  }, 60_000);

  test("consistent ambient + explicit --session B settles on B only", async () => {
    const run = await runPluginCli(INBOX_BIN, ["settle", "w1:p1", "--session", "bbb"], {
      ...baseEnv(),
      HERDR_SOCKET_PATH: sockBPath,
      HERDR_SESSION: "bbb",
    });
    expect(run.status).toBe(0);
    const b = servers[1];
    expect(b?.requests.map((r) => r.method)).toContain("session.snapshot");
    expect(b?.requests.map((r) => r.method)).toContain("pane.report_metadata");
    // Session A saw nothing.
    expect(servers[0]?.requests ?? []).toHaveLength(0);
    // State was namespaced under B's socket.
    const sessions = stateEntries();
    const keys = Object.keys(sessions);
    expect(keys.some((k) => k.includes("bbb"))).toBe(true);
    expect(keys.some((k) => k.includes("aaa"))).toBe(false);
  }, 60_000);

  test("ambient-only settle targets the ambient session", async () => {
    const bSnapshotsBefore =
      servers[1]?.requests.filter((r) => r.method === "session.snapshot").length ?? 0;
    const run = await runPluginCli(INBOX_BIN, ["settle", "w1:p1"], {
      ...baseEnv(),
      HERDR_SOCKET_PATH: sockAPath,
      HERDR_SESSION: "aaa",
    });
    expect(run.status).toBe(0);
    expect(servers[0]?.requests.map((r) => r.method)).toContain("pane.report_metadata");
    const bSnapshotsAfter =
      servers[1]?.requests.filter((r) => r.method === "session.snapshot").length ?? 0;
    expect(bSnapshotsAfter - bSnapshotsBefore).toBe(0);
  }, 60_000);

  test("mutation with no explicit/ambient target fails closed", async () => {
    const run = await runPluginCli(INBOX_BIN, ["settle", "w1:p1"], baseEnv());
    expect(run.status).toBe(1);
    expect(run.errorCode).toBe("target_required");
    // Even the default-session fake saw nothing.
    expect(servers[2]?.requests ?? []).toHaveLength(0);
  }, 60_000);

  test("read command still uses the implicit default socket", async () => {
    const run = await runPluginCli(INBOX_BIN, ["list"], baseEnv());
    expect(run.status).toBe(0);
    expect(servers[2]?.requests.map((r) => r.method)).toContain("session.snapshot");
  }, 60_000);

  test("ambient workspace mismatch fails closed before metadata writes", async () => {
    const b = servers[1];
    const snapshotCount = b?.requests.length ?? 0;
    const run = await runPluginCli(INBOX_BIN, ["settle"], {
      ...baseEnv(),
      HERDR_SOCKET_PATH: sockBPath,
      HERDR_PANE_ID: "w1:p1",
      HERDR_WORKSPACE_ID: "w99",
      HERDR_TAB_ID: "t1",
    });
    expect(run.status).toBe(1);
    expect(run.errorCode).toBe("ambient_identity_mismatch");
    // The snapshot was read (target resolution succeeded) but no metadata
    // mutation was projected onto the wrong workspace's pane.
    expect(b?.requests.slice(snapshotCount).map((r) => r.method) ?? []).not.toContain(
      "pane.report_metadata",
    );
  }, 60_000);

  test("explicit pane positional overrides ambient pane id", async () => {
    const run = await runPluginCli(INBOX_BIN, ["settle", "w1:p1"], {
      ...baseEnv(),
      HERDR_SOCKET_PATH: sockBPath,
      HERDR_PANE_ID: "w9:p9",
      HERDR_WORKSPACE_ID: "w9",
    });
    expect(run.status).toBe(0);
  }, 60_000);

  test("git CLI honors the same conflict rule", async () => {
    const aCount = servers[0]?.requests.length ?? 0;
    const run = await runPluginCli(GIT_CLI, ["status", "w1:p1", "--session", "bbb"], {
      ...baseEnv(),
      HERDR_SOCKET_PATH: sockAPath,
      HERDR_SESSION: "aaa",
    });
    expect(run.status).toBe(1);
    expect(run.errorCode).toBe("target_conflict");
    expect((servers[0]?.requests.length ?? 0) - aCount).toBe(0);
  }, 60_000);

  afterAll(async () => {
    await Promise.all(servers.map((s) => s.close()));
    rmSync(workdir, { recursive: true, force: true });
  });
});
