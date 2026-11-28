/**
 * TS↔Rust output-parity diff harness for rebuildup.git (Issue #292).
 *
 * Modeled on scripts/diff-inbox.ts (#291). The TypeScript implementation
 * was retired in the TS-removal commit, so this harness reconstructs it
 * on the fly: `git archive` extracts `plugins/git` + `packages` at the
 * pinned pre-removal commit into a temp dir, then runs BOTH
 * `bun <tmp>/plugins/git/src/cli.ts` (or `src/tui.ts` / `src/tui-forge.ts`
 * for the TUI fallback steps) and the shipped `plugins/git/bin/git`
 * against the same fake herdr socket, the same stub `gh`, and two
 * IDENTICAL git repo clones — the TS run uses pane `w1:p1` (cwd = tsRepo)
 * and the Rust run uses pane `w1:p2` (cwd = rsRepo), so mutation steps
 * cannot cross-contaminate.
 *
 * Every command's stdout (and exit code) is compared after:
 *   - sorting object keys recursively (JSON key order is not contract),
 *   - normalizing volatile fields (generated_at, timestamps, ISO dates,
 *     40-hex commit SHAs — commit timestamps differ per clone),
 *   - scrubbing tmp paths (socket dir, repo clones, state/config dirs).
 *
 * Documented intentional divergences (stripped on both sides, covered by
 * tests/git-native.test.ts instead):
 *   - `version` in capability: TS reports 0.6.0, Rust 0.7.0.
 *   - `requires` in capability: TS reports {"git":true,"bun":true}; the
 *     native binary needs no Bun, so Rust reports {"git":true}.
 *
 * Both stdout AND stderr are compared: handled errors emit a JSON
 * envelope `{contract, ok:false, error:{code,message}}` on stderr, so
 * error-kind/code/message parity is verified, not just exit codes.
 *
 * Usage:
 *   bun scripts/diff-git.ts [--ts-commit <sha>] [--keep]
 *
 * Defaults: --ts-commit = parent of the commit that deleted
 * plugins/git/src/cli.ts. Exits non-zero on any diff.
 * Documented in docs/research/git-native-rust.md.
 */

import { spawnSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { gitBinary } from "../tests/helpers/git-bin";
import { herdrSocketEndpoint, joinPathEnv, testEnv } from "../tests/helpers/test-env";

const REPO_ROOT = path.join(import.meta.dir, "..");

// --- args -----------------------------------------------------------------

const args = process.argv.slice(2);
function argValue(flag: string): string | undefined {
  const i = args.indexOf(flag);
  return i === -1 ? undefined : args[i + 1];
}
const keepTmp = args.includes("--keep");

// --- resolve TS commit ----------------------------------------------------

function git(...gitArgs: string[]): string {
  const out = spawnSync("git", gitArgs, { cwd: REPO_ROOT, encoding: "utf8" });
  if (out.status !== 0) {
    throw new Error(`git ${gitArgs.join(" ")} failed: ${out.stderr}`);
  }
  return (out.stdout ?? "").trim();
}

// The commit that deleted plugins/git/src/cli.ts; its parent still has
// the full TS implementation.
const deletionCommit = git(
  "log",
  "--diff-filter=D",
  "--format=%H",
  "-1",
  "--",
  "plugins/git/src/cli.ts",
);
const tsCommit = argValue("--ts-commit") ?? git("rev-parse", `${deletionCommit}~1`);

// --- extract TS snapshot --------------------------------------------------

const work = mkdtempSync(path.join(tmpdir(), "git-diff-"));
const tsRoot = path.join(work, "ts");
mkdirSync(tsRoot, { recursive: true });
{
  // plugins/git (TS sources) + packages/* (shared TS deps it imports:
  // herdr-client, git-core).
  const archive = spawnSync("git", ["archive", tsCommit, "plugins/git", "packages"], {
    cwd: REPO_ROOT,
    encoding: null,
    maxBuffer: 64 * 1024 * 1024,
  });
  if (archive.status !== 0) {
    throw new Error(`git archive ${tsCommit} failed: ${archive.stderr}`);
  }
  const tar = spawnSync("tar", ["-x", "-C", tsRoot], {
    input: archive.stdout as Buffer,
    encoding: null,
  });
  if (tar.status !== 0) throw new Error(`tar extract failed: ${tar.stderr}`);
}
const tsCli = path.join(tsRoot, "plugins/git/src/cli.ts");
const tsTui = path.join(tsRoot, "plugins/git/src/tui.ts");
const tsForgeTui = path.join(tsRoot, "plugins/git/src/tui-forge.ts");
const rustBin = gitBinary();

// --- fixture repos ---------------------------------------------------------
// Two byte-identical clones. Deterministic commit identity (fixed
// author/committer/date) so log/compare outputs match modulo the SHA
// normalization below.

function makeRepo(dir: string): void {
  mkdirSync(dir, { recursive: true });
  const _run = (cmd: string, a: string[]) => {
    const r = spawnSync(cmd, a, { cwd: dir, encoding: "utf8" });
    if (r.status !== 0) throw new Error(`${cmd} ${a.join(" ")} in ${dir}: ${r.stderr}`);
  };
  const env = {
    ...process.env,
    GIT_AUTHOR_NAME: "diff",
    GIT_AUTHOR_EMAIL: "diff@example.com",
    GIT_COMMITTER_NAME: "diff",
    GIT_COMMITTER_EMAIL: "diff@example.com",
    GIT_AUTHOR_DATE: "2025-01-01T00:00:00Z",
    GIT_COMMITTER_DATE: "2025-01-01T00:00:00Z",
  };
  const runEnv = (cmd: string, a: string[]) => {
    const r = spawnSync(cmd, a, { cwd: dir, encoding: "utf8", env });
    if (r.status !== 0) throw new Error(`${cmd} ${a.join(" ")} in ${dir}: ${r.stderr}`);
  };
  runEnv("git", ["init", "-q", "-b", "main"]);
  runEnv("git", ["config", "user.name", "diff"]);
  runEnv("git", ["config", "user.email", "diff@example.com"]);
  writeFileSync(path.join(dir, "a.txt"), "one\ntwo\nthree\n");
  writeFileSync(path.join(dir, "b.txt"), "bee\n");
  mkdirSync(path.join(dir, "src"), { recursive: true });
  writeFileSync(path.join(dir, "src", "mod.ts"), "export const x = 1;\n");
  runEnv("git", ["add", "."]);
  runEnv("git", ["commit", "-qm", "initial commit"]);
  writeFileSync(path.join(dir, "a.txt"), "one\ntwo\nthree\nfour\n");
  writeFileSync(path.join(dir, "c.txt"), "new file\n");
  runEnv("git", ["add", "c.txt"]);
  runEnv("git", ["commit", "-qm", "add c.txt"]);
  // Dirty state for status/files/diff: unstaged modification + untracked.
  writeFileSync(path.join(dir, "b.txt"), "bee\nchanged\n");
  writeFileSync(path.join(dir, "d.txt"), "untracked\n");
}

const tsRepo = path.join(work, "repo-ts");
const rsRepo = path.join(work, "repo-rs");
makeRepo(tsRepo);
makeRepo(rsRepo);

// --- stub gh ---------------------------------------------------------------
// Deterministic canned responses for every gh subcommand the git plugin
// invokes in the covered scenarios. Both implementations spawn the same
// stub through the same sanitized env.

const ghDir = path.join(work, "gh");
mkdirSync(ghDir, { recursive: true });
const ghFiles: Record<string, string> = {
  "repo-view": "octo/repo",
  "pr-list": JSON.stringify([
    {
      number: 7,
      title: "Add feature",
      state: "OPEN",
      url: "https://github.com/octo/repo/pull/7",
      headRefName: "feat/x",
      baseRefName: "main",
      isDraft: false,
      updatedAt: "2025-01-02T00:00:00Z",
    },
    {
      number: 8,
      title: "Fix bug",
      state: "OPEN",
      url: "https://github.com/octo/repo/pull/8",
      headRefName: "fix/y",
      baseRefName: "main",
      isDraft: true,
      updatedAt: "2025-01-03T00:00:00Z",
    },
  ]),
  "pr-view": JSON.stringify({
    number: 7,
    title: "Add feature",
    state: "OPEN",
    url: "https://github.com/octo/repo/pull/7",
    body: "body text",
    headRefName: "feat/x",
    headRefOid: "0123456789abcdef0123456789abcdef01234567",
    baseRefName: "main",
    isDraft: false,
    mergeable: "MERGEABLE",
    mergeStateStatus: "CLEAN",
    reviewDecision: "",
    updatedAt: "2025-01-02T00:00:00Z",
    author: { login: "octo" },
    reviews: [
      {
        author: { login: "rev1" },
        state: "APPROVED",
        body: "lgtm",
        submittedAt: "2025-01-02T01:00:00Z",
      },
    ],
    comments: [
      {
        author: { login: "c1" },
        body: "comment body",
        createdAt: "2025-01-02T02:00:00Z",
      },
    ],
  }),
  "issue-list": JSON.stringify([
    {
      number: 5,
      title: "bug report",
      state: "OPEN",
      url: "https://github.com/octo/repo/issues/5",
      updatedAt: "2025-01-02T00:00:00Z",
    },
  ]),
  "issue-view": JSON.stringify({
    number: 5,
    title: "bug report",
    state: "OPEN",
    url: "https://github.com/octo/repo/issues/5",
    body: "issue body",
    stateReason: "",
    updatedAt: "2025-01-02T00:00:00Z",
    createdAt: "2025-01-01T00:00:00Z",
    closedAt: null,
    author: { login: "octo" },
    assignees: [],
    labels: [{ name: "bug" }],
    milestone: null,
    comments: [{ author: { login: "c1" }, body: "hi", createdAt: "2025-01-02T03:00:00Z" }],
    reactions: [],
  }),
  api: JSON.stringify({ id: 1, body: "api comment", html_url: "https://x" }),
};
for (const [name, content] of Object.entries(ghFiles)) {
  writeFileSync(path.join(ghDir, `${name}.json`), content);
}
writeFileSync(path.join(ghDir, "issue-create-url.txt"), "https://github.com/octo/repo/issues/9\n");

const GH_STUB_POSIX = `#!/bin/sh
# gh stub for diff-git harness. Deterministic canned responses.
GH_DIR="$GH_STUB_DIR"
case "$1" in
  --version) echo "gh version 2.82.0 (2025-01-01)"; exit 0;;
  auth) [ "$2" = "status" ] && exit 0 || exit 1;;
esac
cmd="$1 $2"
case "$cmd" in
  "repo view") cat "$GH_DIR/repo-view.json" ;;
  "pr list") cat "$GH_DIR/pr-list.json" ;;
  "pr view") cat "$GH_DIR/pr-view.json" ;;
  "pr checks"|"pr status") cat "$GH_DIR/pr-list.json" ;;
  "issue list") cat "$GH_DIR/issue-list.json" ;;
  "issue view") cat "$GH_DIR/issue-view.json" ;;
  "issue create") cat "$GH_DIR/issue-create-url.txt" ;;
  "issue edit"|"issue close"|"issue comment"|"issue reopen") cat "$GH_DIR/issue-view.json" ;;
  "pr review"|"pr comment"|"pr reply"|"pr reviewers"|"pr merge") cat "$GH_DIR/api.json" ;;
  "api") cat "$GH_DIR/api.json" ;;
  *) echo "gh stub: unsupported $cmd" >&2; exit 1 ;;
esac
`;

const ghBinDir = path.join(work, "gh-bin");
mkdirSync(ghBinDir, { recursive: true });
{
  // installStub equivalent for posix-only harness use: write the shell
  // script (the harness is exercised on POSIX dev machines / Linux CI).
  const stubPath = path.join(ghBinDir, "gh");
  writeFileSync(stubPath, GH_STUB_POSIX);
  chmodSync(stubPath, 0o755);
}

// --- fake herdr socket ------------------------------------------------------
// Two panes pointing at the two repo clones: w1:p1 -> tsRepo (TS impl),
// w1:p2 -> rsRepo (Rust impl).

const sockDir = mkdtempSync(path.join(tmpdir(), "git-diff-sock-"));
const socketPath = path.join(sockDir, "herdr.sock");
const snapshot = {
  session: { name: "diff" },
  workspaces: [{ workspace_id: "w1", title: "diff" }],
  tabs: [{ tab_id: "w1:t1", workspace_id: "w1" }],
  panes: [
    { pane_id: "w1:p1", workspace_id: "w1", tab_id: "w1:t1", cwd: tsRepo, title: "ts" },
    { pane_id: "w1:p2", workspace_id: "w1", tab_id: "w1:t1", cwd: rsRepo, title: "rs" },
  ],
  agents: [],
};
const server = net.createServer((conn) => {
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
        msg.method === "session.snapshot" ? { type: "session_snapshot", snapshot } : { type: "ok" };
      conn.write(`${JSON.stringify({ id: msg.id, result })}\n`);
    }
  });
});
await new Promise<void>((res, rej) => {
  server.once("error", rej);
  server.listen(herdrSocketEndpoint(socketPath), res);
});

