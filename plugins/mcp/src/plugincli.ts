/**
 * Sibling-plugin CLI access for `rebuildup.mcp` (ADR-0025 §3,
 * ADR-0013 env-pin pattern).
 *
 * Plugin contract surfaces (conversations/git/projects/usage, later
 * controls/handoff) are consumed by spawning the sibling plugin's
 * `src/cli.ts` under Bun — never by reading its state files. The
 * plugin root resolves from:
 *   1. per-plugin env override (`HERDR_MCP_ROOT_<NAME>`),
 *   2. the sibling `plugins/<name>` directory (repo + managed layout),
 *   3. `herdr plugin list --json` discovery.
 *
 * The child env pins `HERDR_PLUGIN_STATE_DIR`/`HERDR_PLUGIN_CONFIG_DIR`
 * to the *target* plugin's dirs and `HERDR_PLUGIN_ID` to its id; all
 * ambient `HERDR_PLUGIN_*` values (which belong to rebuildup.mcp or an
 * outer plugin context) plus pane-scoped env are stripped so they
 * cannot redirect the child's state or pane context. The resolved
 * socket is passed explicitly via `--socket` and `HERDR_SOCKET_PATH`.
 */

import { spawn } from "node:child_process";
import { stat } from "node:fs/promises";
import path from "node:path";
import {
  herdrConfigDir,
  herdrStateDir,
  pluginPathComponent,
} from "../../../packages/herdr-client/src/index";

/** Plugin contracts the MCP read surface consumes (T2, #138). */
export const READ_PLUGIN_CONTRACTS = {
  "rebuildup.conversations": "herdr-plugins.conversations/v1",
  "rebuildup.git": "herdr-plugins.git/v1",
  "rebuildup.projects": "herdr-plugins.projects/v1",
  "rebuildup.usage": "herdr-plugins.usage/v1",
} as const;

/** Plugin contracts consumed by the MCP mutation surface (T3, #139). */
export const MUTATION_PLUGIN_CONTRACTS = {
  "rebuildup.controls": "herdr-plugins.controls/v1",
  "rebuildup.handoff": "herdr-plugins.handoff/v1",
} as const;

export type PluginId = keyof typeof READ_PLUGIN_CONTRACTS | keyof typeof MUTATION_PLUGIN_CONTRACTS;

/** `rebuildup.<name>` → sibling directory `plugins/<name>`. */
function pluginDirName(pluginId: string): string {
  return pluginId.startsWith("rebuildup.") ? pluginId.slice("rebuildup.".length) : pluginId;
}

/** Env override for tests/managed installs: `HERDR_MCP_ROOT_GIT` etc. */
function overrideEnvVar(pluginId: string): string {
  return `HERDR_MCP_ROOT_${pluginDirName(pluginId).toUpperCase().replace(/-/g, "_")}`;
}

/** This plugin's own root (plugins/mcp). */
function mcpPluginRoot(): string {
  const envRoot = process.env.HERDR_PLUGIN_ROOT;
  if (envRoot !== undefined && envRoot !== "") return envRoot;
  // src/plugincli.ts → plugins/mcp
  return path.resolve(import.meta.dir, "..");
}

function defaultStateDir(pluginId: string): string {
  return path.join(herdrStateDir(), "plugins", pluginPathComponent(pluginId));
}

function defaultConfigDir(pluginId: string): string {
  return path.join(herdrConfigDir(), "plugins", "config", pluginPathComponent(pluginId));
}

async function isPluginRoot(dir: string, pluginId: string): Promise<boolean> {
  try {
    // Native distribution (ADR-0035): prefer bin/<name>; src/cli.ts is
    // the legacy Bun entrypoint fallback.
    const bin = await stat(path.join(dir, "bin", pluginDirName(pluginId))).catch(() => null);
    const cli = bin?.isFile() === true ? bin : await stat(path.join(dir, "src", "cli.ts"));
    if (!cli.isFile()) return false;
    const tomlPath = path.join(dir, "herdr-plugin.toml");
    const toml = await stat(tomlPath).catch(() => null);
    if (toml === null || !toml.isFile()) return true; // cli.ts is enough signal
    const { readFile } = await import("node:fs/promises");
    const text = await readFile(tomlPath, "utf8");
    return text.includes(`id = "${pluginId}"`);
  } catch {
    return false;
  }
}

/** `herdr plugin list --json` → plugin_root for the plugin id. */
async function discoverViaHerdr(pluginId: string): Promise<string | null> {
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
      if (id === pluginId && typeof root === "string" && root !== "") {
        return root;
      }
    }
  } catch {
    return null;
  }
  return null;
}

/** Prefer the native `bin/<name>`; fall back to `src/cli.ts`. */
async function pluginCliPath(root: string, pluginId: string): Promise<string> {
  const bin = path.join(root, "bin", pluginDirName(pluginId));
  const info = await stat(bin).catch(() => null);
  return info?.isFile() === true ? bin : path.join(root, "src", "cli.ts");
}

