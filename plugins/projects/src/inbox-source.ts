/**
 * rebuildup.inbox source resolution + `transitions` CLI invocation for
 * the projects settle-trigger drain (#182, ADR-0029, ADR-0013 pattern).
 *
 * Projects consumes inbox's organize-transition log exclusively through
 * its public read-only CLI contract — never its state files. The inbox
 * plugin root resolves from:
 *   1. explicit override (`--inbox-root` / `inboxRoot` in config.json),
 *   2. the sibling `../inbox` directory (repo layout and managed
 *      installs both keep `plugins/<name>` side by side),
 *   3. `herdr plugin list --json` discovery.
 *
 * The child process gets `HERDR_PLUGIN_STATE_DIR`/`_CONFIG_DIR` pointed
 * at the *inbox* plugin dirs and `HERDR_PLUGIN_ID=rebuildup.inbox` — the
 * ambient values belong to rebuildup.projects and must never redirect
 * inbox state (same env-pinning as plugins/usage (ADR-0013)).
 */

import { spawn } from "node:child_process";
import { stat } from "node:fs/promises";
import path from "node:path";
import {
  herdrConfigDir,
  herdrStateDir,
  pluginPathComponent,
} from "../../../packages/herdr-client/src/index";

export const INBOX_PLUGIN_ID = "rebuildup.inbox";

/** This plugin's own root (plugins/projects). */
export function projectsPluginRoot(): string {
  const envRoot = process.env.HERDR_PLUGIN_ROOT;
  if (envRoot && envRoot !== "") return envRoot;
  // src/inbox-source.ts → plugins/projects
  return path.resolve(import.meta.dir, "..");
}

function defaultInboxStateDir(): string {
  return path.join(herdrStateDir(), "plugins", pluginPathComponent(INBOX_PLUGIN_ID));
}

function defaultInboxConfigDir(): string {
  return path.join(herdrConfigDir(), "plugins", "config", pluginPathComponent(INBOX_PLUGIN_ID));
}

async function isInboxRoot(dir: string): Promise<boolean> {
  try {
    // Native distribution (ADR-0035): the plugin ships bin/inbox.
    const bin = await stat(path.join(dir, "bin", "inbox"));
    if (!bin.isFile()) return false;
    const tomlPath = path.join(dir, "herdr-plugin.toml");
    const toml = await stat(tomlPath).catch(() => null);
    if (toml === null || !toml.isFile()) return true; // bin/inbox is enough signal
    const { readFile } = await import("node:fs/promises");
    const text = await readFile(tomlPath, "utf8");
    return text.includes(`id = "${INBOX_PLUGIN_ID}"`);
  } catch {
    return false;
  }
}

/** `herdr plugin list --json` → plugin_root for rebuildup.inbox. */
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
      if (id === INBOX_PLUGIN_ID && typeof root === "string" && root !== "") {
        return root;
      }
    }
  } catch {
    return null;
  }
  return null;
}

export interface InboxSource {
  plugin_root: string;
  cli_path: string;
  state_dir: string;
  config_dir: string;
  resolved_via: "override" | "sibling" | "herdr-plugin-list";
}

export type ResolveInboxResult =
  | { ok: true; source: InboxSource }
  | { ok: false; error: string; kind: string };

/**
 * Resolve the inbox plugin root + its state/config dirs. Never throws;
 * failures are structured so the drain can report `inbox_unavailable`
 * without failing the calling `scripts` command.
 */
export async function resolveInboxSource(overrideRoot?: string): Promise<ResolveInboxResult> {
  let root: string | null = null;
  let resolvedVia: InboxSource["resolved_via"] = "override";

  if (overrideRoot !== undefined && overrideRoot !== "") {
    const candidate = path.resolve(overrideRoot);
    if (await isInboxRoot(candidate)) {
      root = candidate;
    } else {
      return {
        ok: false,
        kind: "inbox_unavailable",
        error: `inbox root ${candidate} is not a rebuildup.inbox plugin root`,
      };
    }
  } else {
    const sibling = path.resolve(projectsPluginRoot(), "..", "inbox");
    if (await isInboxRoot(sibling)) {
      root = sibling;
      resolvedVia = "sibling";
    } else {
      const discovered = await discoverViaHerdr();
      if (discovered !== null && (await isInboxRoot(discovered))) {
        root = discovered;
        resolvedVia = "herdr-plugin-list";
      }
    }
  }

  if (root === null) {
    return {
      ok: false,
      kind: "inbox_unavailable",
      error:
        "rebuildup.inbox plugin not found; install it or set inboxRoot in config.json / pass --inbox-root",
    };
  }

  return {
    ok: true,
    source: {
      plugin_root: root,
      cli_path: path.join(root, "bin", "inbox"),
      state_dir: defaultInboxStateDir(),
      config_dir: defaultInboxConfigDir(),
      resolved_via: resolvedVia,
    },
  };
}

export interface InboxCliCallOptions {
  session?: string | undefined;
  socketPath?: string | undefined;
  timeoutMs?: number | undefined;
}

export interface InboxCliResult {
  status: number;
  stdout: string;
  stderr: string;
  json: Record<string, unknown> | null;
}

/**
 * Run `inbox transitions ...` with plugin dirs pinned to the *inbox*
 * plugin. Never throws on non-zero exit — callers inspect `status`/`json`.
 */
export function runInboxCli(
  source: InboxSource,
  args: string[],
  call: InboxCliCallOptions,
): Promise<InboxCliResult> {
  // cli_path is the native binary (plugins/inbox/bin/inbox, ADR-0035) —
  // spawned directly, not through `bun run`.
  const cliArgs = args;
  // Child env: pin the plugin dirs to the *inbox* plugin so the ambient
  // HERDR_PLUGIN_* values (which belong to rebuildup.projects) cannot
  // redirect its state resolution.
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (value === undefined) continue;
    if (key === "HERDR_PLUGIN_STATE_DIR" || key === "HERDR_PLUGIN_CONFIG_DIR") continue;
    env[key] = value;
  }
  env.HERDR_PLUGIN_STATE_DIR = source.state_dir;
  env.HERDR_PLUGIN_CONFIG_DIR = source.config_dir;
  env.HERDR_PLUGIN_ID = INBOX_PLUGIN_ID;

  return new Promise((resolve, reject) => {
    const child = spawn(source.cli_path, cliArgs, {
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

/** Session/target flags forwarded to the inbox CLI verbatim. */
export function inboxTargetArgs(call: InboxCliCallOptions): string[] {
  const args: string[] = [];
  if (call.session !== undefined) args.push("--session", call.session);
  if (call.socketPath !== undefined) args.push("--socket", call.socketPath);
  if (call.timeoutMs !== undefined) args.push("--timeout", String(call.timeoutMs));
  return args;
}
