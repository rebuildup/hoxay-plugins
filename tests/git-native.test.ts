import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { gitBinary } from "./helpers/git-bin";
import { inboxPluginRootFixture } from "./helpers/inbox-bin";
import { installStub } from "./helpers/stub-bin";
import { herdrSocketEndpoint, joinPathEnv, testEnv } from "./helpers/test-env";

/**
 * Contract tests for the NATIVE rebuildup.git plugin (Issue #292,
 * ADR-0035). The binary under test is resolved by {@link gitBinary}:
 * committed `plugins/git/bin/git` prebuilt, then cargo
 * `target/{debug,release}` outputs.
 *
 * Coverage mirrors the retired TS suites at the public-CLI boundary:
 * strict usage errors, capability shape, pane-context git reads on a
 * real fixture repo, forge reads through a stub `gh`, honest failure
 * kinds (`gh_missing`, `pane_not_found`, `not_a_repo`), and the
 * non-TTY `tui` fallback.
 */

// ---------------------------------------------------------------------------
// CLI usage errors — validation happens before any socket/git access.
// ---------------------------------------------------------------------------

function runSync(
  args: string[],
  env?: Record<string, string>,
): {
  status: number;
  stdout: string;
  stderr: string;
  error: { code?: string; kind?: string; message?: string } | null;
} {
  const result = spawnSync(gitBinary(), args, {
    encoding: "utf8",
    env: env ? testEnv(env) : testEnv(),
    timeout: 30_000,
  });
  let error: ReturnType<typeof runSync>["error"] = null;
  try {
    error = (JSON.parse(result.stderr) as { error?: NonNullable<typeof error> }).error ?? null;
  } catch {
    /* non-JSON stderr */
  }
  return {
    status: result.status ?? -1,
    stdout: result.stdout ?? "",
    stderr: result.stderr ?? "",
    error,
  };
}

interface CliRun {
  status: number;
  stdout: string;
  stderr: string;
  error: { code?: string; kind?: string; message?: string } | null;
}

async function runCli(args: string[], env: Record<string, string>): Promise<CliRun> {
  const childEnv = testEnv(env);
  const result = await new Promise<{
    status: number | null;
    stdout: string;
    stderr: string;
  }>((resolve) => {
    const child = spawn(gitBinary(), args, { env: childEnv });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (c: Buffer) => {
      stdout += c.toString("utf8");
    });
    child.stderr.on("data", (c: Buffer) => {
      stderr += c.toString("utf8");
    });
    const timer = setTimeout(() => child.kill("SIGKILL"), 60_000);
    child.on("close", (code) => {
      clearTimeout(timer);
      resolve({ status: code, stdout, stderr });
    });
  });
  let error: CliRun["error"] = null;
  try {
    const parsed = JSON.parse(result.stderr) as { error?: CliRun["error"] };
    error = parsed.error ?? null;
  } catch {
    /* non-JSON stderr */
  }
  return {
    status: result.status ?? -1,
    stdout: result.stdout,
    stderr: result.stderr,
    error,
  };
}

function expectUsage(args: string[]): void {
  const { status, error } = runSync(args);
  expect(status).toBe(2);
  expect(error?.code).toBe("usage");
}

