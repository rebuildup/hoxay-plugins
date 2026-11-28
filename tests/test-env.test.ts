import { afterAll, describe, expect, test } from "bun:test";
import { spawn } from "node:child_process";
import { readdirSync, readFileSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { inboxBinary } from "./helpers/inbox-bin";
import { envValue, herdrSocketEndpoint, socketConsistentEnv, testEnv } from "./helpers/test-env";

/**
 * Guard tests for the shared test-environment/scrub infrastructure
 * (issues #199, #211). These pin the contract that every spawned test
 * process gets a scrubbed env and every fake herdr socket binds the
 * production transport endpoint.
 */

const POISONED_HERDR_ENV = [
  "HERDR_SOCKET_PATH",
  "HERDR_SESSION",
  "HERDR_PANE_ID",
  "HERDR_WORKSPACE_ID",
  "HERDR_TAB_ID",
  "HERDR_PLUGIN_CONTEXT_JSON",
  "HERDR_ENV",
  "HERDR_BIN_PATH",
  "HERDR_STARTUP_CWD",
  "HERDR_PLUGIN_STATE_DIR",
  "HERDR_PLUGIN_CONFIG_DIR",
  "HERDR_MCP_ROOT_PROJECTS",
] as const;

const POISONED_OTHER_ENV = ["XDG_CONFIG_HOME", "XDG_STATE_HOME", "GIT_DIR", "GH_REPO"] as const;

const savedEnv = new Map<string, string | undefined>();
for (const key of [...POISONED_HERDR_ENV, ...POISONED_OTHER_ENV]) {
  savedEnv.set(key, process.env[key]);
}

function poisonAmbientEnv(): void {
  for (const key of POISONED_HERDR_ENV) process.env[key] = `/tmp/poison/${key}`;
  for (const key of POISONED_OTHER_ENV) process.env[key] = `/tmp/poison/${key}`;
}

afterAll(() => {
  for (const [key, value] of savedEnv) {
    if (value === undefined) delete process.env[key];
    else process.env[key] = value;
  }
});

describe("testEnv scrub (#199)", () => {
  test("strips every ambient HERDR_* variable by default", () => {
    poisonAmbientEnv();
    const env = testEnv();
    for (const key of POISONED_HERDR_ENV) {
      // Plugin dir defaults are re-applied to an isolated temp root —
      // never the poisoned ambient value.
      if (key === "HERDR_PLUGIN_STATE_DIR" || key === "HERDR_PLUGIN_CONFIG_DIR") {
        expect(env[key]).toBeDefined();
        expect(env[key]).not.toBe(`/tmp/poison/${key}`);
        expect(env[key]).toContain("herdr-test-env-");
      } else {
        expect(env[key], `${key} must be scrubbed`).toBeUndefined();
      }
    }
    for (const key of ["XDG_CONFIG_HOME", "XDG_STATE_HOME", "GIT_DIR", "GH_REPO"] as const) {
      expect(env[key], `${key} must be scrubbed`).toBeUndefined();
    }
    // Non-herdr environment still passes through.
    expect(envValue(env, "PATH")).toBe(envValue(process.env, "PATH"));
  });

  test("explicit extra is the only opt-in channel", () => {
    poisonAmbientEnv();
    const env = testEnv({
      HERDR_SOCKET_PATH: "/tmp/explicit.sock",
      HERDR_PANE_ID: "w1:p1",
      HERDR_PLUGIN_STATE_DIR: "/tmp/explicit-state",
    });
    expect(env.HERDR_SOCKET_PATH).toBe("/tmp/explicit.sock");
    expect(env.HERDR_PANE_ID).toBe("w1:p1");
    expect(env.HERDR_PLUGIN_STATE_DIR).toBe("/tmp/explicit-state");
    // Untouched ambient vars stay scrubbed.
    expect(env.HERDR_SESSION).toBeUndefined();
    expect(env.HERDR_PLUGIN_CONTEXT_JSON).toBeUndefined();
  });

  test("keepEnvPrefixes admits only the declared fixture channel", () => {
    poisonAmbientEnv();
    const env = testEnv({}, { keepEnvPrefixes: ["HERDR_MCP_ROOT_"] });
    expect(env.HERDR_MCP_ROOT_PROJECTS).toBe("/tmp/poison/HERDR_MCP_ROOT_PROJECTS");
    // Session identity is never admitted, even by prefix opt-in.
    expect(env.HERDR_SOCKET_PATH).toBeUndefined();
    expect(env.HERDR_PANE_ID).toBeUndefined();
  });
});

describe("socketConsistentEnv", () => {
  test("--socket PATH and --socket=PATH pin HERDR_SOCKET_PATH", () => {
    expect(socketConsistentEnv(["list", "--socket", "/tmp/a.sock"])).toEqual({
      HERDR_SOCKET_PATH: "/tmp/a.sock",
    });
    expect(socketConsistentEnv(["list", "--socket=/tmp/b.sock"])).toEqual({
      HERDR_SOCKET_PATH: "/tmp/b.sock",
    });
    expect(socketConsistentEnv(["list"])).toBeUndefined();
  });

  test("extra env merges and --socket wins over a conflicting extra", () => {
    expect(
      socketConsistentEnv(["save", "--socket", "/tmp/c.sock"], {
        HERDR_PLUGIN_STATE_DIR: "/tmp/state",
        HERDR_SOCKET_PATH: "/tmp/other.sock",
      }),
    ).toEqual({ HERDR_PLUGIN_STATE_DIR: "/tmp/state", HERDR_SOCKET_PATH: "/tmp/c.sock" });
  });
});

describe("fake socket transport parity (#211)", () => {
  test("herdrSocketEndpoint matches the production client mapping", () => {
    const logical = path.join(tmpdir(), "parity-herdr.sock");
    const expected = process.platform === "win32" ? `\\\\.\\pipe\\${logical}` : logical;
    expect(herdrSocketEndpoint(logical)).toBe(expected);
  });
});

// ---------------------------------------------------------------------------
// Negative fixture (#199 acceptance): a spawned plugin CLI running under a
// poisoned ambient environment must NOT contact the "live" socket. The
// fake live socket is a local unix socket owned by this test — never a
// real herdr session.
// ---------------------------------------------------------------------------

// rebuildup.inbox is native since #291 (plugins/inbox/bin/inbox).
const INBOX_BIN = inboxBinary();

interface FakeLiveSocket {
  socketPath: string;
  requests: string[];
  close: () => Promise<void>;
}

async function fakeLiveSocket(): Promise<FakeLiveSocket> {
  const socketPath = path.join(tmpdir(), `live-sock-${process.pid}-${Date.now()}.sock`);
  const requests: string[] = [];
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
        const message = JSON.parse(line) as { id?: string; method?: string };
        if (message.method) requests.push(message.method);
        conn.write(`${JSON.stringify({ id: message.id, result: {} })}\n`);
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
    close: () => new Promise((resolve) => server.close(() => resolve())),
  };
}

