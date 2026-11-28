/**
 * Deterministic fixture generator for the #287 performance baseline
 * (Program #286). Generates:
 *
 *   <out>/state/herdr/plugins/rebuildup.conversations/
 *     state.json            — session registry (SessionRecord per session)
 *     events/<key>.jsonl    — normalized ConversationEvent lines
 *   <out>/transcripts/       — placeholder provider transcript files
 *   <out>/git-repo/          — git repo with --files tracked files, of which
 *                              --modified are left modified and --untracked
 *                              untracked files added after the commit
 *
 * Everything is seeded (mulberry32) so regeneration is byte-stable for a
 * given parameter set. Fixtures are NOT committed (multi-MB); regenerate
 * with:
 *
 *   bun scripts/perf/gen-fixtures.ts --out /var/tmp/np287-fixtures
 *
 * Defaults target the #287 baseline scale: ~10k conversation events across
 * 25 sessions and a ~5k-file git repository.
 */

import { mkdir, writeFile } from "node:fs/promises";
import path from "node:path";

// --- args -----------------------------------------------------------------

interface Args {
  out: string;
  events: number;
  sessions: number;
  files: number;
  modified: number;
  untracked: number;
}

function parseArgs(argv: string[]): Args {
  const args: Args = {
    out: "/var/tmp/np287-fixtures",
    events: 10_000,
    sessions: 25,
    files: 5_000,
    modified: 120,
    untracked: 60,
  };
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i];
    const next = () => {
      const v = argv[++i];
      if (v === undefined) throw new Error(`missing value for ${a}`);
      return v;
    };
    if (a === "--out") args.out = next();
    else if (a === "--events") args.events = Number(next());
    else if (a === "--sessions") args.sessions = Number(next());
    else if (a === "--files") args.files = Number(next());
    else if (a === "--modified") args.modified = Number(next());
    else if (a === "--untracked") args.untracked = Number(next());
    else throw new Error(`unknown arg: ${a}`);
  }
  return args;
}

// --- seeded rng -------------------------------------------------------------

function mulberry32(seed: number): () => number {
  let t = seed >>> 0;
  return () => {
    t = (t + 0x6d2b79f5) >>> 0;
    let r = Math.imul(t ^ (t >>> 15), 1 | t);
    r = (r + Math.imul(r ^ (r >>> 7), 61 | r)) ^ r;
    return ((r ^ (r >>> 14)) >>> 0) / 4294967296;
  };
}

const rand = mulberry32(0x287);
const pick = <T>(xs: readonly T[]): T => xs[Math.floor(rand() * xs.length)] as T;
const int = (lo: number, hi: number) => lo + Math.floor(rand() * (hi - lo + 1));

// --- conversations index -----------------------------------------------------

const PROVIDERS = [
  { id: "claude", weight: 0.6 },
  { id: "codex", weight: 0.3 },
  { id: "opencode", weight: 0.1 },
] as const;

const MODELS: Record<string, string[]> = {
  claude: ["claude-opus-4-1", "claude-sonnet-4-5", "claude-haiku-4-5"],
  codex: ["gpt-5.2-codex", "gpt-5.1-codex-mini"],
  opencode: ["anthropic/claude-sonnet-4-5", "openai/gpt-5.2"],
};

const TOOL_NAMES = [
  "Read",
  "Edit",
  "Write",
  "Bash",
  "Grep",
  "Glob",
  "Task",
  "WebFetch",
  "apply_patch",
  "shell",
] as const;

const SEARCH_TERMS = [
  "deploy pipeline",
  "database migration",
  "refactor auth module",
  "fix flaky test",
  "update dependencies",
  "review pull request",
  "performance regression",
  "error handling",
  "cache invalidation",
  "api contract",
] as const;

const LOREM =
  "The quick brown fox jumps over the lazy dog. ".repeat(8) +
  "Lorem ipsum dolor sit amet, consectetur adipiscing elit. ".repeat(4);

function identityFor(paneId: string, seq: number) {
  return {
    host: "perf-host",
    herdr_session: "np287",
    workspace_id: "w1",
    tab_id: "t1",
    pane_id: paneId,
    agent: pick(["claude", "codex", "opencode"]),
    agent_session: { agent: "claude", kind: "session", value: `sess-${int(1000, 9999)}` },
    state_change_seq: seq,
  };
}

