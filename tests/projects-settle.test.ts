import { afterAll, describe, expect, test } from "bun:test";
import { spawn, spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { resolveInboxSource } from "../plugins/projects/src/inbox-source";
import { loadRuns, updateRuns } from "../plugins/projects/src/runs";
import { inboxBinary, inboxPluginRootFixture } from "./helpers/inbox-bin";
import { herdrSocketEndpoint, normalizePath, testEnv } from "./helpers/test-env";

/**
 * Contract tests for the #182 settle triggers: projects drains
 * rebuildup.inbox's `transitions` CLI surface (ADR-0029) and fires
 * `runOnSettle` / stops `stopOnSettle` runs. The REAL inbox cli.ts is
 * spawned as a subprocess (sibling resolution), exactly like production.
 *
 * #199 hygiene: every spawned CLI gets HERDR_* stripped except the
 * explicit socket/plugin dirs, and XDG_CONFIG_HOME/XDG_STATE_HOME are
 * pinned to temp dirs so the default-socket fallback and the inbox
 * state dir can never touch the developer's live session/state.
 */

const dirs: string[] = [];
const savedEnv: Record<string, string | undefined> = {};
for (const key of [
  "HERDR_PLUGIN_STATE_DIR",
  "HERDR_PLUGIN_CONFIG_DIR",
  "HERDR_SOCKET_PATH",
  "HERDR_SESSION",
  "XDG_CONFIG_HOME",
  "XDG_STATE_HOME",
]) {
  savedEnv[key] = process.env[key];
}

function tempDir(): string {
  // Anchor under /tmp on POSIX: the native inbox binary (#291) uses a
  // real sockaddr_un (104-char sun_path limit) while Bun tolerates
  // deeper $TMPDIR paths — fake socket paths must stay connectable.
  const dir = mkdtempSync(
    path.join(process.platform === "win32" ? tmpdir() : "/tmp", "projects-settle-test-"),
  );
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

const PLUGIN = path.join(__dirname, "..", "plugins", "projects");
const CLI = path.join(PLUGIN, "src", "cli.ts");
// rebuildup.inbox is native since #291: spawn bin/inbox directly.
const INBOX_CLI = inboxBinary();

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

// --- Per-test fixture --------------------------------------------------------

interface Fixture {
  socketPath: string;
  projStateDir: string;
  projConfigDir: string;
  xdgState: string;
  xdgConfig: string;
  inboxStateDir: string;
  inboxConfigDir: string;
  createdCwds: (string | null)[];
  inputs: string[];
  closedPanes: string[];
  keyedPanes: string[];
  close(): Promise<void>;
}

interface FakePane {
  pane_id: string;
  workspace_id: string;
  tab_id: string | null;
  cwd: string | null;
  input: string | null;
  output: string;
  alive: boolean;
}

interface SnapshotPane {
  pane_id: string;
  workspace_id?: string;
  tab_id?: string;
  cwd?: string;
}

interface SnapshotAgent {
  pane_id: string;
  agent_status?: string;
  state_change_seq?: number;
  cwd?: string;
}

/**
 * Fake herdr socket answering session.snapshot (configurable panes/agents)
 * plus the runner surface (workspace.create / pane.*). `extraPanes` seeds
 * execution panes that are NOT inbox snapshot panes (e.g. crafted runs).
 */
async function fakeHerdr(opts: {
  panes: SnapshotPane[];
  agents?: SnapshotAgent[];
  extraPanes?: SnapshotPane[];
  /**
   * Bind the socket at the XDG default location
   * ($XDG_CONFIG_HOME/herdr/herdr.sock) so env-less CLI invocations
   * resolve the implicit default session — required to exercise the
   * settle-drain `target_required` gate (O2).
   */
  defaultSocket?: boolean;
}): Promise<Fixture> {
  const socketDir = tempDir();
  const projStateDir = tempDir();
  const projConfigDir = path.join(projStateDir, "config");
  const xdgState = tempDir();
  const xdgConfig = tempDir();
  const socketPath = opts.defaultSocket
    ? path.join(xdgConfig, "herdr", "herdr.sock")
    : path.join(socketDir, "fake-herdr.sock");
  if (opts.defaultSocket) mkdirSync(path.dirname(socketPath), { recursive: true });
  const panes = new Map<string, FakePane>();
  const createdCwds: (string | null)[] = [];
  const inputs: string[] = [];
  const closedPanes: string[] = [];
  const keyedPanes: string[] = [];
  let nextPane = 0;

  const seed = (p: SnapshotPane) => {
    panes.set(p.pane_id, {
      pane_id: p.pane_id,
      workspace_id: p.workspace_id ?? "w1",
      tab_id: p.tab_id ?? "w1:t1",
      cwd: p.cwd ?? null,
      input: null,
      output: "",
      alive: true,
    });
  };
  for (const p of [...opts.panes, ...(opts.extraPanes ?? [])]) seed(p);
  const agents: Record<string, SnapshotAgent> = {};
  for (const a of opts.agents ?? []) agents[a.pane_id] = a;

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
            reply({
              snapshot: {
                session: { name: "fake" },
                panes: opts.panes,
                agents,
              },
            });
            break;
          case "workspace.create": {
            nextPane += 1;
            const pane: FakePane = {
              pane_id: `run-pane-${nextPane}`,
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
              pane.output = `${pane.input}\nfake script output\n__HERDR_DONE_${token}__=0\n`;
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
          case "pane.send_keys": {
            const pane = panes.get(String(message.params?.pane_id));
            if (!pane || !pane.alive) {
              fail("pane_not_found", "pane gone");
              break;
            }
            keyedPanes.push(pane.pane_id);
            reply({ ok: true });
            break;
          }
          case "pane.close": {
            const pane = panes.get(String(message.params?.pane_id));
            if (pane) {
              pane.alive = false;
              closedPanes.push(pane.pane_id);
            }
            reply({ ok: true });
            break;
          }
          case "pane.report_metadata":
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

  const inboxStateDir = path.join(xdgState, "herdr", "plugins", "rebuildup.inbox");
  const inboxConfigDir = path.join(xdgConfig, "herdr", "plugins", "config", "rebuildup.inbox");

  // Point the settle drain at the platform-runnable inbox fixture: the
  // committed plugins/inbox/bin/inbox prebuilt is macOS arm64 only, so
  // on Linux CI the repo sibling root resolves but cannot exec (#291).
  mkdirSync(projConfigDir, { recursive: true });
  writeFileSync(
    path.join(projConfigDir, "config.json"),
    `${JSON.stringify({ inboxRoot: inboxPluginRootFixture() }, null, 2)}\n`,
  );

  return {
    socketPath,
    projStateDir,
    projConfigDir,
    xdgState,
    xdgConfig,
    inboxStateDir,
    inboxConfigDir,
    createdCwds,
    inputs,
    closedPanes,
    keyedPanes,
    close: () => new Promise((resolveClose) => server.close(() => resolveClose())),
  };
}

// --- Process helpers -----------------------------------------------------------

interface ProcResult {
  status: number;
  stdout: string;
  stderr: string;
  json: Record<string, unknown> | null;
}

function projectsEnv(fx: Fixture): Record<string, string> {
  return testEnv({
    XDG_STATE_HOME: fx.xdgState,
    XDG_CONFIG_HOME: fx.xdgConfig,
    HERDR_SOCKET_PATH: fx.socketPath,
    HERDR_PLUGIN_STATE_DIR: fx.projStateDir,
    HERDR_PLUGIN_CONFIG_DIR: fx.projConfigDir,
  });
}

function inboxEnv(fx: Fixture): Record<string, string> {
  return testEnv({
    HERDR_SOCKET_PATH: fx.socketPath,
    HERDR_PLUGIN_STATE_DIR: fx.inboxStateDir,
    HERDR_PLUGIN_CONFIG_DIR: fx.inboxConfigDir,
  });
}

/**
 * projects env WITHOUT any HERDR_SOCKET_PATH/HERDR_SESSION — CLI
 * invocations resolve the implicit default session socket (which, with
 * XDG_CONFIG_HOME pinned, is the fixture's `defaultSocket` bind). Used
 * to exercise the settle-drain `target_required` gate (O2).
 */
function projectsEnvNoTarget(fx: Fixture): Record<string, string> {
  return testEnv({
    XDG_STATE_HOME: fx.xdgState,
    XDG_CONFIG_HOME: fx.xdgConfig,
    HERDR_PLUGIN_STATE_DIR: fx.projStateDir,
    HERDR_PLUGIN_CONFIG_DIR: fx.projConfigDir,
  });
}

function runProc(file: string, args: string[], env: Record<string, string>): Promise<ProcResult> {
  return new Promise((resolve, reject) => {
    // TypeScript entrypoints run under `bun run`; the native inbox
    // binary (#291) is spawned directly.
    const isTs = file.endsWith(".ts");
    const child = isTs
      ? spawn(process.execPath, [file, ...args], { env })
      : spawn(file, args, { env });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString("utf8");
    });
    child.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
    });
    child.on("error", reject);
    child.on("close", (code) => {
      let json: Record<string, unknown> | null = null;
      try {
        json = JSON.parse(stdout) as Record<string, unknown>;
      } catch {
        /* non-JSON */
      }
      resolve({ status: code ?? -1, stdout, stderr, json });
    });
  });
}