// --- runners ---------------------------------------------------------------

interface RunResult {
  status: number | null;
  stdout: string;
  stderr: string;
}

interface Step {
  /** argv for the Rust binary (pane w1:p2 -> rsRepo) */
  rs: string[];
  /** argv for `bun <tsScript> ...` (pane w1:p1 -> tsRepo) */
  ts?: string[];
  /** TS entry script (default cli.ts; tui fallbacks use tui.ts/tui-forge.ts) */
  tsScript?: "cli" | "tui" | "forge";
  /** top-level JSON keys stripped before compare (documented divergences) */
  strip?: string[];
  /** extra env merged into the per-impl cliEnv for this step only */
  tsEnv?: Record<string, string>;
  rsEnv?: Record<string, string>;
}

// Every read/mutation step targets its impl's pane; --socket pins the fake
// session. `--session` is unnecessary once --socket is explicit.
function both(cmdArgs: string[]): { rs: string[]; ts: string[] } {
  return {
    rs: [...cmdArgs, "w1:p2", "--socket", socketPath],
    ts: [...cmdArgs, "w1:p1", "--socket", socketPath],
  };
}
// For commands whose pane id is not the first positional after the
// subcommand (pr/issue take <pane> then action args), build explicitly.
function bothPr(args2: string[]): { rs: string[]; ts: string[] } {
  return {
    rs: ["pr", ...args2, "w1:p2", "--socket", socketPath],
    ts: ["pr", ...args2, "w1:p1", "--socket", socketPath],
  };
}
function bothIssue(args2: string[]): { rs: string[]; ts: string[] } {
  return {
    rs: ["issue", ...args2, "w1:p2", "--socket", socketPath],
    ts: ["issue", ...args2, "w1:p1", "--socket", socketPath],
  };
}

