// Tauri IPC helpers — thin wrappers keeping Svelte components testable.
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

// Mirrors `haily_types::ResponseChunk`'s `#[serde(tag = "type", content = "data")]`
// envelope exactly — each variant's `data` shape differs (string vs. object vs.
// absent), so this MUST be a discriminated union, not one interface with an
// optional `data?: string`. The single `Chunk` type below is the sole definition;
// do not redeclare it elsewhere (this file previously had a second, incompatible
// copy inline in +page.svelte).
export interface TextChunk {
  type: 'Text';
  data: string;
}

/** Turn-ending failure — distinct from `TextChunk` so a consumer that accumulates
 * text can tell "replace/flag as error" apart from "append this too". Mirrors
 * `haily_types::ResponseChunk::Error`. */
export interface ErrorChunk {
  type: 'Error';
  data: string;
}

export interface ToolApprovalRequestChunk {
  type: 'ToolApprovalRequest';
  // `origin` is a server-derived, display-only "who is asking" label (e.g. "L0",
  // "L1:developer"). Optional to match `#[serde(default)]` on the Rust side — an
  // older payload without it is still valid. NEVER an auth input.
  // `reversible` (R4, phase 3): true when this prompt exists ONLY because the
  // per-turn destructive-op cap escalated a normally-`ReversibleWrite` delete —
  // the action IS journaled/undoable. false (or absent, pre-phase-3) means the
  // tool is genuinely `IrreversibleWrite`/`Blocked` on its own merits. Drives
  // whether the modal shows a "can't be undone" claim or a milder confirmation.
  data: {
    tool: string;
    args: string;
    approval_id: string;
    origin?: string | null;
    reversible?: boolean;
  };
}

export interface ToolResultChunk {
  type: 'ToolResult';
  // `reversible`/`journal_id` are additive fields mirroring Rust's `#[serde(default)]`
  // on `ResponseChunk::ToolResult` (R4, phase 3) — an older backend build's payload
  // without them still matches this shape once destructured with `??` fallbacks by
  // the consumer, so treat both as "may be absent", not required. `journal_id` is
  // non-null only when `reversible` is true AND the write's `post_state_version` had
  // already landed at emit time (M4 ordering guard) — see `tool_call.rs`. Snake_case
  // on purpose: this mirrors the wire's Rust field names exactly, no camelCase rename.
  data: { name: string; ok: boolean; reversible?: boolean; journal_id?: string | null };
}

export interface CompleteChunk {
  type: 'Complete';
}

export type Chunk = TextChunk | ErrorChunk | ToolApprovalRequestChunk | ToolResultChunk | CompleteChunk;

export interface ChunkPayload {
  session_id: string;
  chunk: Chunk;
}

/** Frontend-normalized shape of a pending approval, derived from a
 * `ToolApprovalRequestChunk` plus the session it arrived on. */
export interface PendingApproval {
  sessionId: string;
  approvalId: string;
  tool: string;
  args: string;
  /** Server-derived "who is asking" label (e.g. "L0", "L1:developer"), display-only. */
  origin?: string | null;
  /** True when this prompt is a cap-escalated but genuinely reversible action —
   * see `ToolApprovalRequestChunk.data.reversible`. Absent/false = truly final. */
  reversible?: boolean;
}

/** Send a message and return the session UUID. */
export async function sendMessage(message: string): Promise<string> {
  return invoke('send_message', { message });
}

/**
 * Cancel the in-flight turn for `sessionId`. Fires that turn's cancellation token on
 * the backend; the dispatch loop still emits its normal terminal chunk (`Complete` or
 * `Error`) afterward, so callers should rely on the existing `onChunk` handling to
 * close the bubble out rather than mutating UI state directly from this call's
 * result. Returns `false` (not a thrown error) if the turn already finished or
 * `sessionId` is unknown — callers should treat that as a no-op.
 */
export async function cancelTurn(sessionId: string): Promise<boolean> {
  return invoke('cancel_turn', { sessionId });
}

