import { afterAll, describe, expect, test } from "bun:test";
import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { loadRuns } from "../plugins/projects/src/runs";
import { loadState } from "../plugins/projects/src/state";
import {
  buildTriggerMark,
  canonicalTriggerEventName,
  extractWorktreePath,
} from "../plugins/projects/src/triggers";
import { inboxPluginRootFixture } from "./helpers/inbox-bin";
import { herdrSocketEndpoint, normalizePath, testEnv } from "./helpers/test-env";

const dirs: string[] = [];
const savedStateDir = process.env.HERDR_PLUGIN_STATE_DIR;
const savedConfigDir = process.env.HERDR_PLUGIN_CONFIG_DIR;

function tempDir(): string {
  const dir = mkdtempSync(path.join(tmpdir(), "projects-triggers-test-"));
  dirs.push(dir);
  return dir;
}

afterAll(() => {
  for (const dir of dirs) rmSync(dir, { recursive: true, force: true });
  if (savedStateDir === undefined) delete process.env.HERDR_PLUGIN_STATE_DIR;
  else process.env.HERDR_PLUGIN_STATE_DIR = savedStateDir;
  if (savedConfigDir === undefined) delete process.env.HERDR_PLUGIN_CONFIG_DIR;
  else process.env.HERDR_PLUGIN_CONFIG_DIR = savedConfigDir;
});

function useStateDir(): string {
  const dir = tempDir();
  process.env.HERDR_PLUGIN_STATE_DIR = dir;
  process.env.HERDR_PLUGIN_CONFIG_DIR = path.join(dir, "config");
  // Settle drain resolves inbox via config `inboxRoot`: point it at the
  // platform-runnable fixture — the committed bin/inbox prebuilt is
  // macOS arm64 only and cannot exec on Linux CI (#291). With a dead
  // test socket the drain still reports inbox_unavailable honestly.
  const configDir = process.env.HERDR_PLUGIN_CONFIG_DIR;
  mkdirSync(configDir, { recursive: true });
  writeFileSync(
    path.join(configDir, "config.json"),
    `${JSON.stringify({ inboxRoot: inboxPluginRootFixture() }, null, 2)}\n`,
  );
  return dir;
}

function git(cwd: string, args: string[]): string {
  const result = spawnSync("git", args, { cwd, encoding: "utf8", env: testEnv() });
  if (result.status !== 0) throw new Error(`git ${args.join(" ")}: ${result.stderr}`);
  return result.stdout;
}

function initRepo(): string {
  const dir = realpathSync(tempDir());
  git(dir, ["init", "-b", "main"]);
  git(dir, ["config", "user.email", "t@t"]);
  git(dir, ["config", "user.name", "t"]);
  writeFileSync(path.join(dir, "a.txt"), "one\n");
  git(dir, ["add", "a.txt"]);
  git(dir, ["commit", "-qm", "init"]);
  return dir;
}

/** Repo with a committed t3.json, plus a linked worktree checkout of it. */
function repoWithWorktree(scripts: unknown[]): { repo: string; worktree: string } {
  const repo = initRepo();
  writeFileSync(path.join(repo, "t3.json"), JSON.stringify({ scripts }, null, 2));
  git(repo, ["add", "t3.json"]);
  git(repo, ["commit", "-qm", "add t3.json"]);
  const wtParent = realpathSync(tempDir());
  const worktree = path.join(wtParent, "feature-wt");
  git(repo, ["worktree", "add", worktree, "-b", "feature"]);
  return { repo, worktree: realpathSync(worktree) };
}

// --- Fake herdr socket ------------------------------------------------------

interface FakePane {
  pane_id: string;
  workspace_id: string;
  tab_id: string;
  cwd: string | null;
  input: string | null;
  output: string;
  alive: boolean;
}

interface FakeHerdr {
  socketPath: string;
  /** cwd passed to each workspace.create call, in order. */
  createdCwds: (string | null)[];
  /** Injected input lines per pane. */
  inputs: string[];
  close: () => Promise<void>;
}