function runTs(step: Step, env: Record<string, string>): Promise<RunResult> {
  const script = step.tsScript === "tui" ? tsTui : step.tsScript === "forge" ? tsForgeTui : tsCli;
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
  const dir = mkdtempSync(path.join(tmpdir(), `git-diff-${tag}-`));
  return { stateDir: path.join(dir, "state"), configDir: path.join(dir, "config") };
}

function cliEnv(dirs: Dirs): Record<string, string> {
  return testEnv({
    HERDR_PLUGIN_STATE_DIR: dirs.stateDir,
    HERDR_PLUGIN_CONFIG_DIR: dirs.configDir,
    HERDR_SOCKET_PATH: socketPath,
    PATH: joinPathEnv(ghBinDir),
    GH_STUB_DIR: ghDir,
  });
}

// --- normalization ----------------------------------------------------------

const VOLATILE_KEYS = new Set([
  "generated_at",
  "updatedAt",
  "updated_at",
  "createdAt",
  "created_at",
  "closedAt",
  "closed_at",
  "submittedAt",
  "submitted_at",
  "mergedAt",
  "merged_at",
]);
const ISO_TS = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}/;
const SHA40 = /\b[0-9a-f]{40}\b/g;
const SHA7 = /\b[0-9a-f]{7,39}\b(?![0-9a-f])/g;

