/**
 * Data-source resolution + export for rebuildup.usage (ADR-0013).
 *
 * The plugin consumes `rebuildup.conversations` exclusively through its
 * public read-only CLI contract (`capability`, `usage` paging) — never
 * its state files. The conversations plugin root is resolved from:
 *   1. explicit override (`--conversations-root` / env / config),
 *   2. the sibling `../conversations` directory (repo layout and
 *      managed installs both keep `plugins/<name>` side by side),
 *   3. `herdr plugin list --json` discovery.
 *
 * The child process gets `HERDR_PLUGIN_STATE_DIR`/`_CONFIG_DIR`
 * pointed at the *conversations* plugin dirs — the ambient values
 * belong to rebuildup.usage and must never leak into the child.
 */

import { spawn } from "node:child_process";
import { stat } from "node:fs/promises";
import path from "node:path";
import {
  herdrConfigDir,
  herdrStateDir,
  pluginPathComponent,
} from "../../../packages/herdr-client/src/index";
import {
  CONVERSATIONS_PLUGIN_ID,
  type SessionCoverageEntry,
  type SourceOptions,
  type SourceResolution,
  type UsageEventRecord,
} from "./model";

function asRecord(value: unknown): Record<string, unknown> | null {
  return typeof value === "object" && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : null;
}

/** This plugin's own root (plugins/usage). */
export function pluginRoot(): string {
  const envRoot = process.env.HERDR_PLUGIN_ROOT;
  if (envRoot && envRoot !== "") return envRoot;
  // src/source.ts → plugins/usage
  return path.resolve(import.meta.dir, "..");
}

function defaultStateDir(): string {
  return path.join(herdrStateDir(), "plugins", pluginPathComponent(CONVERSATIONS_PLUGIN_ID));
}

function defaultConfigDir(): string {
  return path.join(
    herdrConfigDir(),
    "plugins",
    "config",
    pluginPathComponent(CONVERSATIONS_PLUGIN_ID),
  );
}

async function isConversationsRoot(dir: string): Promise<boolean> {
  try {
    // Native distribution (ADR-0035): the plugin ships bin/conversations;
    // the legacy Bun entrypoint src/cli.ts remains a fallback.
    const bin = await stat(path.join(dir, "bin", "conversations")).catch(() => null);
    const cli = bin?.isFile() === true ? bin : await stat(path.join(dir, "src", "cli.ts"));
    if (!cli.isFile()) return false;
    const tomlPath = path.join(dir, "herdr-plugin.toml");
    const toml = await stat(tomlPath).catch(() => null);
    if (toml === null || !toml.isFile()) return true; // cli entrypoint is enough signal
    const { readFile } = await import("node:fs/promises");
    const text = await readFile(tomlPath, "utf8");
    return text.includes(`id = "${CONVERSATIONS_PLUGIN_ID}"`);
  } catch {
    return false;
  }
}

/** Prefer the native `bin/conversations`; fall back to `src/cli.ts`. */
async function conversationsCliPath(root: string): Promise<string> {
  const bin = path.join(root, "bin", "conversations");
  const info = await stat(bin).catch(() => null);
  return info?.isFile() === true ? bin : path.join(root, "src", "cli.ts");
}

/** `herdr plugin list --json` → plugin_root for rebuildup.conversations. */
async function discoverViaHerdr(): Promise<string | null> {
  const herdrBin = process.env.HERDR_BIN_PATH || "herdr";
  const stdout = await new Promise<string | null>((resolve) => {
    const child = spawn(herdrBin, ["plugin", "list", "--json"], {
      stdio: ["ignore", "pipe", "ignore"],
      timeout: 15_000,
    });
    let out = "";
    child.stdout.on("data", (chunk: Buffer) => {
      out += chunk.toString("utf8");
    });
    child.on("error", () => resolve(null));
    child.on("close", (code) => resolve(code === 0 ? out : null));
  });
  if (stdout === null) return null;
  try {
    const parsed = JSON.parse(stdout) as unknown;
    const list = Array.isArray(parsed)
      ? parsed
      : ((parsed as { plugins?: unknown[] }).plugins ?? []);
    for (const entry of list) {
      if (typeof entry !== "object" || entry === null) continue;
      const e = entry as Record<string, unknown>;
      const id = e.plugin_id ?? e.id;
      const root = e.plugin_root ?? e.root;
      if (id === CONVERSATIONS_PLUGIN_ID && typeof root === "string" && root !== "") {
        return root;
      }
    }
  } catch {
    return null;
  }
  return null;
}

export type ResolveResult =
  | { ok: true; source: SourceResolution }
  | { ok: false; error: string; kind: string };

/**
 * Resolve the conversations plugin root + its state/config dirs.
 * Never throws; failures are structured for the CLI error envelope.
 */
