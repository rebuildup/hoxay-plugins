import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import net from "node:net";
import { hostname, tmpdir } from "node:os";
import path from "node:path";
import { inboxBinary } from "./helpers/inbox-bin";
import { installStub } from "./helpers/stub-bin";
import { envValue, herdrSocketEndpoint, testEnv } from "./helpers/test-env";

/**
 * Contract tests for the NATIVE rebuildup.inbox implementation (Rust,
 * Issue #291). These spawn the shipped binary — `plugins/inbox/bin/inbox`
 * (committed prebuilt, ADR-0035) with a cargo-built fallback — against the
 * same fake herdr socket protocol the TS suite used, so wire behavior is
 * verified end-to-end without Bun or a live session.
 *
 * Coverage mirrors the retired TS-internal tests at contract level:
 * usage errors, the fake-socket command flows (organize verbs, filters,
 * tracker links, stale-generation guards, gh env sanitization), the
 * capability document incl. additive `command_metadata` (#263), and the
 * non-TTY `tui` fallback, and TS↔Rust state compatibility (a state.json
 * written by the retired TS implementation is honored verbatim). Rust
 * unit coverage lives in crates/tui (safety helpers); the wider pure-
 * function surface is exercised end-to-end here. TS↔Rust output parity
 * is evidenced by the diff harness documented in
 * docs/research/inbox-native-rust.md.
 */

const INBOX_BIN = inboxBinary();

// --- fake herdr socket (same NDJSON RPC protocol as the TS suite) -----------

interface FakeAgent {
  pane_id: string;
  agent_status: string;
  state_change_seq: number;
  agent_session?: { agent: string; kind: string; value: string } | null;
  cwd?: string;
  terminal_title_stripped?: string;
}

interface FakePane {
  pane_id: string;
  workspace_id: string;
  tab_id: string;
  title?: string;
  cwd?: string;
}

interface FakeHerdr {
  socketPath: string;
  methods: string[];
  setAgents(agents: FakeAgent[]): void;
  setPanes(panes: FakePane[]): void;
  close(): Promise<void>;
}

async function fakeHerdr(): Promise<FakeHerdr> {
  const dir = mkdtempSync(path.join(tmpdir(), "inbox-fake-"));
  const socketPath = path.join(dir, "herdr.sock");
  const methods: string[] = [];
  let agents: FakeAgent[] = [
    { pane_id: "w1:p1", agent_status: "done", state_change_seq: 1 },
    { pane_id: "w1:p2", agent_status: "working", state_change_seq: 1 },
    { pane_id: "w1:p3", agent_status: "done", state_change_seq: 1 },
  ];
  let panes: FakePane[] = ["w1:p1", "w1:p2", "w1:p3"].map((id) => ({
    pane_id: id,
    workspace_id: "w1",
    tab_id: "w1:t1",
  }));

  const server = net.createServer((conn) => {
    let buffer = "";
    conn.on("data", (chunk: Buffer) => {
      buffer += chunk.toString("utf8");
      let newline = buffer.indexOf("\n");
      while (newline !== -1) {
        const line = buffer.slice(0, newline);
        buffer = buffer.slice(newline + 1);
        newline = buffer.indexOf("\n");
        if (!line.trim()) continue;
        const message = JSON.parse(line) as { id?: string; method?: string };
        if (message.method) methods.push(message.method);
        const result =
          message.method === "session.snapshot"
            ? { snapshot: { session: { name: "fake" }, panes, agents } }
            : {};
        conn.write(`${JSON.stringify({ id: message.id, result })}\n`);
      }
    });
  });
  await new Promise<void>((resolveListen, rejectListen) => {
    server.once("error", rejectListen);
    server.listen(herdrSocketEndpoint(socketPath), () => resolveListen());
  });
  return {
    socketPath,
    methods,
    setAgents(next: FakeAgent[]) {
      agents = next;
    },
    setPanes(next: FakePane[]) {
      panes = next;
    },
    close: () =>
      new Promise((resolveClose) => {
        server.close(() => {
          rmSync(dir, { recursive: true, force: true });
          resolveClose();
        });
      }),
  };
}

interface CliResult {
  status: number;
  json: Record<string, unknown> | null;
  stdout: string;
  stderr: string;
}

function cliEnv(stateDir: string, configDir: string): Record<string, string> {
  return testEnv({
    HERDR_PLUGIN_STATE_DIR: stateDir,
    HERDR_PLUGIN_CONFIG_DIR: configDir,
  });
}

