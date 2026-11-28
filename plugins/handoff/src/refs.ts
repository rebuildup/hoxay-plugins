/**
 * Payload reference resolution for rebuildup.handoff (contract v1, #103).
 *
 * Handoff payloads are reference bundles: they name objects owned by other
 * plugins' public contracts and never copy their content. Resolution is
 * read-side and honest:
 *
 *   resolved    — the owning plugin's public CLI confirms the object exists.
 *   missing_ref — the owning CLI answered successfully and the object is
 *                 absent (deleted / never created / garbage-collected).
 *   unchecked   — the owner answered but the result cannot prove absence
 *                 (e.g. a truncated listing).
 *   error       — the owner plugin or its CLI could not be reached or
 *                 returned malformed output. Never reported as missing.
 *   external    — uri/path/issue refs are never fetched or verified.
 *
 * Owner plugins are consumed ONLY through their public CLI contract
 * (`sessions --offline`, `list --limit 1000`), located via the sibling
 * `plugins/<name>` directory or `herdr plugin list --json` — the same
 * discovery pattern as plugins/usage (ADR-0013). Owner state files are
 * never read directly.
 */

import { spawn } from "node:child_process";
import { stat } from "node:fs/promises";
import path from "node:path";
import {
  herdrConfigDir,
  herdrStateDir,
  pluginPathComponent,
} from "../../../packages/herdr-client/src/index";
import type { HandoffPayload, PayloadRef } from "./model";
import { loadState } from "./state";

export type RefResolutionState = "resolved" | "missing_ref" | "unchecked" | "error" | "external";

export interface ResolvedRef {
  ref: PayloadRef;
  state: RefResolutionState;
  detail: string;
}

// --- owner plugin discovery --------------------------------------------------

interface OwnerSpec {
  /** Directory name under plugins/ (sibling lookup + toml id check). */
  dir: string;
  pluginId: string;
  /** CLI args that enumerate addressable objects (read-only commands). */
  listArgs: string[];
  /** Extract the set of addressable keys/ids + completeness from CLI JSON. */
  extract: (json: Record<string, unknown>) => { ids: Set<string>; complete: boolean } | null;
}

function extractSessions(
  json: Record<string, unknown>,
): { ids: Set<string>; complete: boolean } | null {
  if (!Array.isArray(json.sessions)) return null;
  const ids = new Set<string>();
  for (const entry of json.sessions) {
    if (typeof entry === "object" && entry !== null) {
      const key = (entry as Record<string, unknown>).key;
      if (typeof key === "string") ids.add(key);
    }
  }
  return { ids, complete: true };
}

function extractCheckpoints(
  json: Record<string, unknown>,
): { ids: Set<string>; complete: boolean } | null {
  if (!Array.isArray(json.checkpoints)) return null;
  const ids = new Set<string>();
  for (const entry of json.checkpoints) {
    if (typeof entry === "object" && entry !== null) {
      const id = (entry as Record<string, unknown>).id;
      if (typeof id === "string") ids.add(id);
    }
  }
  // `total` counts all matching entries; `checkpoints` is the page. When
  // the page is smaller than the total, absence is unproven → unchecked.
  const total = typeof json.total === "number" ? json.total : json.checkpoints.length;
  return { ids, complete: json.checkpoints.length >= total };
}

const OWNERS: Record<string, OwnerSpec> = {
  "rebuildup.conversations": {
    dir: "conversations",
    pluginId: "rebuildup.conversations",
    listArgs: ["sessions", "--offline"],
    extract: extractSessions,
  },
  "rebuildup.checkpoints": {
    dir: "checkpoints",
    pluginId: "rebuildup.checkpoints",
    listArgs: ["list", "--limit", "1000"],
    extract: extractCheckpoints,
  },
};

async function isOwnerRoot(dir: string, pluginId: string, dirName: string): Promise<boolean> {
  try {
    // Native distribution (ADR-0035): prefer bin/<name>; src/cli.ts is
    // the legacy Bun entrypoint fallback.
    const bin = await stat(path.join(dir, "bin", dirName)).catch(() => null);
    const cli = bin?.isFile() === true ? bin : await stat(path.join(dir, "src", "cli.ts"));
    if (!cli.isFile()) return false;
    const tomlPath = path.join(dir, "herdr-plugin.toml");
    const toml = await stat(tomlPath).catch(() => null);
    if (toml === null || !toml.isFile()) return true;
    const { readFile } = await import("node:fs/promises");
    const text = await readFile(tomlPath, "utf8");
    return text.includes(`id = "${pluginId}"`);
  } catch {
    return false;
  }
}

