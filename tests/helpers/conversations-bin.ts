import { spawnSync } from "node:child_process";
import {
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  symlinkSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import path from "node:path";
import { testEnv } from "./test-env";

/**
 * Locate a runnable rebuildup.conversations native binary (Issue #293,
 * ADR-0035): the committed prebuilt `plugins/conversations/bin/conversations`
 * first, then cargo `target/debug`/`target/release` outputs, building
 * debug as a last resort. Shared by the native contract suite.
 */
export function conversationsBinary(): string {
  const repoRoot = path.join(import.meta.dir, "..", "..");
  const candidates = [
    path.join(repoRoot, "plugins", "conversations", "bin", "conversations"),
    path.join(repoRoot, "target", "debug", "conversations"),
    path.join(repoRoot, "target", "release", "conversations"),
  ];
  for (const candidate of candidates) {
    if (!existsSync(candidate)) continue;
    const probe = spawnSync(candidate, ["capability"], {
      encoding: "utf8",
      env: testEnv(),
      timeout: 15_000,
    });
    if (probe.status === 0) return candidate;
  }
  const build = spawnSync("cargo", ["build", "-p", "conversations"], {
    cwd: repoRoot,
    encoding: "utf8",
    timeout: 300_000,
  });
  const debugBin = path.join(repoRoot, "target", "debug", "conversations");
  if (build.status === 0 && existsSync(debugBin)) return debugBin;
  throw new Error(
    `conversations binary not runnable (bin/, target/debug, target/release all failed; ` +
      `cargo build -p conversations exited ${build.status}): ${build.stderr?.slice(0, 400)}`,
  );
}

let cachedPluginRoot: string | undefined;

/**
 * Materialize a minimal `rebuildup.conversations` plugin root in a temp
 * dir for tests that exercise plugin-root-relative resolution. The
 * layout satisfies `isPluginRoot`-style checks: `herdr-plugin.toml`
 * containing `id = "rebuildup.conversations"` plus a `bin/conversations`
 * file linked to the platform-runnable binary. Cached per process.
 */
export function conversationsPluginRootFixture(): string {
  if (cachedPluginRoot !== undefined) return cachedPluginRoot;
  const root = mkdtempSync(path.join(tmpdir(), "conversations-plugin-root-"));
  mkdirSync(path.join(root, "bin"), { recursive: true });
  writeFileSync(
    path.join(root, "herdr-plugin.toml"),
    ['id = "rebuildup.conversations"', 'name = "Conversations"', 'version = "0.0.0-test"', ""].join(
      "\n",
    ),
  );
  const binLink = path.join(root, "bin", "conversations");
  try {
    symlinkSync(conversationsBinary(), binLink);
  } catch {
    // Windows without symlink privilege: copy the runnable binary instead.
    copyFileSync(conversationsBinary(), binLink);
  }
  cachedPluginRoot = root;
  return root;
}