export async function resolveConversationsSource(options: SourceOptions): Promise<ResolveResult> {
  const explicitRoot = options.conversationsRoot;
  let root: string | null = null;
  let resolvedVia: SourceResolution["resolved_via"] = "override";

  if (explicitRoot !== undefined && explicitRoot !== "") {
    const candidate = path.resolve(explicitRoot);
    if (await isConversationsRoot(candidate)) {
      root = candidate;
    } else {
      return {
        ok: false,
        kind: "conversations_unavailable",
        error: `--conversations-root ${candidate} is not a rebuildup.conversations plugin root`,
      };
    }
  } else {
    const sibling = path.resolve(pluginRoot(), "..", "conversations");
    if (await isConversationsRoot(sibling)) {
      root = sibling;
      resolvedVia = "sibling";
    } else {
      const discovered = await discoverViaHerdr();
      if (discovered !== null && (await isConversationsRoot(discovered))) {
        root = discovered;
        resolvedVia = "herdr-plugin-list";
      }
    }
  }

  if (root === null) {
    return {
      ok: false,
      kind: "conversations_unavailable",
      error:
        "rebuildup.conversations plugin not found; install it or pass --conversations-root / set conversationsRoot in config.json",
    };
  }

  const stateDir =
    options.conversationsStateDir !== undefined && options.conversationsStateDir !== ""
      ? path.resolve(options.conversationsStateDir)
      : defaultStateDir();
  const configDir =
    options.conversationsConfigDir !== undefined && options.conversationsConfigDir !== ""
      ? path.resolve(options.conversationsConfigDir)
      : defaultConfigDir();

  return {
    ok: true,
    source: {
      plugin_root: root,
      cli_path: await conversationsCliPath(root),
      state_dir: stateDir,
      config_dir: configDir,
      resolved_via: resolvedVia,
    },
  };
}

// --- Export paging -----------------------------------------------------------

export interface CliCallOptions {
  session?: string | undefined;
  socketPath?: string | undefined;
  timeoutMs?: number | undefined;
  offline?: boolean | undefined;
}

export interface UsageSelectors {
  pane?: string | undefined;
  sessionId?: string | undefined;
  provider?: string | undefined;
}

interface ConversationsUsagePage {
  contract?: unknown;
  command?: unknown;
  results?: unknown;
  page?: {
    next_cursor?: unknown;
    has_more?: unknown;
    malformed_lines?: unknown;
    duplicates_skipped?: unknown;
    restarted_sessions?: unknown;
  };
  sessions?: unknown;
  error?: unknown;
}

interface CliRunResult {
  status: number;
  stdout: string;
  stderr: string;
  json: Record<string, unknown> | null;
}

