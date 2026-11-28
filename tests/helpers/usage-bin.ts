import { spawnSync } from "node:child_process";
import { existsSync } from "node:fs";
import path from "node:path";
import { testEnv } from "./test-env";

/**
 * Locate a runnable rebuildup.usage native binary (Issue #294,
 * ADR-0035): the committed prebuilt `plugins/usage/bin/usage` first,
 * then cargo `target/debug`/`target/release` outputs, building debug as
 * a last resort. Shared by the native contract suite.
 */
export function usageBinary(): string {
  const repoRoot = path.join(import.meta.dir, "..", "..");
  const candidates = [
    path.join(repoRoot, "plugins", "usage", "bin", "usage"),
    path.join(repoRoot, "target", "debug", "usage"),
    path.join(repoRoot, "target", "release", "usage"),
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
  const build = spawnSync("cargo", ["build", "-p", "usage"], {
    cwd: repoRoot,
    encoding: "utf8",
    timeout: 300_000,
  });
  const debugBin = path.join(repoRoot, "target", "debug", "usage");
  if (build.status === 0 && existsSync(debugBin)) return debugBin;
  throw new Error(
    `usage binary not runnable (bin/, target/debug, target/release all failed; ` +
      `cargo build -p usage exited ${build.status}): ${build.stderr?.slice(0, 400)}`,
  );
}