describe("git native CLI: usage errors", () => {
  test("every command rejects unknown options with exit 2", () => {
    const cases: string[][] = [
      ["status", "p1", "--bogus"],
      ["files", "p1", "--bogus"],
      ["diff", "p1", "--bogus"],
      ["log", "p1", "--bogus"],
      ["compare", "p1", "--bogus"],
      ["stage", "p1", "--all", "--bogus"],
      ["unstage", "p1", "--path", "x", "--bogus"],
      ["commit", "p1", "-m", "hi", "--bogus"],
      ["branch", "create", "p1", "n", "--bogus"],
      ["branch", "switch", "p1", "n", "--bogus"],
      ["upstream", "p1", "ref", "--bogus"],
      ["fetch", "p1", "--bogus"],
      ["push", "p1", "--bogus"],
      ["remote", "p1", "--bogus"],
      ["pr", "list", "p1", "--bogus"],
      ["pr", "status", "p1", "--bogus"],
      ["pr", "view", "p1", "1", "--bogus"],
      ["pr", "create", "p1", "--title", "t", "--bogus"],
      ["issue", "view", "p1", "1", "--bogus"],
      ["issue", "create", "p1", "--title", "t", "--bogus"],
      ["project", "p1", "--bogus"],
      ["capability", "--bogus"],
    ];
    for (const args of cases) expectUsage(args);
  }, 60_000);

  test("--flag=value on boolean flags and bare --name= are usage errors", () => {
    expectUsage(["push", "p1", "--set-upstream=yes"]);
    expectUsage(["diff", "p1", "--staged=1"]);
    expectUsage(["status", "p1", "--bogus=1"]);
  }, 60_000);

  test("value flags require a value; globals are validated too", () => {
    expectUsage(["files", "p1", "--path"]);
    expectUsage(["commit", "p1", "-m"]);
    expectUsage(["diff", "p1", "--max-bytes"]);
    expectUsage(["status", "p1", "--timeout"]);
    expectUsage(["status", "p1", "--timeout", "abc"]);
    expectUsage(["status", "p1", "--timeout=-5"]);
  }, 60_000);

  test("extra positionals are usage errors", () => {
    expectUsage(["status", "p1", "p2"]);
    expectUsage(["remote", "p1", "extra"]);
    expectUsage(["fetch", "p1", "r1", "r2"]);
    expectUsage(["push", "p1", "r1", "r2", "r3"]);
    expectUsage(["compare", "p1", "a", "b", "c"]);
    expectUsage(["diff", "p1", "a", "b", "c"]);
    expectUsage(["log", "p1", "p2"]);
    expectUsage(["capability", "extra"]);
  }, 60_000);

  test("stage/unstage require --all or --path; commit requires -m", () => {
    expectUsage(["stage", "p1"]);
    expectUsage(["unstage", "p1"]);
    expectUsage(["stage", "p1", "--all", "--path", "x"]);
    expectUsage(["commit", "p1"]);
    expectUsage(["commit", "p1", "--allow-empty"]);
    expectUsage(["commit", "p1", "-m", ""]);
  }, 60_000);

  test("pr and issue validate action, flags, and required args", () => {
    expectUsage(["pr"]);
    expectUsage(["pr", "bogus", "p1"]);
    expectUsage(["pr", "list", "p1", "--title", "t"]);
    expectUsage(["pr", "view", "p1"]);
    expectUsage(["pr", "view", "p1", "abc"]);
    expectUsage(["pr", "list", "p1", "--state", "bogus"]);
    expectUsage(["pr", "list", "p1", "--limit", "0"]);
    expectUsage(["pr", "create", "p1"]);
    expectUsage(["pr", "create", "p1", "--title", "t", "--body", "b", "--body-file", "f"]);
    expectUsage(["pr", "create", "p1", "--title", "t", "--repo", "not-a-repo"]);
    expectUsage(["issue"]);
    expectUsage(["issue", "view", "p1", "abc"]);
    expectUsage(["issue", "create", "p1"]);
    expectUsage(["issue", "edit", "p1", "1"]);
    expectUsage(["issue", "close", "p1", "1", "--reason", "bogus"]);
    expectUsage(["issue", "link", "p1", "owner/repo#0"]);
    expectUsage(["issue", "unlink", "p1", "owner/repo"]);
    expectUsage(["issue", "links", "p1", "--verify"]);
  }, 60_000);
});

// ---------------------------------------------------------------------------
// capability + honest failures
// ---------------------------------------------------------------------------