// Paths scrubbed from every compared string: the fake socket dir, the two
// repo clones (outputs embed repo.root/repo_root), the gh stub dir, and
// per-scenario state/config dirs — in both literal and canonicalized
// (/var -> /private/var) forms.
let scrubPaths: string[] = [];
function scrubPathsInit(tsDirs: Dirs, rsDirs: Dirs) {
  const candidates = [
    sockDir,
    path.dirname(socketPath),
    realpathSync(sockDir),
    tsRepo,
    rsRepo,
    realpathSync(tsRepo),
    realpathSync(rsRepo),
    ghDir,
    ghBinDir,
    tsDirs.stateDir,
    tsDirs.configDir,
    rsDirs.stateDir,
    rsDirs.configDir,
    realpathSync(path.dirname(tsDirs.stateDir)),
    realpathSync(path.dirname(rsDirs.stateDir)),
    // stderr may embed the invoked binary/script paths (spawn errors,
    // bun diagnostics).
    rustBin,
    realpathSync(rustBin),
    tsRoot,
    realpathSync(tsRoot),
  ];
  scrubPaths = [...new Set(candidates.filter(Boolean))].sort((a, b) => b.length - a.length);
}

function scrubString(s: string): string {
  let out = s;
  for (const p of scrubPaths) {
    if (p && out.includes(p)) out = out.split(p).join("<tmp>");
  }
  // Pane ids differ by design (w1:p1 -> tsRepo, w1:p2 -> rsRepo).
  return (
    out
      .split("w1:p1")
      .join("<pane>")
      .split("w1:p2")
      .join("<pane>")
      // Non-TTY fallback hint names the CLI invocation: TS prints
      // `bun run src/cli.ts <cmd>`, the shipped binary prints `git <cmd>`.
      .split("bun run src/cli.ts")
      .join("git")
  );
}

function normalize(value: unknown, key?: string, strip?: ReadonlySet<string>): unknown {
  if (typeof value === "string") {
    // pane_id echoes the argv pane (w1:p1 vs w1:p2 by design).
    if (key === "pane_id" || key === "paneId") return "<pane>";
    if (key !== undefined && VOLATILE_KEYS.has(key)) return "<volatile>";
    if (ISO_TS.test(value)) return "<volatile>";
    // Commit SHAs differ between the two clones (committer date env is
    // pinned but clone order can still differ); the surrounding structure
    // and messages are what the contract guarantees.
    return scrubString(value).replace(SHA40, "<sha>").replace(SHA7, "<sha>");
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
    return scrubString(stdout);
  }
}

// stderr carries either nothing (success) or a single-line JSON error
// envelope `{contract, ok:false, error:{...}}`; fall back to per-line
// normalization for anything else (git stderr passthrough, panics).
function normalizeStderr(stderr: string): unknown {
  const trimmed = stderr.trim();
  if (trimmed === "") return "";
  try {
    return normalize(JSON.parse(trimmed));
  } catch {
    // not a single JSON doc — normalize each line independently
    return trimmed.split("\n").map((line) => {
      const t = line.trim();
      if (t === "") return "";
      try {
        return normalize(JSON.parse(t));
      } catch {
        return scrubString(t);
      }
    });
  }
}

// --- scenarios ---------------------------------------------------------------

