/**
 * Repeatable timing harness for the #287 performance baseline.
 *
 * Runs a command N times as fresh processes (every invocation is already a
 * cold process start — there is no persistent runtime to warm), records
 * wall-clock latency per run, and reports p50/p95/p99 plus min/max/mean.
 *
 *   bun scripts/perf/measure.ts --runs 30 --warmup 3 --label "search" -- \
 *     env XDG_STATE_HOME=... ./plugins/conversations/bin/conversations search deploy --offline
 *
 * Options:
 *   --runs N        measured runs (default 30)
 *   --warmup N      discarded warmup runs (default 3) — also serves as the
 *                   "warm cache" steady state; the first measured run after
 *                   warmup approximates warm-process latency
 *   --label STR     label echoed into the JSON summary
 *   --rss           additionally wrap each run in `/usr/bin/time -l` and
 *                   report max RSS / user+sys CPU per run (macOS only).
 *                   Adds ~1-2 ms overhead per run — use a small --runs.
 *   --timeout MS    per-run timeout (default 120000); a timeout is recorded
 *                   as a failed run, not silently dropped.
 *
 * Output: one JSON object per run on stderr is NOT used; instead a single
 * JSON document on stdout: { label, command, runs, failures, wall_ms:{...},
 * rss_kb?: {...}, cpu_ms?: {...} }.
 *
 * "Cold" vs "warm": every run is a fresh OS process (cold start). OS page
 * cache is warm after warmup; a truly disk-cold run requires cache dropping
 * (sudo purge on macOS) which is intentionally not automated — see the
 * research doc for the cold-start discussion.
 */

import { spawn } from "node:child_process";

interface Args {
  runs: number;
  warmup: number;
  label: string;
  rss: boolean;
  timeout: number;
  command: string[];
}

function parseArgs(argv: string[]): Args {
  const out: Args = {
    runs: 30,
    warmup: 3,
    label: "cmd",
    rss: false,
    timeout: 120_000,
    command: [],
  };
  let i = 0;
  let seenSep = false;
  for (; i < argv.length; i++) {
    const a = argv[i] as string;
    if (a === "--") {
      seenSep = true;
      i++;
      break;
    }
    const next = () => argv[++i] as string;
    if (a === "--runs") out.runs = Number(next());
    else if (a === "--warmup") out.warmup = Number(next());
    else if (a === "--label") out.label = next();
    else if (a === "--rss") out.rss = true;
    else if (a === "--timeout") out.timeout = Number(next());
    else throw new Error(`unknown arg: ${a}`);
  }
  if (!seenSep || i >= argv.length) {
    throw new Error("usage: measure.ts [opts] -- <command...>");
  }
  out.command = argv.slice(i);
  return out;
}

interface RunResult {
  ms: number;
  code: number | null;
  timedOut: boolean;
  rssKb: number | null;
  cpuMs: number | null;
}

function runOnce(cmd: string[], timeoutMs: number, rss: boolean): Promise<RunResult> {
  // /usr/bin/time -l wraps the real command; its own overhead is ~1 ms.
  const full = rss ? ["/usr/bin/time", "-l", ...cmd] : cmd;
  const start = performance.now();
  return new Promise((resolve) => {
    const child = spawn(full[0] as string, full.slice(1), {
      stdio: ["ignore", "pipe", "pipe"],
      env: process.env,
    });
    let stdout = "";
    let stderr = "";
    let timedOut = false;
    const timer = setTimeout(() => {
      timedOut = true;
      child.kill("SIGKILL");
    }, timeoutMs);
    child.stdout.on("data", (c: Buffer) => {
      stdout += c.toString("utf8");
    });
    child.stderr.on("data", (c: Buffer) => {
      stderr += c.toString("utf8");
    });
    child.on("error", () => {
      clearTimeout(timer);
      resolve({ ms: performance.now() - start, code: null, timedOut, rssKb: null, cpuMs: null });
    });
    child.on("close", (code) => {
      clearTimeout(timer);
      const ms = performance.now() - start;
      let rssKb: number | null = null;
      const cpuMs: number | null = null;
      if (rss) {
        // macOS `/usr/bin/time -l` prints "  <bytes>  maximum resident set
        // size" (value before label, bytes); Linux time -v differs.
        const m = /(\d+)\s+maximum resident set size/.exec(stderr);
        if (m) rssKb = Math.round(Number(m[1]) / 1024);
      }
      void stdout;
      resolve({ ms, code, timedOut, rssKb, cpuMs });
    });
  });
}

function percentile(sorted: number[], p: number): number {
  if (sorted.length === 0) return NaN;
  const idx = Math.min(sorted.length - 1, Math.ceil((p / 100) * sorted.length) - 1);
  return sorted[Math.max(0, idx)] as number;
}

function stats(values: number[]) {
  const sorted = [...values].sort((a, b) => a - b);
  const mean = values.reduce((a, b) => a + b, 0) / values.length;
  const variance = values.reduce((a, b) => a + (b - mean) ** 2, 0) / values.length;
  return {
    n: values.length,
    min: Math.round(sorted[0] ?? NaN),
    p50: Math.round(percentile(sorted, 50)),
    p95: Math.round(percentile(sorted, 95)),
    p99: Math.round(percentile(sorted, 99)),
    max: Math.round(sorted[sorted.length - 1] ?? NaN),
    mean: Math.round(mean * 10) / 10,
    stdev: Math.round(Math.sqrt(variance) * 10) / 10,
  };
}

const args = parseArgs(process.argv.slice(2));

// Warmup (discarded) — also warms OS page cache for the measured runs.
for (let i = 0; i < args.warmup; i++) {
  await runOnce(args.command, args.timeout, false);
}

const wall: number[] = [];
const rss: number[] = [];
let failures = 0;
for (let i = 0; i < args.runs; i++) {
  const r = await runOnce(args.command, args.timeout, args.rss);
  if (r.timedOut || r.code !== 0) failures += 1;
  wall.push(r.ms);
  if (r.rssKb !== null) rss.push(r.rssKb);
}

const summary: Record<string, unknown> = {
  label: args.label,
  command: args.command.join(" "),
  runs: args.runs,
  warmup: args.warmup,
  failures,
  wall_ms: stats(wall),
};
if (args.rss) {
  summary.rss_kb = stats(rss);
  summary.rss_note =
    "max RSS of the top-level process only (children not attributed); /usr/bin/time -l overhead included";
}
console.log(JSON.stringify(summary, null, 2));
if (failures > 0) process.exit(1);
