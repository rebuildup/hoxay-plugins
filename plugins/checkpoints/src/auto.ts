/**
 * Auto-capture (work-unit checkpoints) for rebuildup.checkpoints —
 * Issue #74 / parent #4 T3.
 *
 * herdr's `agent_status` is coarser than an upstream T3 Code turn
 * boundary (no per-message signal exists), so what this produces is a
 * **work-unit checkpoint**: one hidden-ref capture per observed
 * working→idle/done transition, drained lazily.
 *
 * Two halves, both daemon-free (ADR-0002):
 *
 * - Hook side (`recordAgentStatus`): the `pane.agent_status_changed`
 *   event hook only writes a capture-pending mark into plugin state —
 *   no git, no socket writes beyond one `session.snapshot`. Fast and
 *   non-blocking by contract.
 * - Drain side (`drainPendingCaptures`): the next CLI invocation
 *   resolves each pending mark against a fresh snapshot, re-checks the
 *   generation guard (`state_change_seq` + `agent_session`, same
 *   convention as rebuildup.inbox), and runs the real
 *   `captureCheckpoint` for marks that still match. Stale marks are
 *   dropped, never captured under a wrong generation.
 *
 * `pane.closed` drops the pane's marks so a reused pane id cannot
 * inherit pending captures.
 */

import { GitError } from "../../../packages/git-core/src/index";
import {
  type CallOptions,
  type PaneInfo,
  reportPaneMetadata,
  type SessionSnapshot,
  sessionSnapshot,
} from "../../../packages/herdr-client/src/index";
import type { AgentInfo } from "../../../packages/herdr-client/src/snapshot";
import {
  CheckpointError,
  captureCheckpoint,
  type ResolvedRepo,
  resolveCheckpointRepo,
} from "./capture";
import {
  type AgentSessionRef,
  type AutoCapturePending,
  autoSessionEntries,
  type CheckpointEntry,
  loadState,
  type PaneAutoState,
  PLUGIN_ID,
  sessionKeyFor,
  updateState,
} from "./state";

/** Statuses that end a work unit and therefore pend a capture. */
const SETTLE_STATUSES = new Set(["idle", "done"]);
/** A return to active work invalidates any outstanding pending mark. */
const ACTIVE_STATUSES = new Set(["working"]);

// --- Generation guard (mirrors plugins/inbox/src/model.rs semantics) ---

export function agentSessionRef(agent: AgentInfo | undefined): AgentSessionRef | null {
  const session = agent?.agent_session;
  if (!session) return null;
  const ref: AgentSessionRef = {};
  if (typeof session.agent === "string") ref.agent = session.agent;
  if (typeof session.kind === "string") ref.kind = session.kind;
  if (typeof session.value === "string") ref.value = session.value;
  return Object.keys(ref).length > 0 ? ref : null;
}

function sameAgentSession(
  recorded: AgentSessionRef | null | undefined,
  current: AgentSessionRef | null,
): boolean {
  // A mark recorded without a fingerprint matches only while the pane
  // still has none — an agent session appearing later is a generation
  // change (same rule as inbox organize marks).
  if (!recorded) return current === null;
  // A recorded fingerprint that temporarily disappears is not treated
  // as a mismatch; state_change_seq still guards real generation
  // advances.
  if (!current) return true;
  return (
    (recorded.agent ?? null) === (current.agent ?? null) &&
    (recorded.value ?? null) === (current.value ?? null)
  );
}

function seqOf(agent: AgentInfo | undefined): number | null {
  return typeof agent?.state_change_seq === "number" ? agent.state_change_seq : null;
}

/**
 * True when the live agent still belongs to the generation the pending
 * mark was recorded against. A `state_change_seq` advance means the
 * agent did something after the settle transition — the pending mark is
 * stale and must not be captured. When either side lacks a seq the
 * agent_session fingerprint alone decides.
 */
export function pendingMatchesGeneration(
  pending: AutoCapturePending,
  agent: AgentInfo | undefined,
): boolean {
  if (!agent) return false;
  const liveSeq = seqOf(agent);
  if (pending.seq !== null && liveSeq !== null && liveSeq !== pending.seq) return false;
  return sameAgentSession(pending.agent_session, agentSessionRef(agent));
}

// --- Hook side ------------------------------------------------------------

export interface StatusTransitionResult {
  /** True when this delivery created/replaced a capture-pending mark. */
  pending: boolean;
  /** The agent_status the hook acted on (may differ from the event's). */
  status: string | null;
}

/**
 * Fold one `pane.agent_status_changed` delivery into the per-session
 * auto-capture map. Called inside `updateState` by the hook; must stay
 * synchronous and free of I/O.
 *
 * Transition rule: only an observed `working → idle|done` edge pends a
 * capture — the first status ever seen for a pane cannot prove the pane
 * was working, so it never pends (no fabricated work units). A return
 * to `working` clears any outstanding mark: the previous work unit is
 * superseded by new activity, and the next settle will re-pend.
 */
export function recordAgentStatus(
  entries: Record<string, PaneAutoState>,
  paneId: string,
  agent: AgentInfo | undefined,
  now: Date,
): StatusTransitionResult {
  const status = typeof agent?.agent_status === "string" ? agent.agent_status : null;
  if (status === null) return { pending: false, status: null };

  const paneState = entries[paneId] ?? {};
  entries[paneId] = paneState;
  const previous = paneState.last_status;
  paneState.last_status = status;

  if (SETTLE_STATUSES.has(status) && previous === "working") {
    paneState.pending = {
      status,
      seq: seqOf(agent),
      agent_session: agentSessionRef(agent),
      recorded_at: now.toISOString(),
    };
    return { pending: true, status };
  }
  if (ACTIVE_STATUSES.has(status)) {
    delete paneState.pending;
  }
  return { pending: false, status };
}