// Async spawn is REQUIRED: the fake herdr server lives in this same
// Bun process, so a blocking spawnSync would deadlock every request.
async function runCli(args: string[], env: Record<string, string>): Promise<CliResult> {
  const proc = Bun.spawn([INBOX_BIN, ...args], {
    env,
    stdout: "pipe",
    stderr: "pipe",
  });
  const [stdout, stderr, status] = await Promise.all([
    new Response(proc.stdout).text(),
    new Response(proc.stderr).text(),
    proc.exited,
  ]);
  let json: Record<string, unknown> | null = null;
  try {
    json = JSON.parse(stdout) as Record<string, unknown>;
  } catch {
    json = null;
  }
  return { status, json, stdout, stderr };
}

function itemByPane(json: Record<string, unknown>, paneId: string) {
  const items = json.items as Array<Record<string, unknown>>;
  return items.find((item) => item.pane_id === paneId);
}

// --- usage errors (no socket needed: validation precedes any connect) -------

describe("inbox native CLI usage errors", () => {
  const env = testEnv();

  async function expectUsage(args: string[]): Promise<void> {
    const { status, stderr } = await runCli(args, env);
    expect(status).toBe(2);
    const doc = JSON.parse(stderr) as { ok?: boolean; error?: { code?: string } };
    expect(doc.ok).toBe(false);
    expect(doc.error?.code).toBe("usage");
  }

  test("every command rejects unknown options with exit 2", async () => {
    const cases: string[][] = [
      ["list", "--bogus"],
      ["settle", "p1", "--bogus"],
      ["snooze", "p1", "--until", "+1h", "--bogus"],
      ["restore", "p1", "--bogus"],
      ["reconcile", "--bogus"],
      ["watch", "--bogus"],
      ["capability", "--bogus"],
      ["_wake", "--until", "2026-01-01T00:00:00Z", "--bogus"],
    ];
    for (const args of cases) await expectUsage(args);
  });

  test("value flags require values and invalid values are usage errors", async () => {
    await expectUsage(["snooze", "p1", "--until"]);
    await expectUsage(["watch", "--interval"]);
    await expectUsage(["watch", "--interval", "abc"]);
    await expectUsage(["watch", "--interval", "-1"]);
    await expectUsage(["list", "--timeout", "abc"]);
  });

  test("extra positionals and wrong-flag usage are exit 2", async () => {
    await expectUsage(["list", "extra"]);
    await expectUsage(["settle", "p1", "extra"]);
    await expectUsage(["restore", "p1", "extra"]);
    await expectUsage(["reconcile", "extra"]);
    await expectUsage(["capability", "extra"]);
    await expectUsage(["settle", "p1", "--until", "+1h"]); // --until is snooze-only
    await expectUsage(["watch", "--until", "+1h"]);
  });

  test("order requires exactly one placement flag", async () => {
    await expectUsage(["order", "w1:p1"]);
    await expectUsage(["order", "w1:p1", "--first", "--last"]);
  });

  test("link requires exactly one tracker kind and a valid ref", async () => {
    await expectUsage(["link", "w1:p1"]);
    await expectUsage(["link", "w1:p1", "--pr", "not-a-ref"]);
    await expectUsage(["link", "w1:p1", "--pr", "owner/repo#1", "--issue", "o/r#2"]);
  });

  test("unknown command exits 2 with usage document", async () => {
    const { status, stderr } = await runCli(["bogus"], env);
    expect(status).toBe(2);
    const doc = JSON.parse(stderr) as { ok?: boolean; error?: { code?: string } };
    expect(doc.ok).toBe(false);
    expect(doc.error?.code).toBe("usage");
  });
});

// --- capability document -------------------------------------------------------

describe("inbox native capability document", () => {
  test("advertises commands, mutation_commands, and command_metadata", async () => {
    const { status, json } = await runCli(["capability"], testEnv());
    expect(status).toBe(0);
    expect(json?.contract).toBe("herdr-plugins.inbox/v1");
    expect(json?.plugin).toBe("rebuildup.inbox");
    expect(json?.version).toBe("0.8.0");

    const commands = json?.commands as string[];
    const mutations = json?.mutation_commands as string[];
    expect(commands).toContain("list");
    expect(commands).toContain("transitions");
    expect(commands).toContain("watch-pr");
    expect(mutations).toContain("settle");
    expect(mutations).toContain("reconcile");
    expect(mutations).toContain("watch-pr");
    expect(mutations).not.toContain("list");

    // Additive #263 metadata: every advertised command is described;
    // commands/mutation_commands stay the backward-compatible split.
    const meta = json?.command_metadata as Record<
      string,
      { kind?: string; idempotent?: boolean; coalesce?: boolean }
    >;
    expect(meta).toBeDefined();
    for (const cmd of [...commands, "help"]) {
      expect(meta[cmd]?.kind).toBe(mutations.includes(cmd) ? "mutation" : "read");
    }
    expect(meta.list?.coalesce).toBe(true);
    expect(meta.transitions?.coalesce).toBe(true);
    expect(meta.settle?.idempotent).toBe(true);
    // Daemon-class commands are excluded from retry semantics.
    expect(meta.watch?.idempotent).toBeUndefined();
    expect(meta["watch-pr"]?.idempotent).toBeUndefined();
    // Mutations are never coalesced.
    for (const cmd of mutations) {
      expect(meta[cmd]?.coalesce).toBeUndefined();
    }
  });
});