function usageFor() {
  const input = int(200, 120_000);
  const output = int(50, 30_000);
  return {
    input_tokens: input,
    output_tokens: output,
    cache_read_input_tokens: int(0, 400_000),
    cache_creation_input_tokens: int(0, 80_000),
  };
}

async function genConversations(out: string, totalEvents: number, sessionCount: number) {
  const stateDir = path.join(out, "state", "herdr", "plugins", "rebuildup.conversations");
  const eventsDir = path.join(stateDir, "events");
  const transcriptsDir = path.join(out, "transcripts");
  await mkdir(eventsDir, { recursive: true });
  await mkdir(transcriptsDir, { recursive: true });

  // Choose provider per session by weight.
  const providers: string[] = [];
  for (let i = 0; i < sessionCount; i++) {
    const r = rand();
    let acc = 0;
    let id = "claude";
    for (const p of PROVIDERS) {
      acc += p.weight;
      if (r <= acc) {
        id = p.id;
        break;
      }
    }
    providers.push(id);
  }

  const perSession = Math.floor(totalEvents / sessionCount);
  const sessions: Record<string, unknown> = {};
  const baseTime = Date.parse("2026-10-01T00:00:00Z");

  for (let s = 0; s < sessionCount; s++) {
    const provider = providers[s] as string;
    const sessionId = `sess-${String(10000 + s * 37)}`;
    const key = `${provider}:${sessionId}`;
    const paneId = `w1:p${(s % 20) + 1}`;
    const transcriptPath = path.join(transcriptsDir, `${key.replace(/:/g, "-")}.jsonl`);
    const lines: string[] = [];
    let turnCount = 0;
    let firstAt: string | null = null;
    let lastAt: string | null = null;

    // Roughly `perSession` events laid out as turns.
    let produced = 0;
    let turn = 0;
    while (produced < perSession) {
      turn += 1;
      turnCount = turn;
      const t0 = baseTime + s * 3600_000 + turn * 47 * 60_000 + int(0, 30_000);
      const iso = new Date(t0).toISOString();
      const identity = identityFor(paneId, s + 1);
      const model = pick(MODELS[provider] ?? ["unknown"]);
      const term = pick(SEARCH_TERMS);

      const push = (ev: Record<string, unknown>) => {
        lines.push(JSON.stringify(ev));
        produced += 1;
        if (firstAt === null) firstAt = iso;
        lastAt = iso;
      };

      push({
        event_id: `${sessionId}-t${turn}-u`,
        type: "turn",
        timestamp: iso,
        identity,
        turn_index: turn,
      });
      push({
        event_id: `${sessionId}-t${turn}-um`,
        type: "user_message",
        timestamp: new Date(t0 + 500).toISOString(),
        identity,
        turn_index: turn,
        text: `Please ${term} — step ${turn} of the migration plan. ${LOREM.slice(0, int(80, 300))}`,
      });
      // 1-3 assistant/tool exchanges per turn.
      const exchanges = int(1, 3);
      for (let e = 0; e < exchanges && produced < perSession; e++) {
        const tool = rand() < 0.55;
        if (tool) {
          const toolName = pick(TOOL_NAMES);
          const tid = `${sessionId}-t${turn}-e${e}`;
          push({
            event_id: `${tid}-tc`,
            type: "tool_call",
            timestamp: new Date(t0 + 1000 + e * 2000).toISOString(),
            identity,
            turn_index: turn,
            tool_use_id: tid,
            tool_name: toolName,
            tool_input: {
              file_path: `src/module-${int(0, 40)}/file-${int(0, 99)}.ts`,
              command: term,
            },
          });
          push({
            event_id: `${tid}-tr`,
            type: "tool_result",
            timestamp: new Date(t0 + 3000 + e * 2000).toISOString(),
            identity,
            turn_index: turn,
            tool_use_id: tid,
            tool_name: toolName,
            is_error: rand() < 0.08,
            text: `tool output for ${toolName}: ${LOREM.slice(0, int(60, 240))}`,
          });
        } else {
          push({
            event_id: `${sessionId}-t${turn}-e${e}-am`,
            type: "assistant_message",
            timestamp: new Date(t0 + 1000 + e * 2000).toISOString(),
            identity,
            turn_index: turn,
            model,
            usage: usageFor(),
            text: `Working on ${term}. ${LOREM.slice(0, int(200, 900))}`,
          });
        }
      }
      if (rand() < 0.06) {
        push({
          event_id: `${sessionId}-t${turn}-sys`,
          type: "system",
          timestamp: new Date(t0 + 4000).toISOString(),
          identity,
          turn_index: turn,
          subtype: pick(["compact_boundary", "status", "permission"]),
        });
      }
    }

    const eventLines = lines.slice(0, perSession);
    const safeName = key.replace(/[^A-Za-z0-9._-]/g, "_");
    await writeFile(path.join(eventsDir, `${safeName}.jsonl`), eventLines.join("\n") + "\n");
    // Placeholder transcript so `sessions` reports transcript_present honestly.
    await writeFile(transcriptPath, eventLines.slice(0, 3).join("\n") + "\n");

    sessions[key] = {
      key,
      provider,
      session_id: sessionId,
      transcript_path: transcriptPath,
      cwd: `/Users/perf/workspace-${s % 5}`,
      status: "ok",
      error: null,
      identity: identityFor(paneId, s + 1),
      cursor: { offset: 0, size: 0, mtime_ms: 0, line_count: eventLines.length },
      guard_hash: null,
      turn_count: turnCount,
      event_count: eventLines.length,
      first_event_at: firstAt,
      last_event_at: lastAt,
      last_indexed_at: new Date(baseTime + sessionCount * 3600_000).toISOString(),
    };
  }

  await writeFile(
    path.join(stateDir, "state.json"),
    JSON.stringify({ version: 1, sessions }, null, 2) + "\n",
  );
  console.log(
    `conversations fixture: ${sessionCount} sessions, ${totalEvents} target events -> ${stateDir}`,
  );
}