/**
 * Resolve a pending tool approval. `sessionId` must be the session the
 * `ToolApprovalRequest` chunk arrived on (`ChunkPayload.session_id`) — it is the
 * auth boundary on the backend, not `approvalId` alone. Returns `false` (not a
 * thrown error) if the approval was already resolved or belongs to a different
 * session; callers should treat that as a no-op rather than surface it as a failure.
 */
export async function resolveApproval(
  sessionId: string,
  approvalId: string,
  approved: boolean,
): Promise<boolean> {
  return invoke('approve_tool', { sessionId, approvalId, approved });
}

/** Subscribe to streaming response chunks from the backend. */
export async function onChunk(
  callback: (payload: ChunkPayload) => void,
): Promise<UnlistenFn> {
  return listen<ChunkPayload>('haily-chunk', (event) => callback(event.payload));
}

/** Return all stored preferences as a key→value map. */
export async function getPreferences(): Promise<Record<string, string>> {
  return invoke('get_preferences');
}

/** Persist a single preference. */
export async function setPreference(key: string, value: string): Promise<void> {
  return invoke('set_preference', { key, value });
}

/** One recorded connector write, as read back for the Safety tab's undo surface.
 * Mirrors `haily_db::queries::journal::ActionJournalRow` (camelCase over the wire). */
export interface JournalEntry {
  id: string;
  sessionId: string;
  toolName: string;
  toolTier: string;
  compensability: string;
  idempotencyKey: string;
  correlationRef: string;
  requestParams: string;
  preState: string | null;
  preStateVersion: string | null;
  postState: string | null;
  postStateVersion: string | null;
  readbackStatus: string;
  /** Raw plan JSON — surfaced verbatim for a `stuck` row so the user can act on it
   * manually; never parsed/re-rendered as anything richer (R4 does that). */
  compensationPlan: string | null;
  undoStatus: string;
  undoAttempts: number;
  createdAt: string;
  undoneAt: string | null;
  retentionExpiresAt: string;
  /** Owning connector manifest's content hash (Phase 6, additive) — `null` for a local-tool
   * row (no manifest) or one written before this column existed. Mirrors
   * `haily_db::queries::journal::ActionJournalRow::manifest_hash`. */
  manifestHash: string | null;
}

/**
 * Recent action-journal rows across every session this GUI instance has started.
 * `sessionIds` should be every session id seen so far in this run (there is no single
 * "current session" — each turn mints a fresh one, see `sendMessage`). Reuses the
 * backend's per-session query; an id with no rows just contributes nothing.
 */
export async function listJournal(sessionIds: string[]): Promise<JournalEntry[]> {
  return invoke('list_journal', { sessionIds });
}

/**
 * Write a consistent standalone copy of the database to `destPath` (Phase 6 manual
 * export — same `VACUUM INTO` mechanism the scheduled backup worker uses). Callers
 * should pick `destPath` via `@tauri-apps/plugin-dialog`'s `save()` and warn the user
 * first that the exported file is unencrypted and contains all local data — this
 * function performs no confirmation of its own.
 */
export async function exportDatabase(destPath: string): Promise<void> {
  return invoke('export_database', { destPath });
}

/** Mirrors `haily_types::WorkItemStatus` — a snapshot of one active work item. */
export interface WorkItemStatus {
  title: string;
  status: string;
  progress: number;
  phase?: string | null;
}

/**
 * Current active work items (queued/running/paused/interrupted), authoritative as of
 * the call. Call this on every (re)mount of the work-items panel: the live event
 * below is delivered over a latest-wins channel that best-effort drops intermediate
 * snapshots under load (see `onWorkItemsChanged`), so mount-time state must always
 * come from this fetch, never from accumulated event history alone.
 */
export async function listWorkItems(): Promise<WorkItemStatus[]> {
  return invoke('list_work_items');
}