/**
 * Minimal fake herdr socket: newline-delimited JSON-RPC answering the
 * methods the projects runner + trigger drain preflight use. On
 * `pane.send_input` it simulates the shell echo plus a completion
 * marker with exit code 0 (same trick as tests/projects-runner.test.ts).
 */
async function fakeHerdr(exitCode = 0): Promise<FakeHerdr> {
  const socketPath = path.join(tempDir(), "fake-herdr.sock");
  const panes = new Map<string, FakePane>();
  const createdCwds: (string | null)[] = [];
  const inputs: string[] = [];
  let nextPane = 0;

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
        const message = JSON.parse(line) as {
          id?: string;
          method?: string;
          params?: Record<string, unknown>;
        };
        const reply = (result: unknown) =>
          conn.write(`${JSON.stringify({ id: message.id, result })}\n`);
        const fail = (code: string, msg: string) =>
          conn.write(`${JSON.stringify({ id: message.id, error: { code, message: msg } })}\n`);

        switch (message.method) {
          case "session.snapshot":
            reply({ snapshot: { panes: [], agents: {} } });
            break;
          case "workspace.create": {
            nextPane += 1;
            const pane: FakePane = {
              pane_id: `pane-${nextPane}`,
              workspace_id: `ws-${nextPane}`,
              tab_id: `tab-${nextPane}`,
              cwd: typeof message.params?.cwd === "string" ? message.params.cwd : null,
              input: null,
              output: "",
              alive: true,
            };
            panes.set(pane.pane_id, pane);
            createdCwds.push(pane.cwd);
            reply({
              type: "workspace_created",
              workspace: { workspace_id: pane.workspace_id },
              tab: { tab_id: pane.tab_id },
              root_pane: {
                pane_id: pane.pane_id,
                workspace_id: pane.workspace_id,
                tab_id: pane.tab_id,
              },
            });
            break;
          }
          case "pane.send_input": {
            const pane = panes.get(String(message.params?.pane_id));
            if (!pane || !pane.alive) {
              fail("pane_not_found", "pane gone");
              break;
            }
            pane.input = String(message.params?.text ?? "");
            inputs.push(pane.input);
            const token = /__HERDR_DONE_(\S+?)__/.exec(pane.input)?.[1];
            if (token !== undefined) {
              pane.output = `${pane.input}\nfake script output\n__HERDR_DONE_${token}__=${exitCode}\n`;
            }
            reply({ ok: true });
            break;
          }
          case "pane.wait_for_output": {
            const pane = panes.get(String(message.params?.pane_id));
            if (!pane || !pane.alive) {
              fail("pane_not_found", "pane gone");
              break;
            }
            const value = String(
              (message.params?.match as { value?: string } | undefined)?.value ?? "",
            );
            const re = new RegExp(value);
            if (!re.test(pane.output)) {
              fail("timeout", "wait_for_output timed out");
              break;
            }
            reply({
              type: "output_matched",
              pane_id: pane.pane_id,
              revision: 1,
              matched_line: pane.output.split("\n").find((l) => re.test(l)) ?? null,
              read: {
                pane_id: pane.pane_id,
                workspace_id: pane.workspace_id,
                tab_id: pane.tab_id,
                source: "recent_unwrapped",
                format: "text",
                text: pane.output,
                revision: 1,
                truncated: false,
              },
            });
            break;
          }
          case "pane.get": {
            const pane = panes.get(String(message.params?.pane_id));
            if (!pane || !pane.alive) {
              fail("pane_not_found", "pane gone");
              break;
            }
            reply({
              pane: {
                pane_id: pane.pane_id,
                workspace_id: pane.workspace_id,
                tab_id: pane.tab_id,
                cwd: pane.cwd,
              },
            });
            break;
          }
          case "pane.read": {
            const pane = panes.get(String(message.params?.pane_id));
            if (!pane || !pane.alive) {
              fail("pane_not_found", "pane gone");
              break;
            }
            reply({
              read: {
                pane_id: pane.pane_id,
                workspace_id: pane.workspace_id,
                tab_id: pane.tab_id,
                source: "recent",
                format: "text",
                text: pane.output,
                revision: 1,
                truncated: false,
              },
            });
            break;
          }
          case "pane.report_metadata":
          case "pane.send_keys":
          case "pane.close":
          case "pane.process_info":
            reply({ ok: true });
            break;
          default:
            fail("unknown_method", `fake herdr: unhandled ${String(message.method)}`);
        }
      }
    });
  });
  await new Promise<void>((resolveListen, rejectListen) => {
    server.once("error", rejectListen);
    server.listen(herdrSocketEndpoint(socketPath), () => resolveListen());
  });
  return {
    socketPath,
    createdCwds,
    inputs,
    close: () => new Promise((resolveClose) => server.close(() => resolveClose())),
  };
}

