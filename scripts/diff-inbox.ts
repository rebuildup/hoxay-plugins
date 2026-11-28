/**
 * TS↔Rust output-parity diff harness for rebuildup.inbox (Issue #291).
 *
 * The TypeScript implementation was retired in commit 0680de0, so this
 * harness reconstructs it on the fly: `git archive` extracts
 * `plugins/inbox` + `packages` at the pinned pre-removal commit into a
 * temp dir, then runs BOTH `bun <tmp>/plugins/inbox/src/cli.ts` (or
 * `src/tui.ts` for the TUI fallback step) and the shipped
 * `plugins/inbox/bin/inbox` against the same fake herdr socket and
 * identical fresh plugin state/config dirs.
 *
 * Every command's stdout (and exit code) is compared after:
 *   - sorting object keys recursively (JSON key order is not contract),
 *   - normalizing volatile fields (ISO timestamps, generated_at, since,
 *     until, ...),
 *   - scrubbing tmp paths (socket dir, per-scenario state/config dirs,
 *     including macOS /var -> /private/var canonicalization differences).
 * The final state.json files are compared the same way.
 *
 * Known intentional divergence: the Rust capability document adds
 * `command_metadata` (additive, #263 / contract v0.8.0) which the pinned
 * TS snapshot predates — the capability step strips that key on both
 * sides; its presence is covered by tests/inbox-native.test.ts.
 *
 * Usage:
 *   bun scripts/diff-inbox.ts [--ts-commit <sha>] [--keep]
 *
 * Defaults: --ts-commit = parent of the TS-removal commit (0680de0~1 =
 * d5d74ce). Exits non-zero on any diff.
 * Documented in docs/research/inbox-native-rust.md.
 */

import { spawnSync } from "node:child_process";
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync } from "node:fs";
import net from "node:net";
import { tmpdir } from "node:os";
import path from "node:path";
import { inboxBinary } from "../tests/helpers/inbox-bin";
import { herdrSocketEndpoint, testEnv } from "../tests/helpers/test-env";

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

const tsCommit = argValue("--ts-commit") ?? git("rev-parse", "0680de0~1");

// --- extract TS snapshot --------------------------------------------------