/** `rebuildup.<name>` → sibling directory/binary name `<name>`. */
function pluginDirName(pluginId: string): string {
  const dot = pluginId.indexOf(".");
  return dot === -1 ? pluginId : pluginId.slice(dot + 1);
}

/** Sibling plugins/<dir>, then `herdr plugin list --json` discovery. */
async function ownerRoot(spec: OwnerSpec): Promise<string | null> {
  const sibling = path.resolve(import.meta.dir, "..", "..", spec.dir);
  if (await isOwnerRoot(sibling, spec.pluginId, spec.dir)) return sibling;

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
      if (id === spec.pluginId && typeof root === "string" && root !== "") {
        if (await isOwnerRoot(root, spec.pluginId, spec.dir)) return root;
      }
    }
  } catch {
    /* fall through */
  }
  return null;
}

interface CliJson {
  ok: boolean;
  json: Record<string, unknown> | null;
  error: string | null;
}

/**
 * Run an owner plugin's public CLI. The child env pins
 * HERDR_PLUGIN_STATE_DIR/_CONFIG_DIR/_ID to the *owner's* dirs — the
 * ambient values belong to rebuildup.handoff and must never leak in
 * (same rule as plugins/usage source.ts).
 */
async function runOwnerCli(root: string, pluginId: string, args: string[]): Promise<CliJson> {
  const component = pluginPathComponent(pluginId);
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (value === undefined) continue;
    if (
      key === "HERDR_PLUGIN_STATE_DIR" ||
      key === "HERDR_PLUGIN_CONFIG_DIR" ||
      key === "HERDR_PLUGIN_ID"
    ) {
      continue;
    }
    env[key] = value;
  }
  env.HERDR_PLUGIN_STATE_DIR = path.join(herdrStateDir(), "plugins", component);
  env.HERDR_PLUGIN_CONFIG_DIR = path.join(herdrConfigDir(), "plugins", "config", component);
  env.HERDR_PLUGIN_ID = pluginId;

  // Native distribution (ADR-0035): bin/<dir> spawns directly; the
  // legacy src/cli.ts entrypoint runs under `bun run`.
  const bin = path.join(root, "bin", pluginDirName(pluginId));
  const binInfo = await stat(bin).catch(() => null);
  const useNative = binInfo?.isFile() === true;
  const command = useNative ? bin : process.execPath;
  const argv = useNative ? args : ["run", path.join(root, "src", "cli.ts"), ...args];
  return new Promise((resolve) => {
    const child = spawn(command, argv, {
      cwd: root,
      env,
      stdio: ["ignore", "pipe", "pipe"],
      timeout: 30_000,
    });
    let stdout = "";
    let stderr = "";
    child.stdout.on("data", (chunk: Buffer) => {
      stdout += chunk.toString("utf8");
    });
    child.stderr.on("data", (chunk: Buffer) => {
      stderr += chunk.toString("utf8");
    });
    child.on("error", (err) => resolve({ ok: false, json: null, error: String(err) }));
    child.on("close", (code) => {
      let json: Record<string, unknown> | null = null;
      try {
        json = JSON.parse(stdout) as Record<string, unknown>;
      } catch {
        /* non-JSON output */
      }
      resolve({
        ok: code === 0 && json !== null,
        json,
        error: code === 0 ? null : `exit ${code ?? "?"}: ${stderr.slice(0, 300)}`,
      });
    });
  });
}

// --- resolution ---------------------------------------------------------------

/** Injectable seams for tests; defaults hit the real owner CLIs/state. */
export interface RefDeps {
  /** Addressable conversation session keys; null = index unavailable. */
  conversationKeys?: () => Promise<Set<string> | null>;
  /** Checkpoint ids + listing completeness; null = unavailable. */
  checkpointIds?: () => Promise<{ ids: Set<string>; complete: boolean } | null>;
  /**
   * Handoff record ids in this plugin's own state (#106
   * `handoff_record` refs); null = state unreadable.
   */
  handoffIds?: () => Promise<Set<string> | null>;
}