describe("git native CLI: capability and failures", () => {
  test("capability returns the git/v1 contract document", () => {
    const { status, stdout, error } = runSync(["capability"]);
    expect(status).toBe(0);
    expect(error).toBeNull();
    const doc = JSON.parse(stdout) as {
      plugin: string;
      version: string;
      commands: string[];
      surfaces?: { tui?: boolean };
    };
    expect(doc.plugin).toBe("rebuildup.git");
    expect(doc.version).toBe("0.7.0");
    for (const cmd of [
      "status",
      "files",
      "diff",
      "log",
      "compare",
      "stage",
      "unstage",
      "commit",
      "branch",
      "upstream",
      "fetch",
      "push",
      "remote",
      "pr",
      "issue",
      "project",
    ]) {
      expect(doc.commands).toContain(cmd);
    }
  });

  test("unknown pane is pane_not_found, not a crash", async () => {
    // Deterministic across platforms: a fake herdr socket whose snapshot
    // contains no w9:p9. (Without a socket env the CLI resolves the
    // DEFAULT herdr socket — on a dev machine that is the live session,
    // on CI it is absent; both outcomes are environment-dependent.)
    const dir = mkdtempSync(
      path.join(process.platform === "win32" ? tmpdir() : "/tmp", "git-native-pane-"),
    );
    const xdg = path.join(dir, "xdg");
    const sockPath = path.join(xdg, "herdr", "sessions", "aaa", "herdr.sock");
    mkdirSync(path.dirname(sockPath), { recursive: true });
    const snap = snapshot(dir);
    (snap.panes as unknown[]).length = 0;
    const server = await startFakeHerdr(sockPath, snap);
    try {
      const { status, error } = await runCli(["status", "w9:p9", "--session", "aaa"], {
        XDG_CONFIG_HOME: xdg,
      });
      expect(status).toBe(1);
      expect(error?.kind ?? error?.code).toBe("pane_not_found");
    } finally {
      await server.close();
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("pane outside a git repo is not_a_repo", async () => {
    // Minimal fake herdr with a pane whose cwd is a plain temp dir.
    const dir = mkdtempSync(
      path.join(process.platform === "win32" ? tmpdir() : "/tmp", "git-native-norepo-"),
    );
    const xdg = path.join(dir, "xdg");
    const sockPath = path.join(xdg, "herdr", "sessions", "aaa", "herdr.sock");
    mkdirSync(path.dirname(sockPath), { recursive: true });
    const server = net.createServer((conn) => {
      let buf = "";
      conn.on("data", (chunk: Buffer) => {
        buf += chunk.toString("utf8");
        const nl = buf.indexOf("\n");
        if (nl === -1) return;
        const msg = JSON.parse(buf.slice(0, nl)) as {
          id: string;
          method: string;
        };
        const result =
          msg.method === "session.snapshot"
            ? {
                type: "session_snapshot",
                snapshot: {
                  session: { name: "aaa" },
                  workspaces: [{ workspace_id: "w1", title: "aaa" }],
                  tabs: [{ tab_id: "t1", workspace_id: "w1" }],
                  panes: [
                    {
                      pane_id: "w1:p1",
                      workspace_id: "w1",
                      tab_id: "t1",
                      cwd: dir,
                      title: "x",
                    },
                  ],
                  agents: [],
                },
              }
            : { type: "ok" };
        conn.write(`${JSON.stringify({ id: msg.id, result })}\n`);
      });
    });
    await new Promise<void>((resolve, reject) => {
      server.once("error", reject);
      server.listen(herdrSocketEndpoint(sockPath), () => resolve());
    });
    try {
      const { status, error } = await runCli(["status", "w1:p1", "--session", "aaa"], {
        XDG_CONFIG_HOME: xdg,
      });
      expect(status).toBe(1);
      expect(error?.kind ?? error?.code).toBe("not_a_repo");
    } finally {
      await new Promise<void>((r) => server.close(() => r()));
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("forge commands report gh_missing when gh is absent", async () => {
    const dir = mkdtempSync(
      path.join(process.platform === "win32" ? tmpdir() : "/tmp", "git-native-ghmissing-"),
    );
    const xdg = path.join(dir, "xdg");
    const repoDir = path.join(dir, "repo");
    mkdirSync(repoDir, { recursive: true });
    const git = (...a: string[]) => spawnSync("git", a, { cwd: repoDir, encoding: "utf8" });
    git("init", "-q", "-b", "main");
    git("config", "user.email", "t@t");
    git("config", "user.name", "t");
    writeFileSync(path.join(repoDir, "a.txt"), "one\n");
    git("add", "a.txt");
    git("commit", "-qm", "init");

    const sockPath = path.join(xdg, "herdr", "sessions", "aaa", "herdr.sock");
    mkdirSync(path.dirname(sockPath), { recursive: true });
    const binDir = path.join(dir, "empty-bin");
    mkdirSync(binDir, { recursive: true });
    // PATH must contain git (repo resolution) but NOT gh. Link the real
    // git binary into the isolated bin dir and use it as the whole PATH.
    const whichGit = spawnSync("which", ["git"], { encoding: "utf8" });
    const gitPath = (whichGit.stdout ?? "").trim();
    if (gitPath) {
      const { symlinkSync } = await import("node:fs");
      symlinkSync(gitPath, path.join(binDir, "git"));
    }
    const server = net.createServer((conn) => {
      let buf = "";
      conn.on("data", (chunk: Buffer) => {
        buf += chunk.toString("utf8");
        const nl = buf.indexOf("\n");
        if (nl === -1) return;
        const msg = JSON.parse(buf.slice(0, nl)) as {
          id: string;
          method: string;
        };
        const result =
          msg.method === "session.snapshot"
            ? {
                type: "session_snapshot",
                snapshot: {
                  session: { name: "aaa" },
                  workspaces: [{ workspace_id: "w1", title: "aaa" }],
                  tabs: [{ tab_id: "t1", workspace_id: "w1" }],
                  panes: [
                    {
                      pane_id: "w1:p1",
                      workspace_id: "w1",
                      tab_id: "t1",
                      cwd: repoDir,
                      title: "x",
                    },
                  ],
                  agents: [],
                },
              }
            : { type: "ok" };
        conn.write(`${JSON.stringify({ id: msg.id, result })}\n`);
      });
    });
    await new Promise<void>((resolve, reject) => {
      server.once("error", reject);
      server.listen(herdrSocketEndpoint(sockPath), () => resolve());
    });
    try {
      // PATH carries git + coreutils but no gh.
      const { status, error } = await runCli(["pr", "list", "w1:p1", "--session", "aaa"], {
        XDG_CONFIG_HOME: xdg,
        PATH: binDir,
      });
      expect(status).toBe(1);
      expect(error?.kind ?? error?.code).toBe("gh_missing");
    } finally {
      await new Promise<void>((r) => server.close(() => r()));
      rmSync(dir, { recursive: true, force: true });
    }
  });
});

// ---------------------------------------------------------------------------
// git domain reads against a real fixture repo through the fake herdr socket
// ---------------------------------------------------------------------------

interface FakeHerdr {
  socketPath: string;
  close(): Promise<void>;
}

function snapshot(cwd: string): Record<string, unknown> {
  return {
    session: { name: "aaa" },
    workspaces: [{ workspace_id: "w1", title: "aaa" }],
    tabs: [{ tab_id: "t1", workspace_id: "w1" }],
    panes: [
      {
        pane_id: "w1:p1",
        workspace_id: "w1",
        tab_id: "t1",
        cwd,
        title: "work",
      },
    ],
    agents: [],
  };
}

async function startFakeHerdr(
  socketPath: string,
  snap: Record<string, unknown>,
): Promise<FakeHerdr> {
  const server = net.createServer((conn) => {
    let buf = "";
    conn.on("data", (chunk: Buffer) => {
      buf += chunk.toString("utf8");
      let nl = buf.indexOf("\n");
      while (nl !== -1) {
        const msg = JSON.parse(buf.slice(0, nl)) as {
          id: string;
          method: string;
        };
        buf = buf.slice(nl + 1);
        nl = buf.indexOf("\n");
        const result =
          msg.method === "session.snapshot"
            ? { type: "session_snapshot", snapshot: snap }
            : { type: "ok" };
        conn.write(`${JSON.stringify({ id: msg.id, result })}\n`);
      }
    });
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(herdrSocketEndpoint(socketPath), () => resolve());
  });
  return {
    socketPath,
    close: () => new Promise<void>((r) => server.close(() => r())),
  };
}

// Minimal gh stub: serves PR/issue list+view from env-pointed JSON files
// and reports the configured repo for `gh repo view`. Mutations and
// unknown subcommands exit non-zero.
const GH_STUB_POSIX = `#!/bin/sh
# gh stub for native git tests. Args: $1 $2 ...
case "$1" in
  --version) echo "gh version 2.82.0 (2025-01-01)"; exit 0;;
  auth) [ "$2" = "status" ] && exit 0 || exit 1;;
esac
cmd="$1 $2"
case "$cmd" in
  "repo view")
    printf '%s' "$GH_STUB_REPO" ;;
  "pr list")
    cat "$GH_STUB_PR_LIST" ;;
  "pr view")
    cat "$GH_STUB_PR_VIEW" ;;
  "issue list")
    cat "$GH_STUB_ISSUE_LIST" ;;
  "issue view")
    cat "$GH_STUB_ISSUE_VIEW" ;;
  *)
    echo "gh stub: unsupported $cmd" >&2
    exit 1 ;;
esac
`;

const GH_STUB_TS = `
const args = process.argv.slice(2);
const joined = args.join(" ");
const read = (env: string) => {
  const p = process.env[env];
  if (!p) return null;
  try { return require("node:fs").readFileSync(p, "utf8"); } catch { return null; }
};
const out = (s: string) => process.stdout.write(s);
if (args[0] === "--version") {
  out("gh version 2.82.0\\n");
} else if (args[0] === "auth" && args[1] === "status") {
  process.exit(0);
} else if (args[0] === "repo" && args[1] === "view") {
  out(process.env.GH_STUB_REPO ?? "octo/repo");
} else if (args[0] === "pr" && args[1] === "list") {
  const s = read("GH_STUB_PR_LIST"); if (s === null) process.exit(1); out(s);
} else if (args[0] === "pr" && args[1] === "view") {
  const s = read("GH_STUB_PR_VIEW"); if (s === null) process.exit(1); out(s);
} else if (args[0] === "issue" && args[1] === "list") {
  const s = read("GH_STUB_ISSUE_LIST"); if (s === null) process.exit(1); out(s);
} else if (args[0] === "issue" && args[1] === "view") {
  const s = read("GH_STUB_ISSUE_VIEW"); if (s === null) process.exit(1); out(s);
} else {
  process.stderr.write("gh stub: unsupported " + joined + "\\n");
  process.exit(1);
}
`;

describe("git native CLI: git reads over fake herdr", () => {
  let workdir = "";
  let xdgConfig = "";
  let repoDir = "";
  let sockPath = "";
  let ghDir = "";
  let binDir = "";
  const servers: FakeHerdr[] = [];

  const baseEnv = () => ({
    XDG_CONFIG_HOME: xdgConfig,
    XDG_STATE_HOME: path.join(workdir, "xdg-state"),
    PATH: joinPathEnv(binDir),
    GH_STUB_REPO: "octo/repo",
    GH_STUB_PR_LIST: path.join(ghDir, "pr-list.json"),
    GH_STUB_PR_VIEW: path.join(ghDir, "pr-view.json"),
    GH_STUB_ISSUE_LIST: path.join(ghDir, "issue-list.json"),
    GH_STUB_ISSUE_VIEW: path.join(ghDir, "issue-view.json"),
  });

  beforeAll(async () => {
    workdir = mkdtempSync(
      path.join(process.platform === "win32" ? tmpdir() : "/tmp", "git-native-"),
    );
    xdgConfig = path.join(workdir, "xdg-config");
    repoDir = path.join(workdir, "repo");
    ghDir = path.join(workdir, "gh");
    binDir = path.join(workdir, "bin");
    mkdirSync(repoDir, { recursive: true });
    mkdirSync(ghDir, { recursive: true });
    mkdirSync(binDir, { recursive: true });

    const git = (...a: string[]) => {
      const r = spawnSync("git", a, { cwd: repoDir, encoding: "utf8" });
      if (r.status !== 0) throw new Error(`git ${a}: ${r.stderr}`);
    };
    git("init", "-q", "-b", "main");
    git("config", "user.email", "t@t");
    git("config", "user.name", "t");
    writeFileSync(path.join(repoDir, "a.txt"), "one\ntwo\n");
    writeFileSync(path.join(repoDir, "b.txt"), "bee\n");
    git("add", ".");
    git("commit", "-qm", "init");
    writeFileSync(path.join(repoDir, "a.txt"), "one\ntwo\nchanged\n");
    // Staged binary rename: exercises the `git diff --numstat -z`
    // rename record (`-\t-\t\0old\0new\0`) — `files[].binary` must be
    // true for the NEW path.
    writeFileSync(
      path.join(repoDir, "logo.png"),
      Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x00, 0x01, 0x02, 0xff]),
    );
    git("add", "logo.png");
    git("commit", "-qm", "add logo");
    git("mv", "logo.png", "logo-renamed.png");
    // Stage c.txt only after the logo commit — committing earlier would
    // sweep the staged c.txt into "add logo" and leave it clean.
    writeFileSync(path.join(repoDir, "c.txt"), "new\n");
    git("add", "c.txt");

    installStub(binDir, { name: "gh", posix: GH_STUB_POSIX, windows: GH_STUB_TS });

    writeFileSync(
      path.join(ghDir, "pr-list.json"),
      JSON.stringify([
        {
          number: 7,
          title: "Add feature",
          state: "OPEN",
          url: "https://github.com/octo/repo/pull/7",
          headRefName: "feat/x",
          baseRefName: "main",
          isDraft: false,
          updatedAt: "2025-01-01T00:00:00Z",
        },
      ]),
    );
    writeFileSync(
      path.join(ghDir, "pr-view.json"),
      JSON.stringify({
        number: 7,
        title: "Add feature",
        state: "OPEN",
        url: "https://github.com/octo/repo/pull/7",
        body: "body",
        headRefName: "feat/x",
        headRefOid: "abc123",
        baseRefName: "main",
        isDraft: false,
        mergeable: "MERGEABLE",
        updatedAt: "2025-01-01T00:00:00Z",
        author: { login: "octo" },
        reviews: [],
        comments: [],
      }),
    );
    writeFileSync(
      path.join(ghDir, "issue-list.json"),
      JSON.stringify([
        {
          number: 5,
          title: "bug",
          state: "OPEN",
          url: "https://github.com/octo/repo/issues/5",
          updatedAt: "2025-01-01T00:00:00Z",
        },
      ]),
    );
    writeFileSync(
      path.join(ghDir, "issue-view.json"),
      JSON.stringify({
        number: 5,
        title: "bug",
        state: "OPEN",
        url: "https://github.com/octo/repo/issues/5",
        body: "b",
        updatedAt: "2025-01-01T00:00:00Z",
        author: { login: "octo" },
        comments: [],
      }),
    );

    sockPath = path.join(xdgConfig, "herdr", "sessions", "aaa", "herdr.sock");
    mkdirSync(path.dirname(sockPath), { recursive: true });
    servers.push(await startFakeHerdr(sockPath, snapshot(repoDir)));
  });

  afterAll(async () => {
    for (const s of servers) await s.close();
    rmSync(workdir, { recursive: true, force: true });
  });

  const run = (args: string[]) => runCli([...args, "--session", "aaa"], baseEnv());

  test("status reports branch and change counts", async () => {
    const { status, stdout, error } = await run(["status", "w1:p1"]);
    expect(status).toBe(0);
    expect(error).toBeNull();
    const doc = JSON.parse(stdout) as {
      branch: { name: string };
      changes: { staged: number; unstaged: number; untracked: number };
    };
    expect(doc.branch.name).toBe("main");
    // a.txt modified, c.txt staged-new → counts are non-negative and
    // reflect the fixture's dirty state honestly.
    expect(doc.changes.staged).toBeGreaterThanOrEqual(1);
    expect(doc.changes.unstaged).toBeGreaterThanOrEqual(1);
  });

  test("files lists changed paths with statuses", async () => {
    const { status, stdout } = await run(["files", "w1:p1"]);
    expect(status).toBe(0);
    const doc = JSON.parse(stdout) as {
      files?: Array<{ path: string; status: string }>;
      changes?: Array<{ path: string; status: string }>;
    };
    const arr = doc.files ?? doc.changes ?? [];
    const paths = arr.map((f) => f.path);
    expect(paths).toContain("a.txt");
    expect(paths).toContain("c.txt");
  });

  test("files marks a renamed binary path binary:true (numstat -z rename)", async () => {
    const { status, stdout } = await run(["files", "w1:p1"]);
    expect(status).toBe(0);
    const doc = JSON.parse(stdout) as {
      files?: Array<{
        path: string;
        oldPath?: string | null;
        kind?: string;
        binary?: boolean | null;
      }>;
    };
    const entry = (doc.files ?? []).find((f) => f.path === "logo-renamed.png");
    expect(entry).toBeDefined();
    expect(entry?.oldPath).toBe("logo.png");
    expect(entry?.kind).toBe("renamed");
    expect(entry?.binary).toBe(true);
  });

  test("log returns bounded commit list", async () => {
    const { status, stdout } = await run(["log", "w1:p1"]);
    expect(status).toBe(0);
    const doc = JSON.parse(stdout) as { commits?: Array<{ subject: string }> };
    expect(doc.commits?.some((c) => c.subject.includes("init"))).toBe(true);
  });

  test("diff returns a bounded patch for unstaged changes", async () => {
    const { status, stdout } = await run(["diff", "w1:p1"]);
    expect(status).toBe(0);
    expect(stdout).toContain("a.txt");
    expect(stdout).toContain("changed");
  });

  test("compare lists commits between base and head", async () => {
    const { status, stdout } = await run(["compare", "w1:p1", "HEAD", "HEAD"]);
    expect(status).toBe(0);
    const doc = JSON.parse(stdout) as { ahead: number; behind: number };
    expect(doc.ahead).toBe(0);
    expect(doc.behind).toBe(0);
  });

  test("pr list delegates to gh and returns PR rows", async () => {
    const { status, stdout, error } = await run(["pr", "list", "w1:p1", "--repo", "octo/repo"]);
    expect(status).toBe(0);
    expect(error).toBeNull();
    const doc = JSON.parse(stdout) as {
      prs?: Array<{ number: number; title: string }>;
      pull_requests?: Array<{ number: number; title: string }>;
    };
    const rows = doc.prs ?? doc.pull_requests ?? [];
    expect(rows.some((r) => r.number === 7 && r.title === "Add feature")).toBe(true);
  });

  test("pr view returns the bounded PR detail", async () => {
    const { status, stdout } = await run(["pr", "view", "w1:p1", "7", "--repo", "octo/repo"]);
    expect(status).toBe(0);
    const doc = JSON.parse(stdout) as { pr?: { number: number; title: string } };
    expect(doc.pr?.number ?? (doc as { number?: number }).number).toBe(7);
  });

  test("issue list and view delegate to gh", async () => {
    const list = await run(["issue", "list", "w1:p1", "--repo", "octo/repo"]);
    expect(list.status).toBe(0);
    const listDoc = JSON.parse(list.stdout) as {
      issues?: Array<{ number: number; title: string }>;
    };
    expect(listDoc.issues?.some((i) => i.number === 5)).toBe(true);

    const view = await run(["issue", "view", "w1:p1", "5", "--repo", "octo/repo"]);
    expect(view.status).toBe(0);
    const viewDoc = JSON.parse(view.stdout) as {
      issue?: { number: number; title: string };
    };
    expect(viewDoc.issue?.number ?? (viewDoc as { number?: number }).number).toBe(5);
  }, 30_000);

  test("stage/commit mutate the real repo and round-trip through status", async () => {
    const staged = await run(["stage", "w1:p1", "--path", "a.txt"]);
    expect(staged.status).toBe(0);
    const commit = await run(["commit", "w1:p1", "-m", "native commit"]);
    expect(commit.status).toBe(0);
    const { stdout } = await run(["log", "w1:p1"]);
    expect(stdout).toContain("native commit");
  }, 30_000);

  test("tui on non-TTY stdin prints plain text, not escape codes", async () => {
    const result = await new Promise<{
      status: number | null;
      stdout: string;
    }>((resolve) => {
      const child = spawn(gitBinary(), ["tui", "main"], {
        env: testEnv(baseEnv()),
      });
      let stdout = "";
      child.stdout.on("data", (c: Buffer) => {
        stdout += c.toString("utf8");
      });
      child.stdin.on("error", () => {});
      child.stdin.end("");
      const timer = setTimeout(() => child.kill("SIGKILL"), 30_000);
      child.on("close", (code) => {
        clearTimeout(timer);
        resolve({ status: code, stdout });
      });
    });
    // Non-TTY → plain-text fallback or honest usage/error text; must not
    // hang and must not emit terminal escape sequences.
    expect(result.status).not.toBeNull();
    expect(result.stdout).not.toContain("\x1b[");
  });
});

// ---------------------------------------------------------------------------
// issue link/unlink/links delegation to rebuildup.inbox (ADR-0013 pattern)
// ---------------------------------------------------------------------------

describe("git native CLI: issue tracker delegation", () => {
  let workdir = "";
  let xdgConfig = "";
  let xdgState = "";
  let pluginConfigDir = "";
  let repoDir = "";
  let sockPath = "";
  let inboxStateFile = "";
  const servers: FakeHerdr[] = [];

  const env = () => ({
    XDG_CONFIG_HOME: xdgConfig,
    XDG_STATE_HOME: xdgState,
    HERDR_PLUGIN_CONFIG_DIR: pluginConfigDir,
  });
  const run = (args: string[]) => runCli([...args, "--session", "aaa"], env());

  beforeAll(async () => {
    workdir = mkdtempSync(
      path.join(process.platform === "win32" ? tmpdir() : "/tmp", "git-native-link-"),
    );
    xdgConfig = path.join(workdir, "xdg-config");
    xdgState = path.join(workdir, "xdg-state");
    pluginConfigDir = path.join(workdir, "plugin-config");
    repoDir = path.join(workdir, "repo");
    mkdirSync(repoDir, { recursive: true });
    mkdirSync(pluginConfigDir, { recursive: true });
    inboxStateFile = path.join(xdgState, "herdr", "plugins", "rebuildup.inbox", "state.json");
    // Delegation resolves the sibling inbox plugin root; the committed
    // prebuilt is macOS arm64 only, so point at the platform-runnable
    // fixture on Linux CI (same pattern as git-issue.test.ts / #291).
    writeFileSync(
      path.join(pluginConfigDir, "config.json"),
      `${JSON.stringify({ inboxPluginRoot: inboxPluginRootFixture() }, null, 2)}\n`,
    );

    const git = (...a: string[]) => {
      const r = spawnSync("git", a, { cwd: repoDir, encoding: "utf8" });
      if (r.status !== 0) throw new Error(`git ${a}: ${r.stderr}`);
    };
    git("init", "-q", "-b", "main");
    git("config", "user.email", "t@t");
    git("config", "user.name", "t");
    writeFileSync(path.join(repoDir, "a.txt"), "one\n");
    git("add", "a.txt");
    git("commit", "-qm", "init");

    sockPath = path.join(xdgConfig, "herdr", "sessions", "aaa", "herdr.sock");
    mkdirSync(path.dirname(sockPath), { recursive: true });
    servers.push(await startFakeHerdr(sockPath, snapshot(repoDir)));
  });

  afterAll(async () => {
    for (const s of servers) await s.close();
    rmSync(workdir, { recursive: true, force: true });
  });

  test("issue link delegates to rebuildup.inbox and records state there", async () => {
    const runResult = await run(["issue", "link", "w1:p1", "octo/widgets#5"]);
    expect(runResult.status).toBe(0);
    const doc = JSON.parse(runResult.stdout) as Record<string, unknown>;
    expect(doc.delegated).toBe("rebuildup.inbox");
    expect(doc.action).toBe("link");
    expect(existsSync(inboxStateFile)).toBe(true);
    const state = JSON.parse(readFileSync(inboxStateFile, "utf8")) as {
      sessions: Record<string, Record<string, { links?: unknown[] }>>;
    };
    const paneEntry = Object.values(state.sessions)
      .flatMap((s) => Object.values(s))
      .find((e) => (e.links ?? []).length > 0);
    expect(paneEntry?.links).toEqual([
      expect.objectContaining({ kind: "issue", repo: "octo/widgets", number: 5 }),
    ]);
  }, 60_000);

  test("issue links reads back the delegated links", async () => {
    const runResult = await run(["issue", "links", "w1:p1"]);
    expect(runResult.status).toBe(0);
    const doc = JSON.parse(runResult.stdout) as {
      action: string;
      delegated?: string;
      links?: Array<{ kind: string; ref: string }>;
    };
    expect(doc.action).toBe("links");
    expect(doc.delegated).toBe("rebuildup.inbox");
    expect(doc.links?.some((l) => l.kind === "issue" && l.ref === "octo/widgets#5")).toBe(true);
  }, 60_000);

  test("issue unlink removes the delegated link", async () => {
    const runResult = await run(["issue", "unlink", "w1:p1", "octo/widgets#5"]);
    expect(runResult.status).toBe(0);
    const doc = JSON.parse(runResult.stdout) as {
      action: string;
      removed?: { kind: string; repo: string; number: number };
    };
    expect(doc.action).toBe("unlink");
    expect(doc.removed).toMatchObject({
      kind: "issue",
      repo: "octo/widgets",
      number: 5,
    });
  }, 60_000);

  test("issue links falls back to src/cli.ts on a pre-#291 inbox checkout", async () => {
    // TS-only inbox root: herdr-plugin.toml + src/cli.ts, no bin/.
    // Mirrors the fallback coverage the retired tests/git-issue.test.ts
    // had for plugins/git/src/inbox.ts — the Rust inboxlink resolver
    // must accept the root and run_inbox_cli must fall back to
    // `bun src/cli.ts` (same semantics as plugins/projects'
    // inbox-source.ts on release-0-13-0).
    const tsOnlyRoot = path.join(workdir, "inbox-ts-only");
    mkdirSync(path.join(tsOnlyRoot, "src"), { recursive: true });
    writeFileSync(
      path.join(tsOnlyRoot, "herdr-plugin.toml"),
      'id = "rebuildup.inbox"\nname = "Inbox"\nversion = "0.0.0-test"\n',
    );
    writeFileSync(
      path.join(tsOnlyRoot, "src", "cli.ts"),
      [
        "const links = {",
        '  via: "src-cli-ts",',
        "  plugin_id: process.env.HERDR_PLUGIN_ID ?? null,",
        "};",
        "console.log(JSON.stringify({ links }));",
        "",
      ].join("\n"),
    );
    const configFile = path.join(pluginConfigDir, "config.json");
    const original = readFileSync(configFile, "utf8");
    writeFileSync(configFile, `${JSON.stringify({ inboxPluginRoot: tsOnlyRoot }, null, 2)}\n`);
    try {
      const runResult = await run(["issue", "links", "w1:p1"]);
      expect(runResult.status).toBe(0);
      const doc = JSON.parse(runResult.stdout) as {
        delegated?: string;
        links?: { via?: string; plugin_id?: string | null };
      };
      expect(doc.delegated).toBe("rebuildup.inbox");
      // Reaching the stub at all proves is_plugin_root accepted a
      // src/cli.ts-only root and run_inbox_cli took the bun fallback.
      expect(doc.links?.via).toBe("src-cli-ts");
      // run_inbox_cli pins the child's HERDR_PLUGIN_ID to the inbox id
      // after scrubbing this plugin's HERDR_PLUGIN_* env.
      expect(doc.links?.plugin_id).toBe("rebuildup.inbox");
    } finally {
      writeFileSync(configFile, original);
    }
  }, 60_000);

  test("issue link is a mutation: fails closed without a target", async () => {
    const runResult = await runCli(["issue", "link", "w1:p1", "octo/widgets#9"], env());
    expect(runResult.status).toBe(1);
    expect(runResult.error?.kind).toBe("target_required");
  }, 60_000);
});