/**
 * Subscribe to live work-item snapshot updates. The backend forwards these over a
 * dedicated `watch`-channel bridge (`haily-io::gui::GuiAdapter`'s `work_items_tx`)
 * that is intentionally separate from the bounded `haily-chunk` channel and is
 * latest-wins: a burst of updates collapses to only the most recent snapshot, and an
 * intermediate one may never reach this callback. Always pair this with a
 * `listWorkItems()` call on mount so a dropped snapshot self-corrects.
 */
export async function onWorkItemsChanged(
  callback: (items: WorkItemStatus[]) => void,
): Promise<UnlistenFn> {
  return listen<WorkItemStatus[]>('haily-work-items', (event) => callback(event.payload));
}

/** Mirrors `haily_tools::connector::manifest::ManifestDiff` (Rust struct, NO camelCase
 * rename — kept snake_case here to match exactly, rather than introducing a case mismatch
 * between this and its parent `ReapprovalState`, which also stays snake_case for the same
 * reason). Every tuple is `[old, new]`; `null` means that field did not change. */
export interface ManifestDiffDto {
  added_ops: string[];
  removed_ops: string[];
  changed_ops: { op_name: string; risk_tier: [string, string] | null; compensability: [string, string] | null }[];
  auth_scheme: [string, string] | null;
  auth_cred_ref: [string, string] | null;
  auth_header_name: [string, string] | null;
  auth_param_name: [string, string] | null;
  protocol_endpoint_suffix: [string, string] | null;
  protocol_envelope: [string, string] | null;
  protocol_methods: [string, string] | null;
  protocol_fault_rules: [string, string] | null;
  protocol_readback: [string, string] | null;
  protocol_context: [string, string] | null;
  protocol_prevalidate: [string, string] | null;
  /** (M1) Only populated when the manifest carries an `auth` section on either version. */
  base_url: [string, string] | null;
  allowed_ip_cidrs: [string[], string[]] | null;
}

/** Surfaced when a connector's live manifest version differs from the last version a human
 * explicitly acknowledged (`acknowledgeConnectorVersion`). Mirrors
 * `haily_app::connector_config::ReapprovalState`. */
export interface ReapprovalState {
  approved_version: string;
  live_version: string;
  diff: ManifestDiffDto;
}

/** One installed connector, for the config UI (Phase 7). Mirrors
 * `haily_app::connector_config::ConnectorSummary`. `cred_ref` is `null` when the manifest
 * declares no `auth` section — the credential form must not render in that case. */
export interface ConnectorSummary {
  id: string;
  connector_name: string;
  version: string;
  status: string;
  base_url_host: string;
  risk_tier: string;
  cred_ref: string | null;
  reapproval: ReapprovalState | null;
}

/** List installed connectors (latest version per connector, any status) with their
 * re-approval state. Read-only. */
export async function listConnectors(): Promise<ConnectorSummary[]> {
  return invoke('list_connectors');
}

/**
 * Set/rotate a connector's credential. Writes straight to the OS keyring (never SQLite) and
 * scrubs any overwritten plaintext's WAL/freelist residue server-side — the caller passes
 * the plain secret once, over the same in-process IPC channel every other command uses, and
 * it is never echoed back or persisted client-side.
 */
export async function setConnectorCredential(credRef: string, secret: string): Promise<void> {
  return invoke('set_connector_credential', { credRef, secret });
}

/**
 * Enable/disable a connector manifest version. Takes effect at the NEXT restart only — the
 * backend does not hot-reload the connector registry. Callers should surface that in the UI
 * rather than imply the toggle is instant.
 */
export async function setConnectorStatus(id: string, status: 'active' | 'disabled'): Promise<void> {
  return invoke('set_connector_status', { id, status });
}

/** Acknowledge a connector's live manifest version, clearing its `reapproval` banner. */
export async function acknowledgeConnectorVersion(connectorName: string, version: string): Promise<void> {
  return invoke('acknowledge_connector_version', { connectorName, version });
}