// Async spawn: the fake "live" socket lives in this process's event
// loop, so a blocking spawnSync would starve it (same constraint as the
// per-plugin fakeHerdrSocket helpers).
function runInboxList(env: Record<string, string>): Promise<{ status: number }> {
  return new Promise((resolve, reject) => {
    const child = spawn(INBOX_BIN, ["list"], { env });
    const timer = setTimeout(() => child.kill("SIGKILL"), 30_000);
    child.on("error", reject);
    child.on("close", (code) => {
      clearTimeout(timer);
      resolve({ status: code ?? -1 });
    });
  });
}

describe("spawned CLI isolation (#199 negative fixture)", () => {
  test("ambient HERDR_SOCKET_PATH never reaches a scrubbed spawn", async () => {
    const live = await fakeLiveSocket();
    const xdg = path.join(tmpdir(), `herdr-guard-${process.pid}`);
    try {
      // Simulate running inside a herdr pane: ambient context points at
      // the fake "live" session socket.
      process.env.HERDR_SOCKET_PATH = live.socketPath;
      process.env.HERDR_SESSION = "live-session";
      process.env.HERDR_PANE_ID = "w1:p1";

      // Scrubbed spawn: `list` is a read — if ambient env leaked, it would
      // resolve the live socket and emit session.snapshot here.
      await runInboxList(testEnv({ XDG_CONFIG_HOME: xdg, XDG_STATE_HOME: xdg }));
      // The command may fail (dead default socket) — that is fine; the
      // assertion that matters is zero contact with the live socket.
      expect(live.requests).toHaveLength(0);

      // Control: the same spawn WITH the ambient opt-in does reach the
      // fake socket — proving the negative above is meaningful.
      await runInboxList(
        testEnv({
          XDG_CONFIG_HOME: xdg,
          XDG_STATE_HOME: xdg,
          HERDR_SOCKET_PATH: live.socketPath,
        }),
      );
      expect(live.requests.length).toBeGreaterThan(0);
    } finally {
      await live.close();
    }
  }, 60_000);
});

// ---------------------------------------------------------------------------
// Static guard: no test file may re-introduce raw ambient-env spawns or
// unmapped fake-socket binds. If this fails, route the site through
// tests/helpers/test-env.ts instead of weakening the check.
// ---------------------------------------------------------------------------

describe("test-suite env hygiene guard", () => {
  const testDir = import.meta.dir;
  const sources = readdirSync(testDir)
    .filter((name) => name.endsWith(".test.ts"))
    .map((name) => ({ name, text: readFileSync(path.join(testDir, name), "utf8") }));

  test("no test file spreads or passes raw process.env to a child", () => {
    for (const { name, text } of sources) {
      expect(/\.\.\.process\.env/.test(text), `${name} spreads process.env`).toBe(false);
      expect(/env:\s*process\.env\b/.test(text), `${name} passes process.env as spawn env`).toBe(
        false,
      );
      expect(
        /env\s*\?\?\s*process\.env\b/.test(text),
        `${name} falls back to process.env as spawn env`,
      ).toBe(false);
    }
  });

  test("every fake herdr socket binds the mapped transport endpoint", () => {
    for (const { name, text } of sources) {
      // A raw filesystem-path listen would ENOENT on win32 where the
      // client dials \\.\pipe\<path> (#211).
      expect(/\.listen\(\s*socketPath\b/.test(text), `${name} listens on the raw socket path`).toBe(
        false,
      );
    }
  });
});