export interface PluginSource {
  plugin_root: string;
  cli_path: string;
  state_dir: string;
  config_dir: string;
  resolved_via: "override" | "sibling" | "herdr-plugin-list";
}

export type ResolvePluginResult =
  | { ok: true; source: PluginSource }
  | { ok: false; kind: "plugin_unavailable"; error: string };

/**
 * Resolve a sibling plugin's root + its state/config dirs. Never
 * throws; failures are structured (`plugin_unavailable`) for the tool
 * error envelope and `herdr_capability` reporting.
 */
export async function resolvePluginSource(pluginId: string): Promise<ResolvePluginResult> {
  const override = process.env[overrideEnvVar(pluginId)];
  let root: string | null = null;
  let resolvedVia: PluginSource["resolved_via"] = "override";

  if (override !== undefined && override !== "") {
    const candidate = path.resolve(override);
    if (await isPluginRoot(candidate, pluginId)) {
      root = candidate;
    } else {
      return {
        ok: false,
        kind: "plugin_unavailable",
        error: `${overrideEnvVar(pluginId)}=${candidate} is not a ${pluginId} plugin root`,
      };
    }
  } else {
    const sibling = path.resolve(mcpPluginRoot(), "..", pluginDirName(pluginId));
    if (await isPluginRoot(sibling, pluginId)) {
      root = sibling;
      resolvedVia = "sibling";
    } else {
      const discovered = await discoverViaHerdr(pluginId);
      if (discovered !== null && (await isPluginRoot(discovered, pluginId))) {
        root = discovered;
        resolvedVia = "herdr-plugin-list";
      }
    }
  }

  if (root === null) {
    return {
      ok: false,
      kind: "plugin_unavailable",
      error: `${pluginId} plugin not found; install it or set ${overrideEnvVar(pluginId)}`,
    };
  }

  return {
    ok: true,
    source: {
      plugin_root: root,
      cli_path: await pluginCliPath(root, pluginId),
      state_dir: defaultStateDir(pluginId),
      config_dir: defaultConfigDir(pluginId),
      resolved_via: resolvedVia,
    },
  };
}

export interface PluginCliResult {
  /** Process exit code (null when the spawn itself failed). */
  status: number | null;
  /** Parsed stdout JSON document, or null when stdout was not JSON. */
  json: Record<string, unknown> | null;
  stderr: string;
}

export interface RunPluginOptions {
  /** Resolved herdr socket path; passed to the child as `--socket`. */
  socketPath: string;
  /** Wall-clock budget for the child process (default 60s). */
  timeoutMs?: number | undefined;
}

/**
 * Spawn a sibling plugin CLI with env-pinned plugin dirs. The child
 * always receives `--socket <resolved>` appended to `args` (every
 * sibling CLI accepts it) plus `HERDR_SOCKET_PATH` for consistency.
 */
export type PluginRunResult =
  | { ok: true; source: PluginSource; result: PluginCliResult }
  | { ok: false; kind: "plugin_unavailable"; error: string };

export async function runPluginCli(
  pluginId: PluginId,
  args: string[],
  options: RunPluginOptions,
): Promise<PluginRunResult> {
  const resolved = await resolvePluginSource(pluginId);
  if (!resolved.ok) return resolved;
  const { source } = resolved;

  // Child env: strip every ambient HERDR_PLUGIN_* value (state/config/
  // root/id/context belong to rebuildup.mcp or an outer plugin
  // context) and pane-scoped identity, then pin the target plugin's
  // dirs + id and the resolved socket (ADR-0013 env-pin pattern).
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (value === undefined) continue;
    if (key.startsWith("HERDR_PLUGIN_")) continue;
    if (key === "HERDR_PANE_ID" || key === "HERDR_WORKSPACE_ID" || key === "HERDR_TAB_ID") {
      continue;
    }
    env[key] = value;
  }
  env.HERDR_PLUGIN_STATE_DIR = source.state_dir;
  env.HERDR_PLUGIN_CONFIG_DIR = source.config_dir;
  env.HERDR_PLUGIN_ID = pluginId;
  env.HERDR_SOCKET_PATH = options.socketPath;

  const cliArgs = [...args, "--socket", options.socketPath];
  const timeoutMs = options.timeoutMs ?? 60_000;

  const result = await new Promise<PluginCliResult>((resolvePromise, reject) => {
    // `.ts` entrypoints run under Bun; a native `bin/` path spawns directly.
    const isTs = source.cli_path.endsWith(".ts");
    const child = spawn(
      isTs ? process.execPath : source.cli_path,
      isTs ? [source.cli_path, ...cliArgs] : cliArgs,
      {
        cwd: source.plugin_root,
        env,
        stdio: ["ignore", "pipe", "pipe"],
        timeout: timeoutMs + 10_000,
      },
    );
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
      resolvePromise({ status: code, json, stderr });
    });
  });

  return { ok: true, source, result };
}