// --- fake-socket command contract ----------------------------------------------

describe("inbox native contract v1 (pin/archive/order)", () => {
  let herdr: FakeHerdr;
  let stateDir: string;
  let configDir: string;
  let env: Record<string, string>;

  beforeAll(async () => {
    herdr = await fakeHerdr();
    stateDir = mkdtempSync(path.join(tmpdir(), "inbox-state-"));
    configDir = mkdtempSync(path.join(tmpdir(), "inbox-config-"));
    env = cliEnv(stateDir, configDir);
  });

  afterAll(async () => {
    await herdr.close();
    rmSync(stateDir, { recursive: true, force: true });
    rmSync(configDir, { recursive: true, force: true });
  });

  const cli = (args: string[]) => runCli([...args, "--socket", herdr.socketPath], env);

  test("pin/unpin/archive/unarchive/order/restore flow", async () => {
    const list0 = await cli(["list"]);
    expect(list0.status).toBe(0);
    expect(list0.json?.contract).toBe("herdr-plugins.inbox/v1");
    expect(((list0.json?.counts ?? {}) as Record<string, number>).inbox).toBe(3);

    const pin = await cli(["pin", "w1:p2"]);
    expect(pin.status).toBe(0);
    expect(pin.json?.command).toBe("pin");
    expect(pin.json?.pinned).toBe(true);

    const list1 = await cli(["list"]);
    const items1 = list1.json?.items as Array<Record<string, unknown>>;
    expect(items1[0]?.pane_id).toBe("w1:p2");
    expect(items1[0]?.pinned).toBe(true);
    expect(((list1.json?.counts ?? {}) as Record<string, number>).pinned).toBe(1);

    const archive = await cli(["archive", "w1:p3"]);
    expect(archive.status).toBe(0);
    expect(archive.json?.organize).toMatchObject({ kind: "archived" });

    const list2 = await cli(["list"]);
    const p3 = itemByPane(list2.json as Record<string, unknown>, "w1:p3");
    expect(p3?.in_inbox).toBe(false);
    expect(p3?.organize).toMatchObject({ kind: "archived" });
    expect(((list2.json?.counts ?? {}) as Record<string, number>).inbox).toBe(2);
    expect(((list2.json?.counts ?? {}) as Record<string, number>).archived).toBe(1);

    const unarchive = await cli(["unarchive", "w1:p3"]);
    expect(unarchive.status).toBe(0);
    expect(unarchive.json?.organize).toBeNull();
    const list3 = await cli(["list"]);
    expect(itemByPane(list3.json as Record<string, unknown>, "w1:p3")?.in_inbox).toBe(true);

    const order = await cli(["order", "w1:p1", "--before", "w1:p3"]);
    expect(order.status).toBe(0);
    expect(order.json?.order).toBe(0);

    const list4 = await cli(["list"]);
    const visible = ((list4.json?.items ?? []) as Array<Record<string, unknown>>).filter(
      (i) => i.in_inbox,
    );
    expect(visible.map((i) => i.pane_id)).toEqual(["w1:p2", "w1:p1", "w1:p3"]);

    await cli(["settle", "w1:p3"]);
    const hiddenOrder = await cli(["order", "w1:p3", "--first"]);
    expect(hiddenOrder.status).not.toBe(0);
    const crossGroup = await cli(["order", "w1:p1", "--before", "w1:p2"]);
    expect(crossGroup.status).not.toBe(0);

    await cli(["restore", "w1:p2"]);
    await cli(["restore", "w1:p1"]);
    const list5 = await cli(["list"]);
    expect(((list5.json?.counts ?? {}) as Record<string, number>).pinned).toBe(0);
  });

  test("attention is never hidden by pin/archive/settle/snooze", async () => {
    herdr.setAgents([
      { pane_id: "w1:p1", agent_status: "done", state_change_seq: 5 },
      { pane_id: "w1:p2", agent_status: "working", state_change_seq: 5 },
      { pane_id: "w1:p3", agent_status: "done", state_change_seq: 5 },
    ]);
    await cli(["restore", "w1:p1"]);
    await cli(["archive", "w1:p1"]);
    herdr.setAgents([
      { pane_id: "w1:p1", agent_status: "blocked", state_change_seq: 5 },
      { pane_id: "w1:p2", agent_status: "working", state_change_seq: 5 },
      { pane_id: "w1:p3", agent_status: "done", state_change_seq: 5 },
    ]);
    const list = await cli(["list"]);
    const p1 = itemByPane(list.json as Record<string, unknown>, "w1:p1");
    expect(p1?.attention).toBe(true);
    expect(p1?.in_inbox).toBe(true);
    expect(p1?.organize).toMatchObject({ kind: "archived" });

    await cli(["pin", "w1:p2"]);
    await cli(["settle", "w1:p2"]);
    herdr.setAgents([
      { pane_id: "w1:p1", agent_status: "blocked", state_change_seq: 5 },
      { pane_id: "w1:p2", agent_status: "blocked", state_change_seq: 5 },
      { pane_id: "w1:p3", agent_status: "done", state_change_seq: 5 },
    ]);
    const list2 = await cli(["list"]);
    const p2 = itemByPane(list2.json as Record<string, unknown>, "w1:p2");
    expect(p2?.attention).toBe(true);
    expect(p2?.in_inbox).toBe(true);
    expect(p2?.pinned).toBe(true);
    expect(p2?.organize).toMatchObject({ kind: "settled" });
  });

  test("organize commands never touch execution surfaces", async () => {
    herdr.methods.length = 0;
    await cli(["pin", "w1:p1"]);
    await cli(["archive", "w1:p2"]);
    await cli(["order", "w1:p3", "--last"]);
    await cli(["unarchive", "w1:p2"]);
    await cli(["unpin", "w1:p1"]);
    await cli(["restore", "w1:p3"]);
    await cli(["reconcile"]);

    const unique = [...new Set(herdr.methods)];
    expect(unique.sort()).toEqual(["pane.report_metadata", "session.snapshot"]);
  });
});