/** `pane.closed`: drop all auto-capture bookkeeping for the pane. */
export function clearPaneAuto(entries: Record<string, PaneAutoState>, paneId: string): void {
  delete entries[paneId];
}

// --- Drain side -----------------------------------------------------------

export interface AutoCaptureSkipped {
  pane_id: string;
  reason: string;
}

export interface AutoCaptureReport {
  /** Checkpoint ids produced by this drain, in capture order. */
  captured: string[];
  /** Pending marks evaluated and dropped without a capture. */
  skipped: AutoCaptureSkipped[];
  /** True when the herdr snapshot was unreachable; marks stay pending. */
  session_unreachable?: boolean;
}

interface DrainWorkItem {
  paneId: string;
  cwd: string;
}

function paneCwd(pane: PaneInfo, agent: AgentInfo | undefined): string | null {
  const cwd =
    pane.cwd ??
    (pane as { foreground_cwd?: string }).foreground_cwd ??
    agent?.cwd ??
    agent?.foreground_cwd;
  return typeof cwd === "string" && cwd !== "" ? cwd : null;
}

function emptyReport(): AutoCaptureReport {
  return { captured: [], skipped: [] };
}

/**
 * Drain capture-pending marks for one herdr session. Safe to call from
 * every CLI command: it is a no-op (one state read) when nothing is
 * pending, leaves marks untouched when the session is unreachable, and
 * never throws for per-pane capture failures — those are reported in
 * `skipped` and the pending mark is dropped (auto-capture is
 * best-effort; `create` remains the explicit path).
 */
export async function drainPendingCaptures(
  options: CallOptions,
  timeoutMs?: number,
): Promise<AutoCaptureReport> {
  const report = emptyReport();
  const sessionKey = sessionKeyFor(options);

  const pending = await loadState().then((state) => {
    const entries = state.auto?.[sessionKey];
    if (!entries) return null;
    const ids = Object.keys(entries).filter((id) => entries[id]?.pending !== undefined);
    return ids.length === 0 ? null : { entries, ids };
  });
  if (!pending) return report;

  const callOptions: CallOptions = { session: options.session, socketPath: options.socketPath };
  if (timeoutMs !== undefined) callOptions.timeoutMs = timeoutMs;

  let snapshot: SessionSnapshot;
  try {
    snapshot = await sessionSnapshot(callOptions);
  } catch {
    report.session_unreachable = true;
    return report;
  }

  const agents = snapshot.agents ?? {};
  const panes = snapshot.panes ?? [];
  const work: DrainWorkItem[] = [];

  // Phase 1 (locked): classify every pending mark. Capturable marks
  // have their pending flag cleared up front — a capture failure drops
  // the mark rather than retrying forever (bounded cost per call).
  await updateState((state) => {
    const entries = autoSessionEntries(state, sessionKey);
    for (const paneId of pending.ids) {
      const paneState = entries[paneId];
      const mark = paneState?.pending;
      if (!paneState || !mark) continue;

      const pane = panes.find((p) => p.pane_id === paneId);
      const agent = agents[paneId];
      if (!pane || !agent) {
        // Pane or agent generation is gone; the mark can never fire.
        delete paneState.pending;
        if (paneState.last_status === undefined) delete entries[paneId];
        report.skipped.push({ pane_id: paneId, reason: "pane_or_agent_gone" });
        continue;
      }
      if (!pendingMatchesGeneration(mark, agent)) {
        delete paneState.pending;
        report.skipped.push({ pane_id: paneId, reason: "stale_generation" });
        continue;
      }
      const cwd = paneCwd(pane, agent);
      if (cwd === null) {
        delete paneState.pending;
        report.skipped.push({ pane_id: paneId, reason: "no_pane_cwd" });
        continue;
      }
      paneState.last_status = agent.agent_status;
      delete paneState.pending;
      work.push({ paneId, cwd });
    }
  });

  // Phase 2 (unlocked): real captures. Git work must not hold the
  // state lock.
  const capturedEntries: Array<{ paneId: string; entry: CheckpointEntry }> = [];
  const gitOptions = timeoutMs === undefined ? {} : { timeoutMs };
  for (const item of work) {
    try {
      const resolved: ResolvedRepo = await resolveCheckpointRepo(item.cwd, gitOptions);
      const entry = await captureCheckpoint(resolved, { ...gitOptions, kind: "auto" });
      capturedEntries.push({ paneId: item.paneId, entry });
      report.captured.push(entry.id);
    } catch (err) {
      const reason =
        err instanceof GitError || err instanceof CheckpointError ? err.kind : "capture_failed";
      report.skipped.push({ pane_id: item.paneId, reason });
    }
  }

  if (capturedEntries.length === 0) return report;

  // Phase 3 (locked): persist captured entries, then project metadata.
  const repoCounts = await updateState((state) => {
    for (const { entry } of capturedEntries) {
      state.checkpoints[entry.id] = entry;
    }
    const counts = new Map<string, number>();
    for (const entry of Object.values(state.checkpoints)) {
      counts.set(entry.repo_root, (counts.get(entry.repo_root) ?? 0) + 1);
    }
    return counts;
  });

  for (const { paneId, entry } of capturedEntries) {
    try {
      await reportPaneMetadata(
        paneId,
        {
          checkpoint_latest: entry.id,
          checkpoint_count: String(repoCounts.get(entry.repo_root) ?? 1),
        },
        { source: PLUGIN_ID, ...options },
      );
    } catch {
      // Metadata projection is display-only; a failure must not fail
      // the CLI command or lose the captured checkpoint.
    }
  }

  return report;
}
