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
 * Locate a runnable rebuildup.inbox native binary (Issue #291, ADR-0035):
 * the committed prebuilt `plugins/inbox/bin/inbox` first, then cargo
 * `target/debug`/`target/release` outputs, building debug as a last
 * resort. Shared by the native contract suite and cross-plugin tests
 * (herdr-target) that spawn the real binary.
 */
export function inboxBinary(): string {
  const repoRoot = path.join(import.meta.dir, "..", "..");
  const candidates = [
    path.join(repoRoot, "plugins", "inbox", "bin", "inbox"),
    path.join(repoRoot, "target", "debug", "inbox"),
    path.join(repoRoot, "target", "release", "inbox"),
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
  const build = spawnSync("cargo", ["build", "-p", "inbox"], {
    cwd: repoRoot,
    encoding: "utf8",
    timeout: 300_000,
  });
  const debugBin = path.join(repoRoot, "target", "debug", "inbox");
  if (build.status === 0 && existsSync(debugBin)) return debugBin;
  throw new Error(
    `inbox binary not runnable (bin/, target/debug, target/release all failed; ` +
      `cargo build -p inbox exited ${build.status}): ${build.stderr?.slice(0, 400)}`,
  );
}

let cachedPluginRoot: string | undefined;

/**
 * Materialize a minimal `rebuildup.inbox` plugin root in a temp dir for
 * sibling-plugin delegation tests (Issue #291). The committed
 * `plugins/inbox/bin/inbox` prebuilt is macOS arm64 only, so on Linux CI
 * the repo sibling root resolves but its binary cannot exec. Tests that
 * exercise `plugins/git` `issue link` delegation or `plugins/projects`
 * settle drains point their existing override (`inboxPluginRoot` /
 * `inboxRoot` in config.json) at THIS root instead, whose `bin/inbox`
 * links to the platform-runnable binary from {@link inboxBinary}.
 *
 * The layout satisfies both resolvers' `isPluginRoot`/`isInboxRoot`
 * checks: `herdr-plugin.toml` containing `id = "rebuildup.inbox"` plus a
 * `bin/inbox` file. Cached per process — the binary path is stable.
 */
export function inboxPluginRootFixture(): string {
  if (cachedPluginRoot !== undefined) return cachedPluginRoot;
  const root = mkdtempSync(path.join(tmpdir(), "inbox-plugin-root-"));
  mkdirSync(path.join(root, "bin"), { recursive: true });
  writeFileSync(
    path.join(root, "herdr-plugin.toml"),
    ['id = "rebuildup.inbox"', 'name = "Inbox"', 'version = "0.0.0-test"', ""].join("\n"),
  );
  const binLink = path.join(root, "bin", "inbox");
  try {
    symlinkSync(inboxBinary(), binLink);
  } catch {
    // Windows without symlink privilege: copy the runnable binary instead.
    copyFileSync(inboxBinary(), binLink);
  }
  cachedPluginRoot = root;
  return root;
}