function runConversationsCli(
  source: SourceResolution,
  args: string[],
  call: CliCallOptions,
): Promise<CliRunResult> {
  // Child env: pin the plugin dirs to the *conversations* plugin so the
  // ambient HERDR_PLUGIN_* values (which belong to rebuildup.usage)
  // cannot redirect its state resolution.
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (value === undefined) continue;
    if (key === "HERDR_PLUGIN_STATE_DIR" || key === "HERDR_PLUGIN_CONFIG_DIR") continue;
    env[key] = value;
  }
  env.HERDR_PLUGIN_STATE_DIR = source.state_dir;
  env.HERDR_PLUGIN_CONFIG_DIR = source.config_dir;
  env.HERDR_PLUGIN_ID = CONVERSATIONS_PLUGIN_ID;

  // `.ts` entrypoints run under Bun; a native `bin/` path spawns directly.
  const isTs = source.cli_path.endsWith(".ts");
  const argv = isTs ? [source.cli_path, ...args] : args;
  const command = isTs ? process.execPath : source.cli_path;
  return new Promise((resolve, reject) => {
    const child = spawn(command, argv, {
      cwd: source.plugin_root,
      env,
      stdio: ["ignore", "pipe", "pipe"],
      timeout: (call.timeoutMs ?? 30_000) + 10_000,
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
    child.on("close", (code) => {
      let json: Record<string, unknown> | null = null;
      try {
        json = JSON.parse(stdout) as Record<string, unknown>;
      } catch {
        /* non-JSON output */
      }
      resolve({ status: code ?? -1, stdout, stderr, json });
    });
  });
}

/** Forwarded connection/selector flags shared by `capability` and `usage`. */
export function conversationsArgs(call: CliCallOptions, selectors?: UsageSelectors): string[] {
  const args: string[] = [];
  if (call.offline === true) args.push("--offline");
  if (call.session !== undefined) args.push("--session", call.session);
  if (call.socketPath !== undefined) args.push("--socket", call.socketPath);
  if (call.timeoutMs !== undefined) args.push("--timeout", String(call.timeoutMs));
  if (selectors?.pane !== undefined) args.push("--pane", selectors.pane);
  if (selectors?.sessionId !== undefined) args.push("--session-id", selectors.sessionId);
  if (selectors?.provider !== undefined) args.push("--provider", selectors.provider);
  return args;
}

export interface ExportResult {
  events: UsageEventRecord[];
  sessions: SessionCoverageEntry[];
  pages: number;
  complete: boolean;
  malformed_lines: number;
  duplicates_skipped: number;
  restarted_sessions: string[];
  /** Conversations contract string observed on the first page. */
  contract: string | null;
}

const EXPORT_PAGE_LIMIT = 500;
/** Generous ceiling: 4000 pages × 500 events = 2M usage events. */
const MAX_EXPORT_PAGES = 4_000;

/**
 * Page through `conversations usage` until the opaque cursor is
 * exhausted, collecting every usage-carrying event plus the session
 * registry entries. Never partially silent: hitting MAX_EXPORT_PAGES
 * returns `complete: false` rather than a truncated "success".
 */
export async function exportUsageEvents(
  source: SourceResolution,
  call: CliCallOptions,
  selectors?: UsageSelectors,
): Promise<ExportResult | { error: string; kind: string }> {
  const events: UsageEventRecord[] = [];
  let sessions: SessionCoverageEntry[] = [];
  let contract: string | null = null;
  let cursor: string | null = null;
  let pages = 0;
  let malformed = 0;
  let duplicates = 0;
  const restarted = new Set<string>();
  const baseArgs = conversationsArgs(call, selectors);

  for (;;) {
    if (pages >= MAX_EXPORT_PAGES) {
      return {
        events,
        sessions,
        pages,
        complete: false,
        malformed_lines: malformed,
        duplicates_skipped: duplicates,
        restarted_sessions: [...restarted],
        contract,
      };
    }
    const args = ["usage", "--limit", String(EXPORT_PAGE_LIMIT)];
    if (cursor !== null) args.push("--cursor", cursor);
    args.push(...baseArgs);

    const run = await runConversationsCli(source, args, call);
    if (run.status !== 0 || run.json === null) {
      let detail = run.stderr.trim();
      try {
        const errJson = JSON.parse(run.stderr) as { error?: { message?: string } };
        if (errJson.error?.message) detail = errJson.error.message;
      } catch {
        /* keep raw stderr */
      }
      return {
        error: `conversations usage failed (exit ${run.status}): ${detail || run.stdout.slice(0, 400)}`,
        kind: "conversations_error",
      };
    }
    const page = run.json as ConversationsUsagePage;
    if (typeof page.contract === "string") contract = page.contract;
    pages += 1;

    const results = Array.isArray(page.results) ? page.results : [];
    for (const r of results) {
      if (typeof r !== "object" || r === null) continue;
      const rec = r as Record<string, unknown>;
      const sessionKey = typeof rec.session_key === "string" ? rec.session_key : null;
      const event = typeof rec.event === "object" && rec.event !== null ? rec.event : null;
      if (sessionKey === null || event === null) continue;
      events.push({ session_key: sessionKey, event: event as UsageEventRecord["event"] });
    }
    if (Array.isArray(page.sessions)) {
      sessions = page.sessions as SessionCoverageEntry[];
    }
    const pageInfo = page.page ?? {};
    malformed += typeof pageInfo.malformed_lines === "number" ? pageInfo.malformed_lines : 0;
    duplicates += typeof pageInfo.duplicates_skipped === "number" ? pageInfo.duplicates_skipped : 0;
    if (Array.isArray(pageInfo.restarted_sessions)) {
      for (const key of pageInfo.restarted_sessions) {
        if (typeof key === "string") restarted.add(key);
      }
    }

    const next = pageInfo.next_cursor;
    const hasMore = pageInfo.has_more === true;
    if (hasMore && typeof next === "string" && next !== "") {
      cursor = next;
      continue;
    }
    return {
      events,
      sessions,
      pages,
      complete: true,
      malformed_lines: malformed,
      duplicates_skipped: duplicates,
      restarted_sessions: [...restarted],
      contract,
    };
  }
}

export interface CapabilityInfo {
  contract: string | null;
  providers: {
    id: string;
    status: string;
    formats?: string[];
    /** Adapter-declared usage accounting (e.g. cache_tokens). */
    usage?: { cache_tokens?: string };
  }[];
}

/** Fetch the conversations capability matrix (provider support statuses). */
export async function fetchConversationsCapability(
  source: SourceResolution,
  call: CliCallOptions,
): Promise<CapabilityInfo | null> {
  const run = await runConversationsCli(source, ["capability"], call);
  if (run.status !== 0 || run.json === null) return null;
  const json = run.json;
  const providers = Array.isArray(json.providers)
    ? (
        json.providers as {
          id?: unknown;
          status?: unknown;
          formats?: unknown;
          usage?: unknown;
        }[]
      )
        .filter((p) => typeof p.id === "string")
        .map((p) => {
          const usageRaw = asRecord(p.usage);
          const cacheTokens = usageRaw?.cache_tokens;
          return {
            id: p.id as string,
            status: typeof p.status === "string" ? p.status : "unknown",
            ...(Array.isArray(p.formats) ? { formats: p.formats as string[] } : {}),
            ...(typeof cacheTokens === "string" ? { usage: { cache_tokens: cacheTokens } } : {}),
          };
        })
    : [];
  return {
    contract: typeof json.contract === "string" ? json.contract : null,
    providers,
  };
}
