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
 * Locate a runnable rebuildup.git native binary (Issue #292, ADR-0035):
 * the committed prebuilt `plugins/git/bin/git` first, then cargo
 * `target/debug`/`target/release` outputs, building debug as a last
 * resort. Shared by the native contract suite and cross-plugin tests
 * that spawn the real binary.
 */
export function gitBinary(): string {
  const repoRoot = path.join(import.meta.dir, "..", "..");
  const candidates = [
    path.join(repoRoot, "plugins", "git", "bin", "git"),
    path.join(repoRoot, "target", "debug", "git"),
    path.join(repoRoot, "target", "release", "git"),
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
  const build = spawnSync("cargo", ["build", "-p", "git"], {
    cwd: repoRoot,
    encoding: "utf8",
    timeout: 300_000,
  });
  const debugBin = path.join(repoRoot, "target", "debug", "git");
  if (build.status === 0 && existsSync(debugBin)) return debugBin;
  throw new Error(
    `git binary not runnable (bin/, target/debug, target/release all failed; ` +
      `cargo build -p git exited ${build.status}): ${build.stderr?.slice(0, 400)}`,
  );
}

let cachedPluginRoot: string | undefined;

/**
 * Materialize a minimal `rebuildup.git` plugin root in a temp dir for
 * sibling-plugin tests that spawn the real plugin (e.g. inbox
 * `issue link` delegation resolving `../git` or a configured root).
 * The committed `plugins/git/bin/git` prebuilt is macOS arm64 only, so
 * on Linux CI the repo sibling root resolves but its binary cannot exec;
 * tests point their override at THIS root whose `bin/git` links to the
 * platform-runnable binary from {@link gitBinary}.
 *
 * The layout satisfies `isPluginRoot`-style checks: `herdr-plugin.toml`
 * containing `id = "rebuildup.git"` plus a `bin/git` file.
 */
export function gitPluginRootFixture(): string {
  if (cachedPluginRoot !== undefined) return cachedPluginRoot;
  const root = mkdtempSync(path.join(tmpdir(), "git-plugin-root-"));
  mkdirSync(path.join(root, "bin"), { recursive: true });
  writeFileSync(
    path.join(root, "herdr-plugin.toml"),
    ['id = "rebuildup.git"', 'name = "Git"', 'version = "0.0.0-test"', ""].join("\n"),
  );
  const binLink = path.join(root, "bin", "git");
  try {
    symlinkSync(gitBinary(), binLink);
  } catch {
    copyFileSync(gitBinary(), binLink);
  }
  cachedPluginRoot = root;
  return root;
}