// --- CLI / hook process helpers ----------------------------------------------

const PLUGIN = path.join(__dirname, "..", "plugins", "projects");
const HOOK = path.join(PLUGIN, "src", "hook.ts");
const CLI = path.join(PLUGIN, "src", "cli.ts");

let deadSocketPath: string | null = null;
function deadSocket(): string {
  if (deadSocketPath === null) deadSocketPath = path.join(tempDir(), "no-such-herdr.sock");
  return deadSocketPath;
}

function baseEnv(extra?: Record<string, string>): Record<string, string> {
  // Plugin dirs are the explicit opt-in channel: tests set them on
  // process.env per-case via useStateDir(); everything else HERDR_* stays
  // scrubbed by testEnv (#199).
  //
  // #199 hygiene: the settle-trigger drain resolves a socket on EVERY
  // scripts-family call. Default to a dead socket so tests that do not
  // pass one can never reach the developer's live herdr session; tests
  // that need a socket pass HERDR_SOCKET_PATH via `extra`.
  return testEnv({
    HERDR_PLUGIN_STATE_DIR: process.env.HERDR_PLUGIN_STATE_DIR,
    HERDR_PLUGIN_CONFIG_DIR: process.env.HERDR_PLUGIN_CONFIG_DIR,
    HERDR_SOCKET_PATH: deadSocket(),
    ...extra,
  });
}

interface ProcResult {
  status: number;
  stdout: string;
  stderr: string;
  json: Record<string, unknown> | null;
}

function parseProc(stdout: string, stderr: string, status: number): ProcResult {
  let json: Record<string, unknown> | null = null;
  try {
    json = JSON.parse(stdout) as Record<string, unknown>;
  } catch {
    /* usage/non-JSON output */
  }
  return { status, stdout, stderr, json };
}

/** Async spawn: the fake socket lives in this process's event loop. */
function runProc(
  file: string,
  args: string[],
  extraEnv?: Record<string, string>,
): Promise<ProcResult> {
  return new Promise((resolve, reject) => {
    const child = spawn(process.execPath, [file, ...args], { env: baseEnv(extraEnv) });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString("utf8");
    });
    child.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
    });
    child.on("error", reject);
    child.on("close", (code) => resolve(parseProc(stdout, stderr, code ?? -1)));
  });
}

function runHook(extraEnv: Record<string, string>): Promise<ProcResult> {
  return runProc(HOOK, [], extraEnv);
}

function runCli(args: string[], extraEnv?: Record<string, string>): Promise<ProcResult> {
  return runProc(CLI, args, extraEnv);
}

function worktreeCreatedEnv(socketPath: string, worktreePath: string): Record<string, string> {
  return {
    HERDR_SOCKET_PATH: socketPath,
    HERDR_PLUGIN_EVENT: "worktree.created",
    HERDR_PLUGIN_EVENT_JSON: JSON.stringify({
      event: "worktree_created",
      data: {
        type: "worktree_created",
        workspace: { workspace_id: "w1" },
        worktree: {
          path: worktreePath,
          is_linked_worktree: true,
          label: "feature",
          branch: "feature",
        },
      },
    }),
  };
}

// --- Unit-level helpers ------------------------------------------------------