const runProjects = (fx: Fixture, args: string[]) => runProc(CLI, args, projectsEnv(fx));
const runInbox = (fx: Fixture, args: string[]) => runProc(INBOX_CLI, args, inboxEnv(fx));

// --- Tests ---------------------------------------------------------------------

describe("settle triggers: runOnSettle / stopOnSettle (#182)", () => {
  test("settled transition fires runOnSettle in the settled worktree; cursor idempotent", async () => {
    const { repo, worktree } = repoWithWorktree([
      { name: "on-settle", command: "echo settle-ran", runOnSettle: true },
      { name: "plain", command: "echo plain" },
    ]);
    const fx = await fakeHerdr({
      panes: [{ pane_id: "w1:p1", workspace_id: "w1", cwd: worktree }],
      agents: [{ pane_id: "w1:p1", agent_status: "done", state_change_seq: 1, cwd: worktree }],
    });
    try {
      const reg = await runProjects(fx, ["register", repo]);
      expect(reg.status).toBe(0);
      const projectId = ((reg.json?.project ?? {}) as { id: string }).id;

      const settle = await runInbox(fx, ["settle", "w1:p1"]);
      expect(settle.status).toBe(0);

      const tick = await runProjects(fx, ["scripts", "tick"]);
      expect(tick.status).toBe(0);
      const triggers = tick.json?.triggers as {
        fired: Array<Record<string, unknown>>;
        skipped: Array<Record<string, unknown>>;
        settle?: { session_key: string; cursor_before: number; cursor_after: number };
      };
      expect(triggers.fired).toHaveLength(1);
      expect(triggers.skipped).toHaveLength(0);
      expect(triggers.fired[0]).toMatchObject({
        project_id: projectId,
        script: "on-settle",
        worktree_path: worktree,
        result: "success",
        source: "inbox.settled",
        pane_id: "w1:p1",
        transition_seq: 1,
      });
      expect(triggers.settle?.cursor_before).toBe(0);
      expect(triggers.settle?.cursor_after).toBe(1);

      // Execution surface: dedicated workspace created IN the worktree.
      expect(fx.createdCwds).toEqual([worktree]);
      expect(fx.inputs[0]).toContain("echo settle-ran");

      // Run record carries inbox.settled provenance.
      process.env.HERDR_PLUGIN_STATE_DIR = fx.projStateDir;
      process.env.HERDR_PLUGIN_CONFIG_DIR = fx.projConfigDir;
      const runs = await loadRuns();
      const run = Object.values(runs.runs)[0];
      expect(run?.trigger).toMatchObject({
        kind: "inbox.settled",
        pane_id: "w1:p1",
        transition_seq: 1,
      });
      expect(normalizePath(run?.repo.root ?? "")).toBe(normalizePath(worktree));

      // Cursor advanced: a second tick is a full no-op.
      const tick2 = await runProjects(fx, ["scripts", "tick"]);
      expect(tick2.status).toBe(0);
      expect(tick2.json?.triggers).toBeUndefined();
      expect(fx.createdCwds).toHaveLength(1);
      expect(Object.keys((await loadRuns()).runs)).toHaveLength(1);
    } finally {
      await fx.close();
    }
  }, 30_000);

  test("settle superseded by restore before drain → settle_superseded, no fire", async () => {
    const { repo, worktree } = repoWithWorktree([
      { name: "on-settle", command: "echo settle-ran", runOnSettle: true },
    ]);
    const fx = await fakeHerdr({
      panes: [{ pane_id: "w1:p1", workspace_id: "w1", cwd: worktree }],
      agents: [{ pane_id: "w1:p1", agent_status: "done", state_change_seq: 1, cwd: worktree }],
    });
    try {
      await runProjects(fx, ["register", repo]);
      await runInbox(fx, ["settle", "w1:p1"]);
      const restore = await runInbox(fx, ["restore", "w1:p1"]);
      expect(restore.status).toBe(0);

      const tick = await runProjects(fx, ["scripts", "tick"]);
      expect(tick.status).toBe(0);
      const triggers = tick.json?.triggers as {
        fired: unknown[];
        skipped: Array<{ reason: string; pane_id?: string }>;
      };
      expect(triggers.fired).toHaveLength(0);
      expect(triggers.skipped).toHaveLength(1);
      expect(triggers.skipped[0]?.reason).toBe("settle_superseded");
      expect(triggers.skipped[0]?.pane_id).toBe("w1:p1");
      expect(fx.createdCwds).toHaveLength(0);
    } finally {
      await fx.close();
    }
  }, 30_000);

  test("settle on an unregistered repo → repo_not_registered, no guessed mapping", async () => {
    const { worktree } = repoWithWorktree([
      { name: "on-settle", command: "echo settle-ran", runOnSettle: true },
    ]);
    const other = initRepo();
    const fx = await fakeHerdr({
      panes: [{ pane_id: "w1:p1", workspace_id: "w1", cwd: worktree }],
      agents: [{ pane_id: "w1:p1", agent_status: "done", state_change_seq: 1, cwd: worktree }],
    });
    try {
      await runProjects(fx, ["register", other]);
      await runInbox(fx, ["settle", "w1:p1"]);
      const tick = await runProjects(fx, ["scripts", "tick"]);
      expect(tick.status).toBe(0);
      const triggers = tick.json?.triggers as {
        fired: unknown[];
        skipped: Array<{ reason: string }>;
      };
      expect(triggers.fired).toHaveLength(0);
      expect(triggers.skipped[0]?.reason).toBe("repo_not_registered");
      expect(fx.createdCwds).toHaveLength(0);
    } finally {
      await fx.close();
    }
  }, 30_000);

  test("no settle transition → no fire; foreign session → nothing touched", async () => {
    const { repo, worktree } = repoWithWorktree([
      { name: "on-settle", command: "echo settle-ran", runOnSettle: true },
    ]);
    const fxA = await fakeHerdr({
      panes: [{ pane_id: "w1:p1", workspace_id: "w1", cwd: worktree }],
      agents: [{ pane_id: "w1:p1", agent_status: "done", state_change_seq: 1, cwd: worktree }],
    });
    const fxB = await fakeHerdr({ panes: [], agents: [] });
    try {
      await runProjects(fxA, ["register", repo]);
      // NOTE: register ran against fxA env; state is host-global so the
      // same projects state dir must be shared — re-register under fxB
      // is unnecessary because projects state lives in fxA.projStateDir.
      // For the foreign-session check we settle on fxA then drain fxB:
      // fxB's socket yields a different inbox session_key → empty log.
      const settle = await runInbox(fxA, ["settle", "w1:p1"]);
      expect(settle.status).toBe(0);

      // Drain against fxB: its session_key has no transitions.
      const tickB = await runProc(CLI, ["scripts", "tick"], {
        ...projectsEnv(fxB),
        HERDR_PLUGIN_STATE_DIR: fxA.projStateDir,
        HERDR_PLUGIN_CONFIG_DIR: fxA.projConfigDir,
      });
      expect(tickB.status).toBe(0);
      expect(fxA.createdCwds).toHaveLength(0);
      expect(fxB.createdCwds).toHaveLength(0);

      // Drain against fxA fires exactly once.
      const tickA = await runProjects(fxA, ["scripts", "tick"]);
      expect(tickA.status).toBe(0);
      const triggers = tickA.json?.triggers as { fired: unknown[] };
      expect(triggers.fired).toHaveLength(1);
      expect(fxA.createdCwds).toEqual([worktree]);
    } finally {
      await fxA.close();
      await fxB.close();
    }
  }, 30_000);

  test("stopOnSettle stops only running runs on the same worktree root with the flag", async () => {
    const { repo, worktree } = repoWithWorktree([
      { name: "keeper", command: "sleep 999", stopOnSettle: true },
      { name: "no-flag", command: "sleep 999" },
      { name: "on-settle", command: "echo settle-ran", runOnSettle: true },
    ]);
    // A second worktree of the SAME repo group — its runs must survive.
    const wt2Parent = realpathSync(tempDir());
    const worktree2 = path.join(wt2Parent, "other-wt");
    git(repo, ["worktree", "add", worktree2, "-b", "other"]);
    const wt2 = realpathSync(worktree2);

    const fx = await fakeHerdr({
      panes: [{ pane_id: "w1:p1", workspace_id: "w1", cwd: worktree }],
      agents: [{ pane_id: "w1:p1", agent_status: "done", state_change_seq: 1, cwd: worktree }],
      extraPanes: [
        { pane_id: "pane-stop-me", cwd: worktree },
        { pane_id: "pane-stop-other-wt", cwd: wt2 },
        { pane_id: "pane-no-flag", cwd: worktree },
      ],
    });
    try {
      const reg = await runProjects(fx, ["register", repo]);
      expect(reg.status).toBe(0);
      const projectId = ((reg.json?.project ?? {}) as { id: string }).id;

      // Craft three running run records directly (deterministic; no
      // marker wait). Same worktree+flag → stopped; other worktree →
      // untouched; same worktree without flag → untouched.
      process.env.HERDR_PLUGIN_STATE_DIR = fx.projStateDir;
      process.env.HERDR_PLUGIN_CONFIG_DIR = fx.projConfigDir;
      const baseRun = {
        project_id: projectId,
        script: { id: "keeper", name: "keeper", command: "sleep 999", source: "t3.json" },
        workspace_id: "w1",
        tab_id: null,
        cwd: worktree,
        command_line: "sleep 999",
        script_path: null,
        shell_family: null,
        shell_source: null,
        started_at: "2026-01-01T00:00:00.000Z",
        finished_at: null,
        exit_code: null,
        result: "running" as const,
        host: null,
        session_key: null,
        stop_requested_at: null,
        log_path: null,
        error: null,
      };
      await updateRuns((state) => {
        state.runs["run-stop-me"] = {
          ...baseRun,
          run_id: "run-stop-me",
          repo: { root: worktree, remote: null },
          pane_id: "pane-stop-me",
        };
        state.runs["run-other-wt"] = {
          ...baseRun,
          run_id: "run-other-wt",
          repo: { root: wt2, remote: null },
          pane_id: "pane-stop-other-wt",
        };
        state.runs["run-no-flag"] = {
          ...baseRun,
          run_id: "run-no-flag",
          script: { id: "no-flag", name: "no-flag", command: "sleep 999", source: "t3.json" },
          repo: { root: worktree, remote: null },
          pane_id: "pane-no-flag",
        };
      });

      await runInbox(fx, ["settle", "w1:p1"]);
      const tick = await runProjects(fx, ["scripts", "tick"]);
      expect(tick.status).toBe(0);
      const triggers = tick.json?.triggers as {
        fired: unknown[];
        stopped?: Array<{ run_id: string; action: string; transition_seq: number }>;
      };
      // runOnSettle script also fires on the same transition.
      expect(triggers.fired).toHaveLength(1);
      expect(triggers.stopped).toHaveLength(1);
      expect(triggers.stopped?.[0]).toMatchObject({
        run_id: "run-stop-me",
        action: "stopped",
        transition_seq: 1,
      });

      // Only the flagged same-worktree run's pane was touched.
      expect(fx.keyedPanes).toEqual(["pane-stop-me"]);
      expect(fx.closedPanes).toContain("pane-stop-me");
      expect(fx.closedPanes).not.toContain("pane-stop-other-wt");
      expect(fx.closedPanes).not.toContain("pane-no-flag");

      const runs = await loadRuns();
      expect(runs.runs["run-stop-me"]?.result).toBe("stopped");
      expect(runs.runs["run-other-wt"]?.result).toBe("running");
      expect(runs.runs["run-no-flag"]?.result).toBe("running");
    } finally {
      await fx.close();
    }
  }, 30_000);

  test("transition_seq dedupe: a run already fired for the seq is skipped", async () => {
    const { repo, worktree } = repoWithWorktree([
      { name: "on-settle", command: "echo settle-ran", runOnSettle: true },
    ]);
    const fx = await fakeHerdr({
      panes: [{ pane_id: "w1:p1", workspace_id: "w1", cwd: worktree }],
      agents: [{ pane_id: "w1:p1", agent_status: "done", state_change_seq: 1, cwd: worktree }],
    });
    try {
      await runProjects(fx, ["register", repo]);
      process.env.HERDR_PLUGIN_STATE_DIR = fx.projStateDir;
      process.env.HERDR_PLUGIN_CONFIG_DIR = fx.projConfigDir;
      // Pre-seed a run record claiming transition_seq 1 already fired
      // (simulates crash-between-execute-and-cursor-advance replay).
      await updateRuns((state) => {
        state.runs["run-already"] = {
          run_id: "run-already",
          project_id: "ghost",
          script: { id: "x", name: "x", command: "echo x", source: "t3.json" },
          repo: { root: worktree, remote: null },
          pane_id: "pane-gone",
          workspace_id: "w1",
          tab_id: null,
          cwd: worktree,
          command_line: "echo x",
          script_path: null,
          shell_family: null,
          shell_source: null,
          started_at: "2026-01-01T00:00:00.000Z",
          finished_at: "2026-01-01T00:00:01.000Z",
          exit_code: 0,
          result: "success",
          host: null,
          session_key: null,
          stop_requested_at: null,
          log_path: null,
          error: null,
          trigger: {
            kind: "inbox.settled",
            pane_id: "w1:p1",
            transition_seq: 1,
            transition_at: "2026-01-01T00:00:00.000Z",
          },
        };
      });

      await runInbox(fx, ["settle", "w1:p1"]);
      const tick = await runProjects(fx, ["scripts", "tick"]);
      expect(tick.status).toBe(0);
      const triggers = tick.json?.triggers as {
        fired: unknown[];
        skipped: Array<{ reason: string }>;
      };
      expect(triggers.fired).toHaveLength(0);
      expect(triggers.skipped[0]?.reason).toBe("already_fired");
      expect(fx.createdCwds).toHaveLength(0);
    } finally {
      await fx.close();
    }
  }, 30_000);

  test("settle drain without explicit/ambient target → target_required, cursor held (O2)", async () => {
    const { repo, worktree } = repoWithWorktree([
      { name: "on-settle", command: "echo settle-ran", runOnSettle: true },
    ]);
    // Fake herdr bound at the XDG default socket so env-less invocations
    // resolve the implicit default session — the exact fallback the
    // mutation gate must reject.
    const fx = await fakeHerdr({
      defaultSocket: true,
      panes: [{ pane_id: "w1:p1", workspace_id: "w1", cwd: worktree }],
      agents: [{ pane_id: "w1:p1", agent_status: "done", state_change_seq: 1, cwd: worktree }],
    });
    try {
      const defaultSock = fx.socketPath;
      // register is a mutation: give it an explicit ambient target.
      const reg = await runProc(CLI, ["register", repo], {
        ...projectsEnvNoTarget(fx),
        HERDR_SOCKET_PATH: defaultSock,
      });
      expect(reg.status).toBe(0);

      // Record the settle transition against the DEFAULT session: the
      // inbox CLI gets the default socket path as an explicit env target,
      // canonicalizing to the same session_key an env-less resolution
      // yields for the drain's read side.
      const settle = await runProc(INBOX_CLI, ["settle", "w1:p1"], {
        ...projectsEnvNoTarget(fx),
        HERDR_SOCKET_PATH: defaultSock,
        HERDR_PLUGIN_STATE_DIR: fx.inboxStateDir,
        HERDR_PLUGIN_CONFIG_DIR: fx.inboxConfigDir,
      });
      expect(settle.status).toBe(0);

      // Env-less `scripts results` — a read-ish command that lazily
      // drains: the transition-log READ resolves the implicit default
      // socket fine, but the settle fire is a herdr mutation, so it is
      // reported target_required, the cursor holds, and nothing executes.
      const results = await runProc(CLI, ["scripts", "results"], projectsEnvNoTarget(fx));
      expect(results.status).toBe(0);
      const triggers = results.json?.triggers as {
        fired: unknown[];
        skipped: Array<{ reason: string; transition_seq?: number }>;
        settle?: { cursor_before: number; cursor_after: number };
      };
      expect(triggers.fired).toHaveLength(0);
      expect(triggers.skipped.map((s) => s.reason)).toEqual(["target_required"]);
      expect(triggers.skipped[0]?.transition_seq).toBe(1);
      expect(triggers.settle?.cursor_before).toBe(0);
      expect(triggers.settle?.cursor_after).toBe(0);
      expect(fx.createdCwds).toHaveLength(0);
      expect(fx.inputs).toHaveLength(0);

      // A later explicitly-targeted invocation still drains the held cursor.
      const tick = await runProc(
        CLI,
        ["scripts", "tick", "--socket", defaultSock],
        projectsEnvNoTarget(fx),
      );
      expect(tick.status).toBe(0);
      const fired = tick.json?.triggers as { fired: Array<Record<string, unknown>> };
      expect(fired.fired).toHaveLength(1);
      expect(fx.createdCwds).toEqual([worktree]);
    } finally {
      await fx.close();
    }
  }, 30_000);
});

describe("inbox source resolution — pre-#291 TS-only checkout", () => {
  test("resolveInboxSource accepts src/cli.ts when bin/inbox is absent", async () => {
    const root = tempDir();
    mkdirSync(path.join(root, "src"), { recursive: true });
    writeFileSync(path.join(root, "herdr-plugin.toml"), 'id = "rebuildup.inbox"\n');
    writeFileSync(path.join(root, "src", "cli.ts"), "// pre-#291 TS entrypoint stub\n");
    const resolved = await resolveInboxSource(root);
    expect(resolved.ok).toBe(true);
    if (resolved.ok) {
      expect(resolved.source.cli_path).toBe(path.join(root, "src", "cli.ts"));
    }
    // A root with neither entrypoint still fails closed.
    const empty = tempDir();
    const rejected = await resolveInboxSource(empty);
    expect(rejected.ok).toBe(false);
  });
});