describe("inbox native contract v1 (filters and tracker links)", () => {
  let herdr: FakeHerdr;
  let stateDir: string;
  let configDir: string;
  let env: Record<string, string>;

  beforeAll(async () => {
    herdr = await fakeHerdr();
    stateDir = mkdtempSync(path.join(tmpdir(), "inbox-state-"));
    configDir = mkdtempSync(path.join(tmpdir(), "inbox-config-"));
    env = cliEnv(stateDir, configDir);
    herdr.setPanes([
      { pane_id: "w1:p1", workspace_id: "w1", tab_id: "w1:t1", title: "fix login bug" },
      { pane_id: "w1:p2", workspace_id: "w1", tab_id: "w1:t1", title: "write docs" },
      { pane_id: "w2:p9", workspace_id: "ws2", tab_id: "w2:t1", title: "deploy pipeline" },
    ]);
    herdr.setAgents([
      {
        pane_id: "w1:p1",
        agent_status: "done",
        state_change_seq: 3,
        agent_session: { agent: "claude", kind: "session", value: "sess-1" },
        cwd: "/repo/app",
        terminal_title_stripped: "fix login bug",
      },
      { pane_id: "w1:p2", agent_status: "working", state_change_seq: 2 },
      { pane_id: "w2:p9", agent_status: "idle", state_change_seq: 1 },
    ]);
  });

  afterAll(async () => {
    await herdr.close();
    rmSync(stateDir, { recursive: true, force: true });
    rmSync(configDir, { recursive: true, force: true });
  });

  const cli = (args: string[]) => runCli([...args, "--socket", herdr.socketPath], env);
  const paneIds = (json: Record<string, unknown> | null) =>
    ((json?.items ?? []) as Array<Record<string, unknown>>).map((i) => i.pane_id);

  test("list filters: attention/organize/search/workspace, read-only", async () => {
    const all = await cli(["list"]);
    expect(all.status).toBe(0);
    expect(paneIds(all.json).sort()).toEqual(["w1:p1", "w1:p2", "w2:p9"]);
    expect(((all.json?.filters ?? {}) as Record<string, unknown>).attention).toBe(false);

    await cli(["settle", "w1:p2"]);
    const settled = await cli(["list", "--organize", "settled"]);
    expect(paneIds(settled.json)).toEqual(["w1:p2"]);
    expect(((settled.json?.counts ?? {}) as Record<string, number>).total).toBe(1);

    const none = await cli(["list", "--organize", "none"]);
    expect(paneIds(none.json).sort()).toEqual(["w1:p1", "w2:p9"]);

    herdr.setAgents([
      {
        pane_id: "w1:p1",
        agent_status: "blocked",
        state_change_seq: 4,
        agent_session: { agent: "claude", kind: "session", value: "sess-1" },
        cwd: "/repo/app",
      },
      { pane_id: "w1:p2", agent_status: "working", state_change_seq: 2 },
      { pane_id: "w2:p9", agent_status: "idle", state_change_seq: 1 },
    ]);
    const attention = await cli(["list", "--attention"]);
    expect(paneIds(attention.json)).toEqual(["w1:p1"]);
    const combined = await cli(["list", "--attention", "--search", "login", "--workspace", "w1"]);
    expect(paneIds(combined.json)).toEqual(["w1:p1"]);
    const miss = await cli(["list", "--search", "login", "--workspace", "ws2"]);
    expect(miss.json?.items).toEqual([]);

    const byCwd = await cli(["list", "--search", "/REPO/app"]);
    expect(paneIds(byCwd.json)).toEqual(["w1:p1"]);

    const again = await cli(["list"]);
    expect(itemByPane(again.json as Record<string, unknown>, "w1:p2")?.organize).toMatchObject({
      kind: "settled",
    });

    const bad = await cli(["list", "--organize", "bogus"]);
    expect(bad.status).toBe(2);

    await cli(["restore", "w1:p2"]);
    herdr.setAgents([
      {
        pane_id: "w1:p1",
        agent_status: "done",
        state_change_seq: 4,
        agent_session: { agent: "claude", kind: "session", value: "sess-1" },
        cwd: "/repo/app",
      },
      { pane_id: "w1:p2", agent_status: "working", state_change_seq: 2 },
      { pane_id: "w2:p9", agent_status: "idle", state_change_seq: 1 },
    ]);
  });

  test("link/unlink/links contract and honest errors", async () => {
    const link = await cli(["link", "w1:p1", "--pr", "owner/repo#42"]);
    expect(link.status).toBe(0);
    expect(link.json?.command).toBe("link");
    expect(((link.json?.links ?? []) as unknown[]).length).toBe(1);

    const linkIssue = await cli(["link", "w1:p1", "--issue", "owner/repo#7"]);
    expect(linkIssue.status).toBe(0);
    expect(((linkIssue.json?.links ?? []) as unknown[]).length).toBe(2);

    const list = await cli(["list"]);
    const p1 = itemByPane(list.json as Record<string, unknown>, "w1:p1");
    expect(((p1?.links ?? []) as Array<Record<string, unknown>>).map((l) => l.ref).sort()).toEqual([
      "owner/repo#42",
      "owner/repo#7",
    ]);
    expect(((list.json?.counts ?? {}) as Record<string, number>).linked).toBe(1);

    const links = await cli(["links", "w1:p1"]);
    expect(links.status).toBe(0);
    expect(((links.json?.links ?? []) as unknown[]).length).toBe(2);

    const relink = await cli(["link", "w1:p1", "--pr", "owner/repo#42"]);
    expect(((relink.json?.links ?? []) as unknown[]).length).toBe(2);

    const unlink = await cli(["unlink", "w1:p1", "--issue", "owner/repo#7"]);
    expect(unlink.status).toBe(0);
    expect((unlink.json?.removed as Record<string, unknown>)?.ref).toBe("owner/repo#7");
    expect(((unlink.json?.links ?? []) as unknown[]).length).toBe(1);

    const noPane = await cli(["link", "w9:p9", "--pr", "owner/repo#1"]);
    expect(noPane.status).not.toBe(0);
    const noLink = await cli(["unlink", "w1:p1", "--issue", "owner/repo#7"]);
    expect(noLink.status).not.toBe(0);
    const badRef = await cli(["link", "w1:p1", "--pr", "not-a-ref"]);
    expect(badRef.status).toBe(2);
    const both = await cli(["link", "w1:p1", "--pr", "owner/repo#1", "--issue", "o/r#2"]);
    expect(both.status).toBe(2);
    const noKind = await cli(["link", "w1:p1"]);
    expect(noKind.status).toBe(2);

    const verified = await cli([
      "link",
      "w1:p2",
      "--pr",
      "definitely-not-a-real-owner-zzz/repo#1",
      "--verify",
    ]);
    expect(verified.status).toBe(0);
    expect(((verified.json?.warnings ?? []) as string[]).length).toBeGreaterThan(0);
    expect(((verified.json?.links ?? []) as unknown[]).length).toBe(1);
    await cli(["unlink", "w1:p2", "--pr", "definitely-not-a-real-owner-zzz/repo#1"]);
  });

  test("stale links never attach to a different agent generation", async () => {
    herdr.setAgents([
      {
        pane_id: "w1:p1",
        agent_status: "done",
        state_change_seq: 4,
        agent_session: { agent: "claude", kind: "session", value: "sess-2" },
      },
      { pane_id: "w1:p2", agent_status: "working", state_change_seq: 2 },
      { pane_id: "w2:p9", agent_status: "idle", state_change_seq: 1 },
    ]);
    const list = await cli(["list"]);
    const p1 = itemByPane(list.json as Record<string, unknown>, "w1:p1");
    expect(p1?.links).toEqual([]);

    const relink = await cli(["link", "w1:p1", "--pr", "owner/repo#42"]);
    expect(relink.status).toBe(0);
    expect(((relink.json?.links ?? []) as unknown[]).length).toBe(1);
    await cli(["unlink", "w1:p1", "--pr", "owner/repo#42"]);

    await cli(["link", "w1:p2", "--issue", "owner/repo#9"]);
    await cli(["settle", "w1:p2"]);
    await cli(["restore", "w1:p2"]);
    const linksAfter = await cli(["links", "w1:p2"]);
    expect(((linksAfter.json?.links ?? []) as unknown[]).length).toBe(1);
    await cli(["unlink", "w1:p2", "--issue", "owner/repo#9"]);
  });

  test("link commands touch only the snapshot read surface", async () => {
    herdr.methods.length = 0;
    await cli(["link", "w1:p1", "--pr", "owner/repo#5"]);
    await cli(["links", "w1:p1"]);
    await cli(["unlink", "w1:p1", "--pr", "owner/repo#5"]);
    const unique = [...new Set(herdr.methods)];
    expect(unique).toEqual(["session.snapshot"]);
  });

  // Fake `gh` for the --verify env-sanitization test (same stub contract
  // as the TS suite; the Rust verify path must scrub identically).
  const GH_VERIFY_POSIX = `#!/bin/sh
if [ -n "$GH_DEBUG" ] || [ -n "$GH_REPO" ] || [ -n "$GH_FORCE_TTY" ] || [ -n "$GIT_DIR" ] || [ -n "$GIT_CONFIG_COUNT" ] || [ -n "$GIT_CONFIG_PARAMETERS" ]; then
  echo "env-leak-marker" >&2
  exit 1
fi
if [ "$FAKE_GH_FAIL" = "1" ]; then
  echo "credential-marker-9f8e7d6c" >&2
  exit 1
fi
exit 0
`;
  const GH_VERIFY_TS = `const NL = String.fromCharCode(10);
const poisoned = ["GH_DEBUG", "GH_REPO", "GH_FORCE_TTY", "GIT_DIR", "GIT_CONFIG_COUNT", "GIT_CONFIG_PARAMETERS"].some((k) => process.env[k]);
if (poisoned) {
  process.stderr.write("env-leak-marker" + NL);
  process.exit(1);
}
if (process.env.FAKE_GH_FAIL === "1") {
  process.stderr.write("credential-marker-9f8e7d6c" + NL);
  process.exit(1);
}
process.exit(0);
`;

  test("link --verify sanitizes gh env and never echoes gh stderr (#216)", async () => {
    const ghBin = mkdtempSync(path.join(tmpdir(), "inbox-fake-gh-"));
    try {
      installStub(ghBin, { name: "gh", posix: GH_VERIFY_POSIX, windows: GH_VERIFY_TS });

      const verifyEnv: Record<string, string> = { ...env };
      for (const key of Object.keys(verifyEnv)) {
        if (key !== "PATH" && key.toLowerCase() === "path") delete verifyEnv[key];
      }
      verifyEnv.PATH = `${ghBin}${path.delimiter}${envValue(env, "PATH") ?? ""}`;
      verifyEnv.GH_DEBUG = "api";
      verifyEnv.GH_REPO = "evil/repo";
      verifyEnv.GH_FORCE_TTY = "1";
      verifyEnv.GIT_DIR = "/tmp/poison-git-dir";
      verifyEnv.GIT_CONFIG_COUNT = "1";
      verifyEnv.GIT_CONFIG_PARAMETERS = "'core.sshCommand=evil'";
      const runVerify = (extra: Record<string, string>) =>
        runCli(
          ["link", "w1:p1", "--pr", "owner/repo#216", "--verify", "--socket", herdr.socketPath],
          { ...verifyEnv, ...extra },
        );

      const ok = await runVerify({});
      expect(ok.status).toBe(0);
      expect(ok.json?.warnings).toEqual([]);
      expect(((ok.json?.links ?? []) as unknown[]).length).toBeGreaterThan(0);

      const bad = await runVerify({ FAKE_GH_FAIL: "1" });
      expect(bad.status).toBe(0);
      const warnings = bad.json?.warnings as string[];
      expect(warnings).toHaveLength(1);
      expect(warnings[0]).toContain("link verification failed");
      expect(JSON.stringify(bad.json)).not.toContain("credential-marker");
      expect(JSON.stringify(bad.json)).not.toContain("env-leak-marker");
      expect(((bad.json?.links ?? []) as unknown[]).length).toBeGreaterThan(0);

      await cli(["unlink", "w1:p1", "--pr", "owner/repo#216"]);
    } finally {
      rmSync(ghBin, { recursive: true, force: true });
    }
  });
});