async function ownerIdSet(
  pluginId: string,
  args: string[],
): Promise<{ ids: Set<string>; complete: boolean } | { error: string } | null> {
  const spec = OWNERS[pluginId];
  if (!spec) return { error: `no owner spec for ${pluginId}` };
  const root = await ownerRoot(spec);
  if (root === null) return { error: `${pluginId} plugin not found` };
  const result = await runOwnerCli(root, pluginId, args);
  if (!result.ok || result.json === null) {
    return { error: result.error ?? "owner CLI returned no JSON" };
  }
  const extracted = spec.extract(result.json);
  if (extracted === null) return { error: "owner CLI output missing expected list" };
  return extracted;
}

function defaultRefDeps(): Required<RefDeps> {
  const cache = new Map<
    string,
    Promise<{ ids: Set<string>; complete: boolean } | { error: string } | null>
  >();
  const get = (pluginId: string, args: string[]) => {
    const key = `${pluginId} ${args.join(" ")}`;
    let pending = cache.get(key);
    if (pending === undefined) {
      pending = ownerIdSet(pluginId, args);
      cache.set(key, pending);
    }
    return pending;
  };
  return {
    conversationKeys: async () => {
      const spec = OWNERS["rebuildup.conversations"];
      const result = await get("rebuildup.conversations", spec?.listArgs ?? []);
      if (result === null || "error" in result) return null;
      return result.ids;
    },
    checkpointIds: async () => {
      const spec = OWNERS["rebuildup.checkpoints"];
      const result = await get("rebuildup.checkpoints", spec?.listArgs ?? []);
      if (result === null || "error" in result) return null;
      return result;
    },
    handoffIds: async () => {
      try {
        const state = await loadState();
        return new Set(Object.keys(state.handoffs));
      } catch {
        return null;
      }
    },
  };
}

/** Resolve every payload ref; order and content of `payload.refs` preserved. */
export async function resolvePayloadRefs(
  payload: HandoffPayload,
  deps: RefDeps = {},
): Promise<ResolvedRef[]> {
  const defaults = defaultRefDeps();
  const d: Required<RefDeps> = {
    conversationKeys: deps.conversationKeys ?? defaults.conversationKeys,
    checkpointIds: deps.checkpointIds ?? defaults.checkpointIds,
    handoffIds: deps.handoffIds ?? defaults.handoffIds,
  };
  const out: ResolvedRef[] = [];
  for (const ref of payload.refs) {
    if (ref.kind === "external") {
      out.push({
        ref,
        state: "external",
        detail: `${ref.target}: ${ref.value} (not fetched or verified)`,
      });
      continue;
    }
    if (ref.kind === "conversation_session") {
      const keys = await d.conversationKeys();
      if (keys === null) {
        out.push({
          ref,
          state: "error",
          detail: "rebuildup.conversations index unavailable",
        });
        continue;
      }
      if (keys.has(ref.session_key)) {
        out.push({
          ref,
          state: "resolved",
          detail: `session ${ref.session_key} present in conversations index`,
        });
      } else {
        out.push({
          ref,
          state: "missing_ref",
          detail: `session ${ref.session_key} absent from conversations index`,
        });
      }
      continue;
    }
    if (ref.kind === "handoff_record") {
      // Self-owned ref (#106): resolved against this plugin's state.json —
      // no external CLI involved.
      const ids = await d.handoffIds();
      if (ids === null) {
        out.push({ ref, state: "error", detail: "handoff state unreadable" });
        continue;
      }
      if (ids.has(ref.id)) {
        out.push({ ref, state: "resolved", detail: `handoff record ${ref.id} present` });
      } else {
        out.push({
          ref,
          state: "missing_ref",
          detail: `handoff record ${ref.id} absent from handoff state`,
        });
      }
      continue;
    }
    // checkpoint
    const listing = await d.checkpointIds();
    if (listing === null) {
      out.push({
        ref,
        state: "error",
        detail: "rebuildup.checkpoints listing unavailable",
      });
      continue;
    }
    if (listing.ids.has(ref.id)) {
      out.push({ ref, state: "resolved", detail: `checkpoint ${ref.id} present` });
    } else if (!listing.complete) {
      out.push({
        ref,
        state: "unchecked",
        detail: `checkpoint ${ref.id} not in first page; listing truncated`,
      });
    } else {
      out.push({
        ref,
        state: "missing_ref",
        detail: `checkpoint ${ref.id} absent from checkpoints state`,
      });
    }
  }
  return out;
}

export function missingRefCount(resolved: ResolvedRef[]): number {
  return resolved.filter((r) => r.state === "missing_ref").length;
}
