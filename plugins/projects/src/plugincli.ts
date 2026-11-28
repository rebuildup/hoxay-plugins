/**
 * Sibling-plugin CLI access for rebuildup.projects (ADR-0029 §2,
 * ADR-0013 env-pin pattern — same shape as plugins/textgen).
 *
 * The repo↔worktree association surface consumes `rebuildup.git`'s
 * public read-only CLI contract (`herdr-plugins.git/v1`) for
 * pane-addressed `status` summaries. The plugin root resolves from:
 *   1. `HERDR_PROJECTS_ROOT_GIT` env override (tests/managed installs),
 *   2. the sibling `plugins/git` directory (repo + managed layout),
 *   3. `herdr plugin list --json` discovery.
 *
 * The child env strips EVERY ambient `HERDR_*` variable (issue #199:
 * an inherited `HERDR_SOCKET_PATH`/`HERDR_SESSION`/plugin-context value
 * would silently redirect the child's socket or state resolution) and
 * then pins only what the git plugin needs: its own state/config dirs,
 * its plugin id, and the resolved socket via `--socket` +
 * `HERDR_SOCKET_PATH`. Other plugins' state files are never read.
 */

import { spawn } from "node:child_process";
import { existsSync } from "node:fs";
import { readFile, stat } from "node:fs/promises";
import path from "node:path";
import {
  herdrConfigDir,
  herdrStateDir,
  pluginPathComponent,
} from "../../../packages/herdr-client/src/index";

export const GIT_PLUGIN_ID = "rebuildup.git";
export const GIT_CONTRACT = "herdr-plugins.git/v1";

/** Env override for tests/managed installs: `HERDR_PROJECTS_ROOT_GIT`. */
const GIT_ROOT_ENV = "HERDR_PROJECTS_ROOT_GIT";

/** This plugin's own root (plugins/projects). */
function projectsPluginRoot(): string {
  const envRoot = process.env.HERDR_PLUGIN_ROOT;
  if (envRoot !== undefined && envRoot !== "") return envRoot;
  // src/plugincli.ts → plugins/projects
  return path.resolve(import.meta.dir, "..");
}

function pluginDirName(pluginId: string): string {
  const dot = pluginId.indexOf(".");
  return dot === -1 ? pluginId : pluginId.slice(dot + 1);
}

async function isPluginRoot(dir: string, pluginId: string): Promise<boolean> {
  try {
    const tomlPath = path.join(dir, "herdr-plugin.toml");
    const toml = await stat(tomlPath);
    if (!toml.isFile()) return false;
    // Native layout (ADR-0035, #292): committed bin/git. Legacy TS layout:
    // src/cli.ts run under bun. Either marks a usable plugin root.
    const nativeBin = await stat(path.join(dir, "bin", "git")).catch(() => null);
    const tsCli = await stat(path.join(dir, "src", "cli.ts")).catch(() => null);
    if (nativeBin?.isFile() !== true && tsCli?.isFile() !== true) return false;
    const text = await readFile(tomlPath, "utf8");
    return text.includes(`id = "${pluginId}"`);
  } catch {
    return false;
  }
}

/** `herdr plugin list --json` → plugin_root for the given plugin id. */
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

export type GitPluginRootResolution =
  | { ok: true; root: string; via: "override" | "sibling" | "herdr-plugin-list" }
  | { ok: false; error: string };

/**
 * Resolve the rebuildup.git plugin root. Never throws — failures are
 * structured so the association surface can degrade honestly.
 */
export async function resolveGitPluginRoot(): Promise<GitPluginRootResolution> {
  const override = process.env[GIT_ROOT_ENV];
  if (override !== undefined && override !== "") {
    const candidate = path.resolve(override);
    if (await isPluginRoot(candidate, GIT_PLUGIN_ID)) {
      return { ok: true, root: candidate, via: "override" };
    }
    return {
      ok: false,
      error: `${GIT_ROOT_ENV}=${candidate} is not a ${GIT_PLUGIN_ID} plugin`,
    };
  }
  const sibling = path.resolve(projectsPluginRoot(), "..", pluginDirName(GIT_PLUGIN_ID));
  if (await isPluginRoot(sibling, GIT_PLUGIN_ID)) {
    return { ok: true, root: sibling, via: "sibling" };
  }
  const discovered = await discoverViaHerdr(GIT_PLUGIN_ID);
  if (discovered !== null && (await isPluginRoot(discovered, GIT_PLUGIN_ID))) {
    return { ok: true, root: discovered, via: "herdr-plugin-list" };
  }
  return {
    ok: false,
    error:
      `${GIT_PLUGIN_ID} plugin not found; install it or set ${GIT_ROOT_ENV} ` +
      "(resolution: override → sibling → herdr plugin list)",
  };
}

// --- sibling plugin CLI invocation ------------------------------------------

export interface GitPluginCliResult {
  status: number;
  stdout: string;
  stderr: string;
  json: Record<string, unknown> | null;
  /** Parsed `{error:{kind,message}}` from stderr, when present. */
  error: { kind: string; message: string } | null;
}

/**
 * Spawn the git plugin CLI (`bun <root>/src/cli.ts …`) with a fully
 * scrubbed env. Only read-only git-v1 commands may be passed — the
 * association surface never invokes mutations.
 */
export function runGitPluginCli(
  root: string,
  args: string[],
  call: { socketPath: string; timeoutMs: number },
): Promise<GitPluginCliResult> {
  // Native plugin (#292) ships bin/git and is spawned directly; the legacy
  // TS layout keeps running under bun for older installs.
  const nativeBin = path.join(root, "bin", "git");
  const useNative = existsSync(nativeBin);
  const cliArgs = useNative
    ? [...args, "--socket", call.socketPath]
    : [path.join(root, "src", "cli.ts"), ...args, "--socket", call.socketPath];

  // Strip every ambient HERDR_* value (socket/session/plugin context
  // belongs to the CALLER, never the child), then pin the git plugin's
  // own dirs + id + the resolved socket (ADR-0013 env-pin pattern).
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (value === undefined) continue;
    if (key.startsWith("HERDR_")) continue;
    env[key] = value;
  }
  const component = pluginPathComponent(GIT_PLUGIN_ID);
  env.HERDR_PLUGIN_STATE_DIR = path.join(herdrStateDir(), "plugins", component);
  env.HERDR_PLUGIN_CONFIG_DIR = path.join(herdrConfigDir(), "plugins", "config", component);
  env.HERDR_PLUGIN_ID = GIT_PLUGIN_ID;
  env.HERDR_SOCKET_PATH = call.socketPath;

  return new Promise((resolvePromise, reject) => {
    const child = spawn(useNative ? nativeBin : process.execPath, cliArgs, {
      cwd: root,
      env,
      stdio: ["ignore", "pipe", "pipe"],
      timeout: call.timeoutMs + 15_000,
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
      let error: GitPluginCliResult["error"] = null;
      try {
        const parsed = JSON.parse(stderr) as { error?: { kind?: unknown; message?: unknown } };
        if (typeof parsed.error?.kind === "string") {
          error = {
            kind: parsed.error.kind,
            message: typeof parsed.error.message === "string" ? parsed.error.message : "",
          };
        }
      } catch {
        /* stderr was not the contract error envelope */
      }
      resolvePromise({ status: code ?? -1, stdout, stderr, json, error });
    });
  });
}