// --- transitions log + non-TTY tui fallback --------------------------------------

describe("inbox native transitions and tui fallback", () => {
  let herdr: FakeHerdr;
  let stateDir: string;
  let configDir: string;
  let env: Record<string, string>;

  beforeAll(async () => {
    herdr = await fakeHerdr();
    stateDir = mkdtempSync(path.join(tmpdir(), "inbox-state-"));
    configDir = mkdtempSync(path.join(tmpdir(), "inbox-config-"));
    env = cliEnv(stateDir, configDir);
  });

  afterAll(async () => {
    await herdr.close();
    rmSync(stateDir, { recursive: true, force: true });
    rmSync(configDir, { recursive: true, force: true });
  });

  test("transitions records organize ops with seq cursor", async () => {
    const cli = (args: string[]) => runCli([...args, "--socket", herdr.socketPath], env);
    // Only settle/snooze/restore record organize transitions (pin/unpin/
    // archive do not — cli.ts parity).
    await cli(["settle", "w1:p1"]);
    await cli(["restore", "w1:p1"]);

    const page = await cli(["transitions", "--limit", "50"]);
    expect(page.status).toBe(0);
    expect(page.json?.command).toBe("transitions");
    const items = page.json?.transitions as Array<Record<string, unknown>>;
    expect(items.length).toBeGreaterThanOrEqual(2);
    const seqs = items.map((t) => t.seq as number);
    expect(seqs).toEqual([...seqs].sort((a, b) => a - b));
    const head = page.json?.head_seq as number;
    expect(head).toBeGreaterThanOrEqual(seqs[seqs.length - 1] ?? 0);

    // --after is exclusive cursor pagination.
    const firstSeq = items[0]?.seq as number;
    const after = await cli(["transitions", "--after", String(firstSeq)]);
    const afterItems = after.json?.transitions as Array<Record<string, unknown>>;
    expect(afterItems.every((t) => (t.seq as number) > firstSeq)).toBe(true);

    await cli(["restore", "w1:p1"]);
    await cli(["unpin", "w1:p2"]);
  });

  test("tui without a TTY prints an honest plain list", async () => {
    // `tui` has no argv channel for --socket; the target comes from the
    // ambient HERDR_SOCKET_PATH the pane runtime exports. testEnv strips
    // HERDR_* by default, so re-add just the socket path for this spawn.
    const tuiEnv = { ...env, HERDR_SOCKET_PATH: herdr.socketPath };
    const proc = Bun.spawn([INBOX_BIN, "tui"], {
      env: tuiEnv,
      stdout: "pipe",
      stderr: "pipe",
      stdin: "ignore",
    });
    const [stdout, , status] = await Promise.all([
      new Response(proc.stdout).text(),
      new Response(proc.stderr).text(),
      proc.exited,
    ]);
    expect(status).toBe(0);
    expect(stdout).toContain("inbox — session=");
    expect(stdout).toContain("w1:p1");
    expect(stdout).toContain("interactive UI requires a TTY");
  });

  test("tui --help prints usage without touching the socket", async () => {
    const proc = Bun.spawn([INBOX_BIN, "tui", "--help"], {
      env: testEnv(),
      stdout: "pipe",
      stderr: "pipe",
    });
    const [stdout, , status] = await Promise.all([
      new Response(proc.stdout).text(),
      new Response(proc.stderr).text(),
      proc.exited,
    ]);
    expect(status).toBe(0);
    expect(stdout).toContain("inbox tui");
  });
});