const SCENARIOS: { name: string; steps: Step[] }[] = [
  {
    name: "usage-errors",
    steps: [
      { rs: ["bogus"], ts: ["bogus"] },
      {
        rs: ["status", "w1:p2", "--bogus", "--socket", socketPath],
        ts: ["status", "w1:p1", "--bogus", "--socket", socketPath],
      },
      {
        rs: ["status", "w9:p9", "--socket", socketPath],
        ts: ["status", "w9:p9", "--socket", socketPath],
      },
      { rs: ["capability", "extra"], ts: ["capability", "extra"] },
    ],
  },
  {
    name: "capability",
    steps: [
      {
        rs: ["capability"],
        ts: ["capability"],
        // Intentional divergences: 0.6.0 -> 0.7.0 version bump, and
        // requires {"git","bun"} -> {"git"} (the native binary needs no
        // Bun). Covered by contract tests.
        strip: ["version", "requires"],
      },
    ],
  },
  {
    name: "git-reads",
    steps: [
      { rs: both(["status"]).rs, ts: both(["status"]).ts },
      { rs: both(["files"]).rs, ts: both(["files"]).ts },
      { rs: both(["log"]).rs, ts: both(["log"]).ts },
      { rs: both(["log", "--limit", "1"]).rs, ts: both(["log", "--limit", "1"]).ts },
      { rs: both(["diff"]).rs, ts: both(["diff"]).ts },
      { rs: both(["diff", "--stat"]).rs, ts: both(["diff", "--stat"]).ts },
      { rs: both(["diff", "--name-status"]).rs, ts: both(["diff", "--name-status"]).ts },
      { rs: both(["diff", "--numstat"]).rs, ts: both(["diff", "--numstat"]).ts },
      { rs: both(["diff", "--staged"]).rs, ts: both(["diff", "--staged"]).ts },
      { rs: both(["compare", "HEAD", "HEAD"]).rs, ts: both(["compare", "HEAD", "HEAD"]).ts },
      { rs: both(["compare", "HEAD~1", "HEAD"]).rs, ts: both(["compare", "HEAD~1", "HEAD"]).ts },
      { rs: both(["remote"]).rs, ts: both(["remote"]).ts },
    ],
  },
  {
    name: "git-mutations",
    steps: [
      // Stage/unstage/commit/branch on each impl's own clone — outputs
      // must match even though the repos are separate objects.
      { rs: both(["stage", "--path", "b.txt"]).rs, ts: both(["stage", "--path", "b.txt"]).ts },
      { rs: both(["status"]).rs, ts: both(["status"]).ts },
      {
        rs: both(["commit", "-m", "harness commit"]).rs,
        ts: both(["commit", "-m", "harness commit"]).ts,
      },
      { rs: both(["log", "--limit", "1"]).rs, ts: both(["log", "--limit", "1"]).ts },
      {
        rs: both(["branch", "create", "feature-x"]).rs,
        ts: both(["branch", "create", "feature-x"]).ts,
      },
      {
        rs: both(["branch", "switch", "feature-x"]).rs,
        ts: both(["branch", "switch", "feature-x"]).ts,
      },
      { rs: both(["status"]).rs, ts: both(["status"]).ts },
      { rs: both(["upstream", "origin/main"]).rs, ts: both(["upstream", "origin/main"]).ts },
      { rs: both(["fetch"]).rs, ts: both(["fetch"]).ts },
      { rs: both(["push"]).rs, ts: both(["push"]).ts },
    ],
  },
  {
    name: "forge-reads",
    steps: [
      {
        rs: bothPr(["list", "--repo", "octo/repo"]).rs,
        ts: bothPr(["list", "--repo", "octo/repo"]).ts,
      },
      {
        rs: bothPr(["view", "7", "--repo", "octo/repo"]).rs,
        ts: bothPr(["view", "7", "--repo", "octo/repo"]).ts,
      },
      {
        rs: bothPr(["status", "--repo", "octo/repo"]).rs,
        ts: bothPr(["status", "--repo", "octo/repo"]).ts,
      },
      {
        rs: bothIssue(["list", "--repo", "octo/repo"]).rs,
        ts: bothIssue(["list", "--repo", "octo/repo"]).ts,
      },
      {
        rs: bothIssue(["view", "5", "--repo", "octo/repo"]).rs,
        ts: bothIssue(["view", "5", "--repo", "octo/repo"]).ts,
      },
    ],
  },
  {
    name: "forge-mutations",
    steps: [
      // gh-backed mutations against the stub; --expect-head is required.
      {
        rs: [
          ...bothPr([
            "comment",
            "7",
            "--repo",
            "octo/repo",
            "--body",
            "hi",
            "--expect-head",
            "0123456789abcdef0123456789abcdef01234567",
          ]).rs,
        ],
        ts: [
          ...bothPr([
            "comment",
            "7",
            "--repo",
            "octo/repo",
            "--body",
            "hi",
            "--expect-head",
            "0123456789abcdef0123456789abcdef01234567",
          ]).ts,
        ],
      },
      {
        rs: bothIssue(["create", "--repo", "octo/repo", "--title", "new bug", "--body", "body"]).rs,
        ts: bothIssue(["create", "--repo", "octo/repo", "--title", "new bug", "--body", "body"]).ts,
      },
      {
        rs: bothIssue(["close", "5", "--repo", "octo/repo", "--reason", "completed"]).rs,
        ts: bothIssue(["close", "5", "--repo", "octo/repo", "--reason", "completed"]).ts,
      },
    ],
  },
  {
    name: "tui-fallback",
    steps: [
      // Non-TTY stdin: both print the plain-text fallback. TS ships the
      // TUI as src/tui.ts; Rust folds it into `git tui`.
      { rs: ["tui"], ts: [], tsScript: "tui" },
      // Forge TUI fallback: TS entry is `tui-forge.ts main`; Rust is
      // `git tui forge`. Both need the HERDR_TUI_* origin pins that herdr
      // sets when launching a pane entrypoint — each side pins its own
      // pane/cwd (w1:p1->tsRepo, w1:p2->rsRepo).
      {
        rs: ["tui", "forge"],
        ts: ["main"],
        tsScript: "forge",
        rsEnv: { HERDR_TUI_PANE_ID: "w1:p2", HERDR_TUI_CWD: rsRepo },
        tsEnv: { HERDR_TUI_PANE_ID: "w1:p1", HERDR_TUI_CWD: tsRepo },
      },
    ],
  },
];