describe("trigger mark helpers", () => {
  test("canonicalTriggerEventName accepts both wire forms only", () => {
    expect(canonicalTriggerEventName("worktree.created")).toBe("worktree.created");
    expect(canonicalTriggerEventName("worktree_created")).toBe("worktree.created");
    expect(canonicalTriggerEventName("pane.closed")).toBeNull();
    expect(canonicalTriggerEventName("worktree.opened")).toBeNull();
    expect(canonicalTriggerEventName(undefined)).toBeNull();
  });

  test("extractWorktreePath reads data.worktree.path (schema shape)", () => {
    expect(extractWorktreePath({ worktree: { path: "/wt/a" } })).toBe("/wt/a");
    expect(extractWorktreePath({ worktree_path: "/wt/b" })).toBe("/wt/b");
    expect(extractWorktreePath({ worktree: {} })).toBeNull();
    expect(extractWorktreePath({})).toBeNull();
    expect(extractWorktreePath(null)).toBeNull();
  });

  test("buildTriggerMark rejects non-worktree.created events and pathless payloads", () => {
    const data = { worktree: { path: "/wt/x" } };
    expect(buildTriggerMark({ event: "pane.closed", data, socketPath: "/s" })).toBeNull();
    expect(buildTriggerMark({ event: "worktree.created", data: {}, socketPath: "/s" })).toBeNull();
    const mark = buildTriggerMark({
      event: "worktree.created",
      data: { worktree: { path: "/wt/x" }, workspace: { workspace_id: "w9" } },
      socketPath: "/tmp/sock",
      receivedAt: "2026-01-01T00:00:00.000Z",
    });
    expect(mark?.worktree_path).toBe("/wt/x");
    expect(mark?.workspace_id).toBe("w9");
    expect(mark?.socket_path).toBe("/tmp/sock");
    expect(mark?.received_at).toBe("2026-01-01T00:00:00.000Z");
  });
});

// --- Hook entrypoint ---------------------------------------------------------

describe("hook entrypoint", () => {
  test("records a pending mark for worktree.created; malformed/other events record nothing", async () => {
    const stateDir = useStateDir();
    const fake = await fakeHerdr();
    try {
      const wt = path.join(tempDir(), "wt");
      const hook = await runHook({
        ...worktreeCreatedEnv(fake.socketPath, wt),
        HERDR_PLUGIN_STATE_DIR: stateDir,
      });
      expect(hook.status).toBe(0);
      const state = await loadState();
      expect(state.pending_triggers).toHaveLength(1);
      expect(state.pending_triggers?.[0]?.event).toBe("worktree.created");
      expect(state.pending_triggers?.[0]?.worktree_path).toBe(wt);
      expect(state.pending_triggers?.[0]?.socket_path).toBe(fake.socketPath);

      // Duplicate delivery of the SAME event collapses (idempotent record).
      const again = await runHook({
        ...worktreeCreatedEnv(fake.socketPath, wt),
        HERDR_PLUGIN_STATE_DIR: stateDir,
      });
      expect(again.status).toBe(0);
      expect((await loadState()).pending_triggers).toHaveLength(1);

      // Malformed JSON, missing payload, wrong event, missing worktree
      // path: all exit 0 and record nothing.
      const cases: Record<string, string>[] = [
        { HERDR_PLUGIN_EVENT_JSON: "not-json{" },
        {},
        {
          HERDR_PLUGIN_EVENT: "pane.closed",
          HERDR_PLUGIN_EVENT_JSON: JSON.stringify({
            event: "pane_closed",
            data: { type: "pane_closed" },
          }),
        },
        {
          HERDR_PLUGIN_EVENT: "worktree.created",
          HERDR_PLUGIN_EVENT_JSON: JSON.stringify({
            event: "worktree_created",
            data: { type: "worktree_created", worktree: {} },
          }),
        },
      ];
      for (const extra of cases) {
        const r = await runHook({
          HERDR_SOCKET_PATH: fake.socketPath,
          HERDR_PLUGIN_STATE_DIR: stateDir,
          ...extra,
        });
        expect(r.status).toBe(0);
      }
      expect((await loadState()).pending_triggers).toHaveLength(1);
    } finally {
      await fake.close();
    }
  }, 30_000);
});

// --- Drain end-to-end ---------------------------------------------------------