const work = mkdtempSync(path.join(tmpdir(), "inbox-diff-"));
const tsRoot = path.join(work, "ts");
mkdirSync(tsRoot, { recursive: true });
{
  // plugins/inbox (TS sources) + packages/* (shared TS deps it imports).
  const archive = spawnSync("git", ["archive", tsCommit, "plugins/inbox", "packages"], {
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
const tsCli = path.join(tsRoot, "plugins/inbox/src/cli.ts");
const tsTui = path.join(tsRoot, "plugins/inbox/src/tui.ts");
const rustBin = inboxBinary();

// --- fake herdr socket (same NDJSON protocol as tests/inbox-native) ------

interface FakeAgent {
  pane_id: string;
  agent_status: string;
  state_change_seq: number;
  agent_session?: { agent: string; kind: string; value: string } | null;
  cwd?: string;
  terminal_title_stripped?: string;
}
interface FakePane {
  pane_id: string;
  workspace_id: string;
  tab_id: string;
  title?: string;
  cwd?: string;
}

const AGENTS: FakeAgent[] = [
  { pane_id: "w1:p1", agent_status: "done", state_change_seq: 1 },
  { pane_id: "w1:p2", agent_status: "working", state_change_seq: 1 },
  { pane_id: "w1:p3", agent_status: "done", state_change_seq: 1 },
];
const PANES: FakePane[] = ["w1:p1", "w1:p2", "w1:p3"].map((id) => ({
  pane_id: id,
  workspace_id: "w1",
  tab_id: "w1:t1",
}));

const sockDir = mkdtempSync(path.join(tmpdir(), "inbox-diff-sock-"));
const socketPath = path.join(sockDir, "herdr.sock");
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
        msg.method === "session.snapshot"
          ? { snapshot: { session: { name: "fake" }, panes: PANES, agents: AGENTS } }
          : {};
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
  /** argv for the Rust binary */
  rs: string[];
  /** argv for `bun <tsScript> ...` — defaults to the same args via cli.ts */
  ts?: string[];
  /** TS entry script (default cli.ts; tui fallback uses tui.ts) */
  tsScript?: "cli" | "tui";
  /** top-level JSON keys stripped before compare (documented divergences) */
  strip?: string[];
}

function runTs(step: Step, env: Record<string, string>): Promise<RunResult> {
  const script = step.tsScript === "tui" ? tsTui : tsCli;
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
  const dir = mkdtempSync(path.join(tmpdir(), `inbox-diff-${tag}-`));
  return { stateDir: path.join(dir, "state"), configDir: path.join(dir, "config") };
}

function cliEnv(dirs: Dirs): Record<string, string> {
  return testEnv({
    HERDR_PLUGIN_STATE_DIR: dirs.stateDir,
    HERDR_PLUGIN_CONFIG_DIR: dirs.configDir,
    HERDR_SOCKET_PATH: socketPath,
    HERDR_WORKSPACE_ID: "w1",
    HERDR_TAB_ID: "w1:t1",
    HERDR_PANE_ID: "w1:p1",
  });
}

// --- normalization ----------------------------------------------------------

const VOLATILE_KEYS = new Set([
  "generated_at",
  "resolved_at",
  "checked_at",
  "fetched_at",
  "updated_at",
  "created_at",
  "since",
  "until",
  "at",
  "ts",
  "timestamp",
]);
const ISO_TS = /^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}/;

// Paths scrubbed from every compared string: the fake socket dir (both
// the literal and canonicalized forms — Rust canonicalizes /var ->
// /private/var for the session key) and the per-scenario state/config
// dirs (they embed absolute paths in `state_file`/`session.socket`).
let scrubPaths: string[] = [];
function scrubPathsInit(tsDirs: Dirs, rsDirs: Dirs) {
  const candidates = [
    sockDir,
    path.dirname(socketPath),
    realpathSync(sockDir),
    tsDirs.stateDir,
    tsDirs.configDir,
    rsDirs.stateDir,
    rsDirs.configDir,
    realpathSync(path.dirname(tsDirs.stateDir)),
    realpathSync(path.dirname(rsDirs.stateDir)),
  ];
  scrubPaths = [...new Set(candidates.filter(Boolean))].sort((a, b) => b.length - a.length);
}

function scrubString(s: string): string {
  let out = s;
  for (const p of scrubPaths) {
    if (p && out.includes(p)) out = out.split(p).join("<tmp>");
  }
  return out;
}

function normalize(value: unknown, key?: string, strip?: ReadonlySet<string>): unknown {
  if (typeof value === "string") {
    if (key !== undefined && VOLATILE_KEYS.has(key)) return "<volatile>";
    if (ISO_TS.test(value)) return "<volatile>";
    return scrubString(value);
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
      // session-map keys embed the socket path ("<host> <sockpath>") and
      // Rust canonicalizes /var -> /private/var — scrub keys like values.
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
    return stdout;
  }
}

// --- scenarios ---------------------------------------------------------------
// Each scenario is a sequence run against FRESH state dirs on both impls;
// every step's normalized stdout + exit code must match, and the resulting
// state.json files must match after normalization.

const SCENARIOS: { name: string; steps: Step[] }[] = [
  {
    name: "usage-errors",
    steps: [
      { rs: ["bogus"] },
      // ambient pane identity resolves w1:p1 in the fake session
      { rs: ["settle", "--socket", socketPath] },
      { rs: ["watch", "--interval", "abc", "--socket", socketPath] },
      { rs: ["snooze", "--socket", socketPath] }, // no --until, no config default
    ],
  },
  {
    name: "capability",
    steps: [
      {
        rs: ["capability", "--socket", socketPath],
        // additive in Rust v0.8.0 (#263); covered by contract tests.
        // `version` is the intentional 0.7.0 -> 0.8.0 bump.
        strip: ["command_metadata", "version"],
      },
    ],
  },
  {
    name: "organize-flow",
    steps: [
      { rs: ["list", "--socket", socketPath] },
      { rs: ["settle", "--socket", socketPath] },
      { rs: ["pin", "--socket", socketPath] },
      { rs: ["list", "--organize", "settled", "--socket", socketPath] },
      { rs: ["list", "--attention", "--socket", socketPath] },
      { rs: ["snooze", "--until", "+1h", "--socket", socketPath] },
      { rs: ["list", "--organize", "snoozed", "--socket", socketPath] },
      { rs: ["archive", "--socket", socketPath] },
      { rs: ["unarchive", "--socket", socketPath] },
      { rs: ["order", "--first", "--socket", socketPath] },
      { rs: ["unpin", "--socket", socketPath] },
      { rs: ["restore", "--socket", socketPath] },
      { rs: ["transitions", "--socket", socketPath] },
      { rs: ["reconcile", "--socket", socketPath] },
    ],
  },
  {
    name: "links-flow",
    steps: [
      { rs: ["link", "w1:p1", "--pr", "owner/repo#42", "--socket", socketPath] },
      { rs: ["links", "--socket", socketPath] },
      { rs: ["list", "--search", "repo", "--socket", socketPath] },
      { rs: ["unlink", "w1:p1", "--pr", "owner/repo#42", "--socket", socketPath] },
      { rs: ["links", "--socket", socketPath] },
    ],
  },
  {
    name: "tui-fallback",
    steps: [
      // TS ships the TUI as a separate entry (src/tui.ts); Rust folds it
      // into `inbox tui`. Non-TTY both print the plain-list fallback.
      { rs: ["tui"], ts: [], tsScript: "tui" },
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
  const tsEnv = cliEnv(tsDirs);
  const rsEnv = cliEnv(rsDirs);

  for (let i = 0; i < scenario.steps.length; i++) {
    const step = scenario.steps[i] as Step;
    const label = `${scenario.name} #${i}: inbox ${step.rs.join(" ")}`;
    const strip = step.strip ? new Set(step.strip) : undefined;
    const [ts, rs] = await Promise.all([runTs(step, tsEnv), runRust(step, rsEnv)]);
    const tsNorm = normalize(parseJsonOrText(ts.stdout), undefined, strip);
    const rsNorm = normalize(parseJsonOrText(rs.stdout), undefined, strip);
    const sameStatus = ts.status === rs.status;
    const stdoutDiff = diffJson(tsNorm, rsNorm);
    if (sameStatus && stdoutDiff === null) {
      report(true, label);
    } else {
      report(
        false,
        label,
        `  status ts=${ts.status} rs=${rs.status}\n${stdoutDiff ?? ""}\n  ts.stderr: ${ts.stderr.slice(0, 300)}\n  rs.stderr: ${rs.stderr.slice(0, 300)}`,
      );
    }
  }

  // Final state.json parity (normalized) — evidences on-disk format parity.
  const readState = (p: string): unknown => {
    try {
      return JSON.parse(readFileSync(p, "utf8"));
    } catch {
      return null;
    }
  };
  const tsState = readState(path.join(tsDirs.stateDir, "state.json"));
  const rsState = readState(path.join(rsDirs.stateDir, "state.json"));
  const stateDiff = diffJson(normalize(tsState), normalize(rsState));
  report(stateDiff === null, `${scenario.name}: final state.json parity`, stateDiff ?? undefined);
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