// --- state compatibility -----------------------------------------------------
//
// tests/fixtures/inbox-state-ts-v2.json was written by the RETIRED TypeScript
// implementation (plugins/inbox/src/state.ts @ pre-#291 main, via updateState)
// with a placeholder session key. The Rust port must read it verbatim:
// schema v2 namespacing, generation marks, links, watch baselines, and the
// organize-transition log all carry over.

describe("state compatibility (TS-written state.json)", () => {
  test("rust binary honors organize state, links, watch, and transitions written by TS", async () => {
    const herdr = await fakeHerdr();
    const stateDir = mkdtempSync(path.join(tmpdir(), "inbox-state-"));
    const configDir = mkdtempSync(path.join(tmpdir(), "inbox-config-"));
    try {
      const sess = { agent: "claude", kind: "session", value: "sess-abc" };
      herdr.setAgents([
        { pane_id: "w1:p1", agent_status: "done", state_change_seq: 1, agent_session: sess },
        { pane_id: "w1:p2", agent_status: "working", state_change_seq: 1, agent_session: sess },
        { pane_id: "w1:p3", agent_status: "done", state_change_seq: 1 },
        { pane_id: "w1:p4", agent_status: "working", state_change_seq: 1 },
      ]);
      herdr.setPanes(
        ["w1:p1", "w1:p2", "w1:p3", "w1:p4"].map((id) => ({
          pane_id: id,
          workspace_id: "w1",
          tab_id: "w1:t1",
        })),
      );

      // Mirrors sessionKeyFor/session_key_for: canonicalize the socket
      // path. realpathSync can't lstat a unix socket, so resolve the
      // parent dir (which carries the /var→/private/var symlink) and
      // re-append the basename — matching std::fs::canonicalize output.
      const canonicalSocket = path.join(
        realpathSync(path.dirname(herdr.socketPath)),
        path.basename(herdr.socketPath),
      );
      const sessionKey = `${hostname()} ${canonicalSocket}`;
      const fixture = JSON.parse(
        readFileSync(path.join(import.meta.dir, "fixtures", "inbox-state-ts-v2.json"), "utf8"),
      ) as Record<string, Record<string, unknown>>;
      for (const section of ["sessions", "watch", "transitions"] as const) {
        const map = fixture[section];
        if (map && "SESSION_KEY_PLACEHOLDER" in map) {
          fixture[section] = { [sessionKey]: map.SESSION_KEY_PLACEHOLDER };
        }
      }
      mkdirSync(stateDir, { recursive: true });
      writeFileSync(path.join(stateDir, "state.json"), `${JSON.stringify(fixture, null, 2)}\n`);

      const env = cliEnv(stateDir, configDir);
      const cli = (args: string[]) => runCli([...args, "--socket", herdr.socketPath], env);

      const list = await cli(["list"]);
      expect(list.status).toBe(0);
      const p1 = itemByPane(list.json as Record<string, unknown>, "w1:p1");
      const p2 = itemByPane(list.json as Record<string, unknown>, "w1:p2");
      const p3 = itemByPane(list.json as Record<string, unknown>, "w1:p3");
      const p4 = itemByPane(list.json as Record<string, unknown>, "w1:p4");
      expect(p1?.organize).toMatchObject({ kind: "settled" });
      expect(p1?.in_inbox).toBe(false);
      expect(p1?.links).toEqual(
        expect.arrayContaining([expect.objectContaining({ ref: "owner/repo#12" })]),
      );
      expect(p2?.organize).toMatchObject({ kind: "snoozed" });
      expect(p2?.in_inbox).toBe(false);
      expect(p3?.organize).toMatchObject({ kind: "archived" });
      expect(p3?.pinned).toBe(true);
      expect(p4?.in_inbox).toBe(true);
      expect(p4?.links).toEqual(
        expect.arrayContaining([expect.objectContaining({ ref: "owner/repo#34" })]),
      );

      const transitions = await cli(["transitions", "--limit", "50"]);
      expect(transitions.status).toBe(0);
      const items = transitions.json?.transitions as Array<Record<string, unknown>>;
      expect(items.map((t) => t.kind)).toEqual(["settled", "snoozed", "cleared"]);
      expect(items[2]?.cause).toBe("pane_closed");
    } finally {
      await herdr.close();
      rmSync(stateDir, { recursive: true, force: true });
      rmSync(configDir, { recursive: true, force: true });
    }
  });
});