// --- run --------------------------------------------------------------------

let failures = 0;
let checks = 0;

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
  const baseTsEnv = cliEnv(tsDirs);
  const baseRsEnv = cliEnv(rsDirs);

  for (let i = 0; i < scenario.steps.length; i++) {
    const step = scenario.steps[i] as Step;
    const label = `${scenario.name} #${i}: git ${step.rs.join(" ")}`;
    const strip = step.strip ? new Set(step.strip) : undefined;
    const tsEnv = { ...baseTsEnv, ...(step.tsEnv ?? {}) };
    const rsEnv = { ...baseRsEnv, ...(step.rsEnv ?? {}) };
    const [ts, rs] = await Promise.all([runTs(step, tsEnv), runRust(step, rsEnv)]);
    const tsNorm = normalize(parseJsonOrText(ts.stdout), undefined, strip);
    const rsNorm = normalize(parseJsonOrText(rs.stdout), undefined, strip);
    const tsErr = normalizeStderr(ts.stderr);
    const rsErr = normalizeStderr(rs.stderr);
    const sameStatus = ts.status === rs.status;
    const stdoutDiff = diffJson(tsNorm, rsNorm);
    const stderrDiff = diffJson(tsErr, rsErr);
    if (sameStatus && stdoutDiff === null && stderrDiff === null) {
      report(true, label);
    } else {
      report(
        false,
        label,
        `  status ts=${ts.status} rs=${rs.status}\nstdout diff:\n${stdoutDiff ?? ""}\nstderr diff:\n${stderrDiff ?? ""}`,
      );
    }
  }
}

// --- teardown ---------------------------------------------------------------

server.close();
rmSync(sockDir, { recursive: true, force: true });
if (keepTmp) {
  console.log(`kept TS snapshot at ${tsRoot}`);
} else {
  rmSync(work, { recursive: true, force: true });
}

console.log(
  `\n${checks - failures}/${checks} checks identical (ts=${tsCommit.slice(0, 8)} rust=${rustBin})`,
);
process.exit(failures === 0 ? 0 : 1);