// --- git repo -----------------------------------------------------------------

async function genGitRepo(repoDir: string, files: number, modified: number, untracked: number) {
  const { execFileSync } = await import("node:child_process");
  const { mkdirSync, rmSync, writeFileSync } = await import("node:fs");
  rmSync(repoDir, { recursive: true, force: true });
  mkdirSync(repoDir, { recursive: true });
  const git = (args: string[]) => execFileSync("git", args, { cwd: repoDir, stdio: "pipe" });

  git(["init", "-b", "main"]);
  git(["config", "user.email", "perf@example.com"]);
  git(["config", "user.name", "perf"]);
  git(["config", "commit.gpgsign", "false"]);

  const content = (i: number) =>
    `// generated fixture file ${i}\nexport const value${i} = ${i};\n// ${"x".repeat(120)}\n`;

  const trackedPaths: string[] = [];
  // ~5000 files across 100 module dirs.
  const perDir = Math.ceil(files / 100);
  let written = 0;
  for (let d = 0; d < 100 && written < files; d++) {
    const dir = path.join(repoDir, "src", `module-${String(d).padStart(3, "0")}`);
    mkdirSync(dir, { recursive: true });
    for (let f = 0; f < perDir && written < files; f++) {
      const p = path.join(dir, `file-${String(f).padStart(3, "0")}.ts`);
      writeFileSync(p, content(written));
      trackedPaths.push(p);
      written += 1;
    }
  }
  // A few non-source files for realism.
  writeFileSync(path.join(repoDir, "README.md"), "# perf fixture repo\n");
  writeFileSync(path.join(repoDir, "package.json"), '{"name":"perf-fixture","private":true}\n');

  git(["add", "-A"]);
  git(["commit", "-m", "initial fixture commit", "--quiet"]);

  // Leave `modified` tracked files dirty and `untracked` new files.
  for (let i = 0; i < modified && i < trackedPaths.length; i++) {
    const target = trackedPaths[(i * 37) % trackedPaths.length] as string;
    writeFileSync(target, content(9000 + i) + `// dirty edit ${i}\n`);
  }
  for (let i = 0; i < untracked; i++) {
    const dir = path.join(repoDir, "src", `module-${String(i % 100).padStart(3, "0")}`);
    mkdirSync(dir, { recursive: true });
    writeFileSync(path.join(dir, `new-file-${i}.ts`), `// untracked ${i}\n`);
  }
  console.log(
    `git fixture: ${files} tracked files, ${modified} modified, ${untracked} untracked -> ${repoDir}`,
  );
}

// --- main ----------------------------------------------------------------------

const args = parseArgs(process.argv.slice(2));
await genConversations(args.out, args.events, args.sessions);
await genGitRepo(path.join(args.out, "git-repo"), args.files, args.modified, args.untracked);
console.log(`fixtures root: ${args.out}`);