describe("scripts tick drain (fake hook events)", () => {
  test("hook mark → tick fires runOnWorktreeCreate script in the new worktree; no double-fire", async () => {
    const stateDir = useStateDir();
    const { repo, worktree } = repoWithWorktree([
      { name: "setup", command: "echo setup-ran", runOnWorktreeCreate: true },
      { name: "plain", command: "echo plain" },
    ]);
    const fake = await fakeHerdr();
    try {
      const reg = await runCli(["register", repo], { HERDR_PLUGIN_STATE_DIR: stateDir });
      expect(reg.status).toBe(0);
      const projectId = ((reg.json?.project ?? {}) as { id: string }).id;

      const hook = await runHook({
        ...worktreeCreatedEnv(fake.socketPath, worktree),
        HERDR_PLUGIN_STATE_DIR: stateDir,
      });
      expect(hook.status).toBe(0);

      const tick = await runCli(["scripts", "tick"], {
        HERDR_PLUGIN_STATE_DIR: stateDir,
        HERDR_SOCKET_PATH: fake.socketPath,
      });
      expect(tick.status).toBe(0);
      const triggers = tick.json?.triggers as {
        fired: Array<Record<string, unknown>>;
        skipped: Array<Record<string, unknown>>;
        duplicates: number;
        pending_remaining: number;
      };
      expect(triggers.fired).toHaveLength(1);
      expect(triggers.skipped).toHaveLength(0);
      expect(triggers.fired[0]?.project_id).toBe(projectId);
      expect(triggers.fired[0]?.script).toBe("setup");
      expect(triggers.fired[0]?.worktree_path).toBe(worktree);
      expect(triggers.fired[0]?.result).toBe("success");

      // Execution surface: dedicated workspace created IN the worktree.
      expect(fake.createdCwds).toEqual([worktree]);
      expect(fake.inputs[0]).toContain("echo setup-ran");
      // The runner injects command and marker as separate send_input
      // lines (#67 per-line parsing); assert across all injected input.
      expect(fake.inputs.join("\n")).toContain("__HERDR_DONE_");

      // Run record carries the trigger provenance.
      const runs = await loadRuns();
      const run = Object.values(runs.runs)[0];
      expect(run?.trigger?.kind).toBe("worktree.created");
      expect(run?.trigger?.worktree_path).toBe(worktree);
      expect(run?.trigger?.event_received_at).toBeTruthy();
      expect(normalizePath(run?.repo.root ?? "")).toBe(normalizePath(worktree));
      expect(run?.project_id).toBe(projectId);

      // Marks consumed; a second tick fires nothing new. The settle
      // drain reports inbox_unavailable (dead test socket) — honest,
      // never an error.
      const tick2 = await runCli(["scripts", "tick"], { HERDR_PLUGIN_STATE_DIR: stateDir });
      expect(tick2.status).toBe(0);
      const triggers2 = tick2.json?.triggers as
        | { fired: unknown[]; inbox_unavailable?: boolean }
        | undefined;
      expect(triggers2?.fired ?? []).toHaveLength(0);
      expect(triggers2?.inbox_unavailable).toBe(true);
      expect(fake.createdCwds).toHaveLength(1);
      expect(Object.keys((await loadRuns()).runs)).toHaveLength(1);
    } finally {
      await fake.close();
    }
  }, 30_000);

  test("no trigger without a real event: tick with empty backlog does nothing", async () => {
    const stateDir = useStateDir();
    const fake = await fakeHerdr();
    try {
      const tick = await runCli(["scripts", "tick"], { HERDR_PLUGIN_STATE_DIR: stateDir });
      expect(tick.status).toBe(0);
      // No worktree fires; the settle drain reports inbox_unavailable on
      // the dead test socket — still no fabricated trigger activity.
      const triggers = tick.json?.triggers as
        | { fired: unknown[]; inbox_unavailable?: boolean }
        | undefined;
      expect(triggers?.fired ?? []).toHaveLength(0);
      expect(triggers?.inbox_unavailable).toBe(true);
      expect(fake.createdCwds).toHaveLength(0);
      expect(Object.keys((await loadRuns()).runs)).toHaveLength(0);
    } finally {
      await fake.close();
    }
  }, 30_000);

  test("unregistered repo worktree is skipped honestly — no guessed mapping", async () => {
    const stateDir = useStateDir();
    const { repo, worktree } = repoWithWorktree([
      { name: "setup", command: "echo setup-ran", runOnWorktreeCreate: true },
    ]);
    // Register repo A; the event worktree belongs to repo B (unregistered).
    const other = initRepo();
    const fake = await fakeHerdr();
    try {
      const reg = await runCli(["register", other], { HERDR_PLUGIN_STATE_DIR: stateDir });
      expect(reg.status).toBe(0);
      void repo;
      const hook = await runHook({
        ...worktreeCreatedEnv(fake.socketPath, worktree),
        HERDR_PLUGIN_STATE_DIR: stateDir,
      });
      expect(hook.status).toBe(0);

      const tick = await runCli(["scripts", "tick"], {
        HERDR_PLUGIN_STATE_DIR: stateDir,
        HERDR_SOCKET_PATH: fake.socketPath,
      });
      expect(tick.status).toBe(0);
      const triggers = tick.json?.triggers as {
        fired: unknown[];
        skipped: Array<{ reason: string; worktree_path: string }>;
      };
      expect(triggers.fired).toHaveLength(0);
      expect(triggers.skipped).toHaveLength(1);
      expect(triggers.skipped[0]?.reason).toBe("repo_not_registered");
      expect(triggers.skipped[0]?.worktree_path).toBe(worktree);
      expect(fake.createdCwds).toHaveLength(0);
    } finally {
      await fake.close();
    }
  }, 30_000);

  test("duplicate hook deliveries for one worktree fire exactly once", async () => {
    const stateDir = useStateDir();
    const { repo, worktree } = repoWithWorktree([
      { name: "setup", command: "echo setup-ran", runOnWorktreeCreate: true },
    ]);
    const fake = await fakeHerdr();
    try {
      await runCli(["register", repo], { HERDR_PLUGIN_STATE_DIR: stateDir });
      // Same worktree path delivered twice BEFORE any drain: the second
      // record collapses at hook time; even if it did not, drain dedupes.
      for (let i = 0; i < 2; i += 1) {
        const hook = await runHook({
          ...worktreeCreatedEnv(fake.socketPath, worktree),
          HERDR_PLUGIN_STATE_DIR: stateDir,
        });
        expect(hook.status).toBe(0);
      }
      const tick = await runCli(["scripts", "tick"], {
        HERDR_PLUGIN_STATE_DIR: stateDir,
        HERDR_SOCKET_PATH: fake.socketPath,
      });
      expect(tick.status).toBe(0);
      const triggers = tick.json?.triggers as { fired: unknown[] };
      expect(triggers.fired).toHaveLength(1);
      expect(fake.createdCwds).toHaveLength(1);
    } finally {
      await fake.close();
    }
  }, 30_000);

  test("registered repo but no runOnWorktreeCreate script → honest skip", async () => {
    const stateDir = useStateDir();
    const { repo, worktree } = repoWithWorktree([
      { name: "plain", command: "echo plain" },
      { name: "settle-only", command: "echo x", runOnSettle: true },
    ]);
    const fake = await fakeHerdr();
    try {
      await runCli(["register", repo], { HERDR_PLUGIN_STATE_DIR: stateDir });
      await runHook({
        ...worktreeCreatedEnv(fake.socketPath, worktree),
        HERDR_PLUGIN_STATE_DIR: stateDir,
      });
      const tick = await runCli(["scripts", "tick"], {
        HERDR_PLUGIN_STATE_DIR: stateDir,
        HERDR_SOCKET_PATH: fake.socketPath,
      });
      expect(tick.status).toBe(0);
      const triggers = tick.json?.triggers as {
        fired: unknown[];
        skipped: Array<{ reason: string }>;
      };
      expect(triggers.fired).toHaveLength(0);
      expect(triggers.skipped[0]?.reason).toBe("no_run_on_worktree_create_scripts");
      expect(fake.createdCwds).toHaveLength(0);
    } finally {
      await fake.close();
    }
  }, 30_000);

  test("broken t3.json in the worktree → structured skip, drain survives", async () => {
    const stateDir = useStateDir();
    const { repo, worktree } = repoWithWorktree([
      { name: "setup", command: "echo setup-ran", runOnWorktreeCreate: true },
    ]);
    // Corrupt the worktree's t3.json only (main repo stays valid).
    writeFileSync(path.join(worktree, "t3.json"), "{broken");
    const fake = await fakeHerdr();
    try {
      await runCli(["register", repo], { HERDR_PLUGIN_STATE_DIR: stateDir });
      await runHook({
        ...worktreeCreatedEnv(fake.socketPath, worktree),
        HERDR_PLUGIN_STATE_DIR: stateDir,
      });
      const tick = await runCli(["scripts", "tick"], {
        HERDR_PLUGIN_STATE_DIR: stateDir,
        HERDR_SOCKET_PATH: fake.socketPath,
      });
      expect(tick.status).toBe(0);
      const triggers = tick.json?.triggers as {
        fired: unknown[];
        skipped: Array<{ reason: string }>;
      };
      expect(triggers.fired).toHaveLength(0);
      expect(triggers.skipped[0]?.reason).toBe("t3json_invalid");
      expect(fake.createdCwds).toHaveLength(0);
    } finally {
      await fake.close();
    }
  }, 30_000);

  test("deleted worktree → skip, never fabricated", async () => {
    const stateDir = useStateDir();
    const { repo, worktree } = repoWithWorktree([
      { name: "setup", command: "echo setup-ran", runOnWorktreeCreate: true },
    ]);
    const fake = await fakeHerdr();
    try {
      await runCli(["register", repo], { HERDR_PLUGIN_STATE_DIR: stateDir });
      const hook = await runHook({
        ...worktreeCreatedEnv(fake.socketPath, worktree),
        HERDR_PLUGIN_STATE_DIR: stateDir,
      });
      expect(hook.status).toBe(0);
      // Remove the worktree before the drain runs.
      git(repo, ["worktree", "remove", "--force", worktree]);
      const tick = await runCli(["scripts", "tick"], {
        HERDR_PLUGIN_STATE_DIR: stateDir,
        HERDR_SOCKET_PATH: fake.socketPath,
      });
      expect(tick.status).toBe(0);
      const triggers = tick.json?.triggers as {
        fired: unknown[];
        skipped: Array<{ reason: string }>;
      };
      expect(triggers.fired).toHaveLength(0);
      expect(triggers.skipped[0]?.reason).toBe("worktree_gone");
      expect(fake.createdCwds).toHaveLength(0);
    } finally {
      await fake.close();
    }
  }, 30_000);

  test("unreachable emitting session keeps marks pending for a later drain", async () => {
    const stateDir = useStateDir();
    const { repo, worktree } = repoWithWorktree([
      { name: "setup", command: "echo setup-ran", runOnWorktreeCreate: true },
    ]);
    const deadSocket = path.join(tempDir(), "dead.sock");
    try {
      await runCli(["register", repo], { HERDR_PLUGIN_STATE_DIR: stateDir });
      const hook = await runHook({
        ...worktreeCreatedEnv(deadSocket, worktree),
        HERDR_PLUGIN_STATE_DIR: stateDir,
      });
      expect(hook.status).toBe(0);

      const tick = await runCli(["scripts", "tick", "--timeout", "2000"], {
        HERDR_PLUGIN_STATE_DIR: stateDir,
      });
      expect(tick.status).toBe(0);
      const triggers = tick.json?.triggers as {
        fired: unknown[];
        session_unreachable?: boolean;
        pending_remaining: number;
      };
      expect(triggers.fired).toHaveLength(0);
      expect(triggers.session_unreachable).toBe(true);
      expect(triggers.pending_remaining).toBe(1);
      // Mark survives for a later drain once the session is reachable.
      const state = await loadState();
      expect(state.pending_triggers).toHaveLength(1);
    } finally {
      void 0;
    }
  }, 30_000);
});
