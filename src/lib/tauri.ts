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

/** Handle-only chunk for a model-projected view (View Engine Phase A) — the bulk `DataView`
 * payload never rides the chat stream; the GUI fetches it separately via `getView(view_id)`.
 * Mirrors `haily_types::ResponseChunk::ViewRef`. A text-only channel (Telegram/CLI/ACP)
 * renders this as a short "[view] {entity}" line instead of opening a pane. */
export interface ViewRefChunk {
  type: 'ViewRef';
  data: { view_id: string; entity: string; provenance: ViewProvenance };
}

/** Which tier/model produced this L0 turn (Auto Model Routing R1, transparency
 * invariant) — a NEW additive variant, never a change to `CompleteChunk` (adjacently-
 * tagged serde breaks on an existing fieldless variant gaining a payload). Emitted once,
 * immediately before `Complete`, ONLY when server-side routing is enabled; `badge` is
 * `undefined`/absent on a legacy (pre-phase-5) payload. */
export interface TurnMetaChunk {
  type: 'TurnMeta';
  data: { badge?: string | null };
}

export type Chunk =
  | TextChunk
  | ErrorChunk
  | ToolApprovalRequestChunk
  | ToolResultChunk
  | CompleteChunk
  | ViewRefChunk
  | TurnMetaChunk;

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

/** Judgment depth for a turn. `deep` buys multi-stream judgment at ~3–5× cost. */
export type DepthMode = 'quick' | 'normal' | 'deep';

/**
 * Persist the depth toggle. Takes effect on the next message; the backend never
 * auto-escalates to `deep` — it is only ever set by this explicit action or a genuine
 * user-message phrase. An unknown value is normalized to `normal` server-side.
 */
export async function setDepth(mode: DepthMode): Promise<void> {
  return invoke('set_depth', { mode });
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

/**
 * Re-read LLM preferences and hot-swap the active backend at the next turn boundary.
 * Returns the active provider name (`'llama.cpp'` / `'cloud'` / `'unconfigured'`) so the
 * caller can distinguish a real model load from a silent "unconfigured" fallback — the
 * router never errors on load, only when a message is sent. Mirrors the Rust
 * `reload_llm` command (`src-tauri/src/lib.rs`).
 */
export async function reloadLlm(): Promise<string> {
  return invoke('reload_llm');
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

/** Mirrors `haily_types::ProactiveCardKind`'s `#[serde(tag = "type", content = "data")]`
 * envelope — same discriminated-union shape as `Chunk` above, for the same reason
 * (each variant's `data` differs). */
export type ProactiveCardKind =
  | { type: 'MorningBrief'; data: { text: string } }
  | { type: 'Alert'; data: { title: string; body: string; urgent: boolean } }
  | { type: 'ReminderFired'; data: { reminder_id: string; title: string } };

/** Mirrors `haily_types::ProactiveCard` — one discrete proactive event (morning brief,
 * alert, or fired reminder) for the dedicated card panel (phase 08), distinct from the
 * chat stream. */
export interface ProactiveCard {
  id: string;
  created_at: string;
  kind: ProactiveCardKind;
}

/**
 * Subscribe to live proactive-card snapshots. The backend forwards these over a
 * dedicated `watch`-channel bridge (`haily-io::gui::GuiAdapter`'s `proactive_tx`),
 * intentionally separate from the bounded `haily-chunk` channel so a burst of
 * proactive events can never compete with (or block behind) in-flight chat chunks —
 * mirrors `onWorkItemsChanged`'s channel discipline exactly.
 *
 * Unlike work-items, the payload here is NOT a full authoritative snapshot re-fetched
 * on demand: there is no `list_*` reconcile command for proactive events (they are
 * discrete, not a single replaceable state), so delivery is best-effort by design —
 * the backend already accumulates/caps cards per kind before forwarding (see
 * `GuiProactiveReceiver`'s doc comment), but a card CAN still be lost if the frontend
 * was never mounted to observe it. Callers should not assume every event that ever
 * fired is eventually delivered.
 */
export async function onProactiveCards(
  callback: (cards: ProactiveCard[]) => void,
): Promise<UnlistenFn> {
  return listen<ProactiveCard[]>('haily-proactive-cards', (event) => callback(event.payload));
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

// ---------------------------------------------------------------------------
// Phase 11a — Channel Event Backbone (GUI cockpit read/action surface).
// The Svelte components (RunTimeline, DiffViewer, SkillsBrowser, ApprovalsQueue, ChannelsPanel)
// that CONSUME these wrappers land in P11b. `WorkspacePanel` (its original name) was absorbed
// into `components/workspaces/WorkspacesList.svelte` by Unified Chat UI phase 10.
// ---------------------------------------------------------------------------

/** Mirrors `haily_types::RunEvent`'s `#[serde(tag = "type", content = "data")]` envelope
 * exactly — the ordered, non-coalescing pipeline event stream. A discriminated union for
 * the same reason as `Chunk`: each variant's `data` shape differs. UNTRUSTED content
 * (`StageOutput.chunk`, `GateResult.decisive`, `DiffAvailable.file`, `PlanReady.plan_path`)
 * is already tag-stripped server-side at the delivery chokepoint — render it as inert text,
 * never as HTML/markup. */
export type RunEvent =
  | { type: 'RunStarted'; data: { run_id: string; work_item_id: string } }
  | { type: 'StageStarted'; data: { run_id: string; stage: string; tier?: string | null } }
  | { type: 'StageOutput'; data: { run_id: string; seq: number; chunk: string } }
  | { type: 'GateResult'; data: { run_id: string; gate: string; pass: boolean; decisive: string } }
  | { type: 'Retry'; data: { run_id: string; attempt: number } }
  | { type: 'Escalation'; data: { run_id: string; from: string; to: string } }
  | { type: 'DiffAvailable'; data: { run_id: string; file: string } }
  | { type: 'ApprovalNeeded'; data: { run_id: string; approval_id: string } }
  | { type: 'PlanReady'; data: { run_id: string; plan_path: string } }
  | { type: 'RunPaused'; data: { run_id: string; reason: string } }
  | { type: 'RunComplete'; data: { run_id: string; outcome: string } };

/** One `haily-run-events` payload: the session the run belongs to plus the event. */
export interface RunEventPayload {
  session_id: string;
  event: RunEvent;
}

/**
 * Subscribe to the ordered pipeline `RunEvent` stream. Delivered over a dedicated,
 * BOUNDED, ordered `mpsc` bridge (`haily-io::gui::GuiRunEventReceiver`) — NOT the
 * latest-wins `watch` channels the work-item/proactive panels use — so events arrive in
 * full and in order, never coalesced. A build log depends on this: `onWorkItemsChanged`'s
 * "reconcile on mount" caveat does NOT apply here; every event is delivered exactly once.
 */
export async function onRunEvents(
  callback: (payload: RunEventPayload) => void,
): Promise<UnlistenFn> {
  return listen<RunEventPayload>('haily-run-events', (event) => callback(event.payload));
}

// ---------------------------------------------------------------------------
// View Engine Phase A — self-describing, read-only `DataView` snapshots rendered by the GUI
// workspace pane (`src/lib/components/view/`). A view is fetched whole by id (`getView`),
// NEVER streamed incrementally — see `ViewRefChunk` above for the chat-stream handle.
// `schema`/`records` may hold MODEL-AUTHORED strings (`LlmProjected` provenance): treat every
// value as untrusted display text and never interpolate it into HTML/attributes without going
// through `src/lib/data-view.ts`'s scheme-allowlist helper first.
// ---------------------------------------------------------------------------

/** Mirrors `haily_types::EnumVariant`. */
export interface EnumVariant {
  value: string;
  label: string;
}

/** Mirrors `haily_types::FieldType`'s `#[serde(tag = "type", content = "data")]` envelope — a
 * discriminated union for the same reason as `Chunk`/`RunEvent`: unit variants carry no `data`
 * key at all (adjacently-tagged serde omits it when there's no content), so this is NOT one
 * interface with an optional `data?`. */
export type FieldType =
  | { type: 'Text' }
  | { type: 'LongText' }
  | { type: 'Int' }
  | { type: 'Float' }
  | { type: 'Money'; data: { currency: string } }
  | { type: 'Bool' }
  | { type: 'Date' }
  | { type: 'DateTime' }
  | { type: 'Enum'; data: { variants: EnumVariant[] } }
  | { type: 'Reference'; data: { entity: string } }
  | { type: 'Tags' }
  | { type: 'Email' }
  | { type: 'Phone' }
  | { type: 'Url' }
  | { type: 'Opaque' };

/** Mirrors `haily_types::FieldDef`. `writable` is always `false` in View Engine Phase A
 * (read-only views only) — kept as a real field so a future writable-field phase needs no
 * reshape. */
export interface FieldDef {
  name: string;
  label: string;
  ftype: FieldType;
  writable: boolean;
  required: boolean;
  help: string | null;
}

/** Mirrors `haily_types::ProjectionKind` — a bare-string enum on the wire (no `#[serde(tag)]`
 * on the Rust side for a unit-only enum means it serializes as a plain string, unlike
 * `FieldType` above). An unrecognized/future kind (or one this build's renderer doesn't yet
 * implement) MUST be treated as `Table` — see `src/lib/data-view.ts`'s
 * `normalizeProjectionKind`. */
export type ProjectionKind = 'Table' | 'Cards' | 'Kanban' | 'Calendar' | 'Chart';

/** Mirrors `haily_types::ProjectionSpec`. */
export interface ProjectionSpec {
  kind: ProjectionKind;
  binding: string | null;
}

/** Mirrors `haily_types::ViewProvenance` — same bare-string wire encoding as `ProjectionKind`. */
export type ViewProvenance = 'Registry' | 'LlmProjected';

/** Mirrors `haily_types::DataView` — a read-only, self-describing latest-snapshot fetched via
 * `getView`. `records` values are keyed by `schema[].name` and typed per the matching
 * `FieldDef.ftype`; render every value through `src/lib/data-view.ts`'s formatters, never
 * `{@html}`. */
export interface DataView {
  view_id: string;
  entity: string;
  schema: FieldDef[];
  records: Record<string, unknown>[];
  projections: ProjectionSpec[];
  active: ProjectionSpec;
  total: number | null;
  cursor: string | null;
  provenance: ViewProvenance;
}

/**
 * Fetch a previously-produced `DataView` snapshot by id. Returns `null` (not a thrown error)
 * for an unknown/evicted `view_id` — the workspace pane renders "view expired" for that case
 * rather than treating it as a fetch failure. Mirrors the `get_view` Tauri command.
 */
export async function getView(viewId: string): Promise<DataView | null> {
  return invoke('get_view', { viewId });
}

/**
 * Record a GUI-originated view telemetry event — the Phase-B go/no-go signal (design §14).
 * `sessionId` must be the session the originating `ViewRefChunk` arrived on
 * (`ChunkPayload.session_id`) — mirrors `resolveApproval`'s auth-boundary contract; it is NOT
 * part of `DataView`/`ViewRefChunk` itself. `detail`'s meaning is keyed by `kind`: the
 * switched-to kind for `projection_switched`, `'+'`/`'-'` for `usefulness`, free-text intent
 * for `edit_demand`. An `edit_demand` with an empty/absent `detail` is silently dropped
 * server-side — "a click alone is not demand" — never surfaced as an error here.
 */
export async function recordViewIntent(
  viewId: string,
  sessionId: string,
  kind: 'viewed' | 'projection_switched' | 'usefulness' | 'edit_demand',
  detail?: string,
): Promise<void> {
  return invoke('record_view_intent', { viewId, sessionId, kind, detail: detail ?? null });
}

/** Which kind of coding-pipeline run to launch (Pipeline Activation & Wiring phase 3).
 * `'plan'` runs scout→design→write→approval; `'build'` runs build→verify→ship on the
 * given task description. Mirrors the two GUI-reachable arms of the Rust
 * `haily_core::pipeline::RunKind` enum accepted by `start_coding_run` (its
 * `PlanThenBuild` variant has no toggle position in this form yet). */
export type CodingRunKind = 'plan' | 'build';

/**
 * Launch a coding-pipeline run bound to a freshly minted session id — the GUI's own
 * trigger onto the P1 launch entrypoint (`haily_app::launch_coding_run`), reachable
 * anywhere a chat turn already is. Inherits the SAME `ApprovalGate` + kill switch as any
 * other turn; this call never bypasses approval. The launched run's events stream into
 * the existing `onRunEvents`/`RunTimeline` — no new subscription is needed. `repoPath`
 * of `null` falls back server-side to the `coding.default_repo` preference; if neither
 * resolves (or the repo is not a git repository), the call rejects — surface that as an
 * inline error, not a launch.
 */
export async function startCodingRun(
  kind: CodingRunKind,
  task: string,
  repoPath: string | null,
  depth: DepthMode,
): Promise<void> {
  return invoke('start_coding_run', { kind, task, repoPath, depth });
}

/** One skill row for the cockpit skills browser. Mirrors `haily_app::cockpit::SkillView`.
 * `source` is `"authored"` (trusted kit-pack — no confidence/use lifecycle) or
 * `"synthesized"` (EMA/decay lifecycle; confidence/use_count/last_used_at populated). */
export interface SkillView {
  name: string;
  source: 'authored' | 'synthesized';
  description: string;
  kind: string | null;
  confidence: number | null;
  use_count: number | null;
  last_used_at: string | null;
  enabled: boolean;
  pinned: boolean;
}

/** Authored + synthesized skills with their persisted enable/pin state. Read-only. */
export async function listSkills(): Promise<SkillView[]> {
  return invoke('list_skills');
}

/** Enable/disable a skill. Persists the admin state (enforcement is wired in P11b — see the
 * backend `cockpit` module doc; mirrors the connector-status persist-then-consume pattern). */
export async function setSkillEnabled(name: string, enabled: boolean): Promise<void> {
  return invoke('set_skill_enabled', { name, enabled });
}

/** Pin/unpin a skill. Persists the admin state (enforcement deferred — see `setSkillEnabled`). */
export async function pinSkill(name: string, pinned: boolean): Promise<void> {
  return invoke('pin_skill', { name, pinned });
}

/** One active coding workspace. Mirrors `haily_app::cockpit::WorkspaceView`.
 * `sandbox_enforcing === false` is the `NullSandbox` warning: execution is NOT isolated and
 * requires per-work-root first-exec approval — the panel must surface it prominently.
 *
 * `branch`/`worktree_path` are git-specific and MUST only ever be rendered under Settings →
 * Advanced (Unified Chat UI phase 10, D6) — the default Workspaces screen's status/copy is a
 * pure function of the other fields below, never these two. `resumable` is computed
 * server-side from the SAME guard `resume_run` itself enforces (status/reason-class AND the
 * worktree still existing) — never re-derive it from `run_status` alone. */
export interface WorkspaceView {
  id: string;
  session_id: string;
  repo_path: string;
  branch: string;
  worktree_path: string;
  work_item_id: string | null;
  created_at: string;
  dirty: boolean;
  /** Count of changed paths — `0` when clean or `worktree_reclaimed`. */
  changed_file_count: number;
  sandbox_kind: string;
  sandbox_enforcing: boolean;
  /** The pipeline run driving this workspace, if known yet. `null` before any run is linked. */
  run_id: string | null;
  /** Originating task text, for the row's "Bản làm việc riêng cho {task}" label. */
  task: string | null;
  /** Raw `pipeline_runs.status` of the linked run — `null` if none is linked yet. */
  run_status: string | null;
  /** Set only when `run_status === 'paused'`. */
  pause_reason_class: string | null;
  /** `true` when the worktree directory itself is gone (already applied+removed, or GC'd) while
   * the workspace row is still active — the "đã dọn dẹp" state. */
  worktree_reclaimed: boolean;
  /** Whether "Tiếp tục" (`resumeRun`) should be offered — server-computed, see the interface doc. */
  resumable: boolean;
}

/** Active coding workspaces with dirty status and host sandbox posture. Read-only. */
export async function listWorkspaces(): Promise<WorkspaceView[]> {
  return invoke('list_workspaces');
}

/**
 * Discard a coding workspace (revert worktree, remove it, delete branch, soft-delete row).
 * `sessionId` MUST be the workspace's own `session_id` (from its `WorkspaceView`) — it is
 * the auth boundary; a foreign id returns `false` (a no-op), never discarding another
 * session's workspace. Returns `false` (not a thrown error) if no active workspace matched.
 */
export async function discardWorkspace(id: string, sessionId: string): Promise<boolean> {
  return invoke('discard_workspace', { id, sessionId });
}

/**
 * The unified diff of a workspace's worktree against HEAD, for the DiffViewer's read side.
 * `sessionId` is the same auth boundary as `discardWorkspace`. Returns `null` for an
 * unknown/foreign id. The text is UNTRUSTED repo content (capped server-side) — render it
 * as inert data, never as markup. ACCEPTING changes is a separate action that routes
 * through the existing `worktree_apply` approval via `resolveApproval` — this is view-only.
 */
export async function workspaceDiff(id: string, sessionId: string): Promise<string | null> {
  return invoke('workspace_diff', { id, sessionId });
}

/** One in-flight approval in the unified queue, as read back from the backend broker.
 * Mirrors `haily_core::PendingApproval` (snake_case over the wire). Distinct from the
 * frontend-normalized `PendingApproval` above (which is built from a `ToolApprovalRequest`
 * chunk and carries the tool/args): this is a RECONCILE snapshot — the tool name/args live
 * in the chunk the frontend already received (correlate by `approval_id`). `session_id` is
 * the auth boundary: only that session may resolve it via `resolveApproval`. */
export interface QueuedApproval {
  approval_id: string;
  session_id: string;
  created_at: string;
}

/**
 * The PENDING set of the unified approvals queue — every in-flight approval across all
 * channels. Use this to reconcile which approvals are still live (prune resolved ones);
 * the descriptive payload comes from the `ToolApprovalRequestChunk` stream, and history
 * from `listJournal`. Resolve one via `resolveApproval` (session-auth enforced backend-side).
 */
export async function listApprovals(): Promise<QueuedApproval[]> {
  return invoke('list_approvals');
}

// ---------------------------------------------------------------------------
// Mobile Thin-Client plan phase 2b — pairing QR, OOB confirm-on-pair (M4), devices panel,
// status banners. Every wrapper below invokes a command registered ONLY behind the Rust
// `mobile-server` feature (see `src-tauri/Cargo.toml`); a build without that feature simply
// has no matching command, so these calls reject with a generic "command not found" error —
// callers must already treat every mobile_* call as fallible (existing try/catch convention),
// there is no separate "is this feature compiled in" probe.
// ---------------------------------------------------------------------------

/** Mirrors `haily_types::PairingQr` — the payload encoded into the pairing QR image. */
export interface PairingQr {
  host: string;
  port: number;
  cert_fingerprint: string;
  pairing_code: string;
  expires_at: string;
}

/** Mint a fresh pairing code and its QR payload. Interactive confirm mode (M4): the phone's
 * `/pair` request blocks server-side until a matching `confirmPair` call resolves it. */
export async function mobilePairingQr(deviceName?: string): Promise<PairingQr> {
  return invoke('mobile_pairing_qr', { deviceName: deviceName ?? null });
}

/** One pairing request still awaiting the desktop's out-of-band decision (M4). Mirrors
 * `haily_app::PendingPairView`. */
export interface PendingPair {
  code: string;
  device_name: string;
}

/**
 * Every pairing request still awaiting confirmation. POLLED by the caller (there is no push
 * event for a newly-arrived pairing request — see `haily_app::mobile_admin`'s module doc) —
 * call this on an interval while the pairing screen is open.
 */
export async function mobilePendingPairs(): Promise<PendingPair[]> {
  return invoke('mobile_pending_pairs');
}

/** Approve or deny a pending pairing request (M4). Returns `false` (not a thrown error) for an
 * unknown/already-resolved code — treat that as a no-op. */
export async function mobileConfirmPair(code: string, approve: boolean): Promise<boolean> {
  return invoke('mobile_confirm_pair', { code, approve });
}

/** One paired device row. Mirrors `haily_app::DeviceView`. */
export interface MobileDevice {
  device_id: string;
  device_name: string;
  created_at: string;
  last_seen_at: string | null;
}

/** Every non-revoked paired device, most-recently-paired first. */
export async function mobileListDevices(): Promise<MobileDevice[]> {
  return invoke('mobile_list_devices');
}

/** Revoke a paired device — soft-revokes it AND ends its live connection immediately. */
export async function mobileRevokeDevice(deviceId: string): Promise<void> {
  return invoke('mobile_revoke_device', { deviceId });
}

/** Mirrors `haily_app::MobileStatusView` — the panel's status banners. `running` is a
 * best-effort loopback liveness probe (see the Rust doc comment for why nothing stronger is
 * observable without editing the P2a server internals). */
export interface MobileStatus {
  enabled: boolean;
  running: boolean;
  tailnet_present: boolean;
  lan_opt_in: boolean;
  port: number;
}

/** Status banners: enabled/running/tailnet-absent/LAN-opt-in. */
export async function mobileServerStatus(): Promise<MobileStatus> {
  return invoke('mobile_server_status');
}

/**
 * Force TLS identity regeneration (m5). The CALLER must warn the user first that every
 * already-paired device's pinned LAN fingerprint will mismatch until it re-pairs — this
 * function performs no confirmation of its own. Returns the new fingerprint.
 */
export async function mobileRegenerateCert(): Promise<string> {
  return invoke('mobile_regenerate_cert');
}

// ---------------------------------------------------------------------------
// Unified Chat UI phase 2 (D1) — data-driven slash-command registry: built-ins +
// authored + gate-filtered synthesized skills, unioned and name-sorted. Consumed by the
// slash palette + ＋ menu (phase 3).
// ---------------------------------------------------------------------------

/** Which built-in pipeline/control action a slash command maps to. Mirrors
 * `haily_app::slash_registry::BuiltInKind` — `'pass_through'` covers every built-in that
 * resolves to an ordinary chat turn (most non-`plan`/`build` commands; `/help`/`/undo`/
 * `/writes`/`/kill`/`/settings` are intercepted earlier on CLI/Telegram and never reach the
 * dispatch-layer registry there, but still appear here for the GUI palette). */
export type BuiltInKind = 'plan' | 'build' | 'pass_through';

/** Mirrors `haily_app::slash_registry::SlashAction`'s `#[serde(tag = "type", content = "data")]`
 * envelope — a discriminated union for the same reason as `Chunk`/`RunEvent`. `SkillTurn`
 * carries the underlying skill's OWN name (not necessarily the same as the command's slugified
 * `name` above it, if the name was normalized or shadow-qualified). */
export type SlashActionDto =
  | { type: 'BuiltIn'; data: BuiltInKind }
  | { type: 'SkillTurn'; data: string };

/** Which store a command's underlying skill came from — badge the palette entry
 * accordingly. Mirrors `haily_app::slash_registry::SlashSource`. */
export type SlashSource = 'built_in' | 'authored' | 'synthesized';

/** One command in the merged registry. Mirrors `haily_app::slash_registry::SlashCommand`. */
export interface SlashCommand {
  name: string;
  description: string;
  arg_hint: string | null;
  example: string | null;
  action: SlashActionDto;
  source: SlashSource;
}

/** The current slash-command registry: built-ins + enabled authored + enabled synthesized
 * skills, name-sorted. Rebuilds opportunistically server-side before returning, so a skill
 * enabled/edited moments ago is reflected without waiting for the next chat message. */
export async function listSlashCommands(): Promise<SlashCommand[]> {
  return invoke('list_slash_commands');
}

// ---------------------------------------------------------------------------
// Unified Chat UI phase 8 (D4) — structured skill editor + versioning. Mirrors
// `haily_kms::skill_editor`. Local-GUI-only: never bridged to mobile.
// ---------------------------------------------------------------------------

/** Which store a skill's content lives in. Mirrors `haily_kms::skill_editor::SkillEditKind`. */
export type SkillEditKind = 'authored' | 'synthesized';

/** The 4-field structured shape both edit paths render to/from markdown. Mirrors
 * `haily_kms::skill_editor::SkillDraft`. */
export interface SkillDraft {
  procedure: string;
  success_conditions: string;
  forbidden_actions: string;
  required_from_user: string;
}

/** One skill's editable view. Mirrors `haily_kms::skill_editor::SkillDetail`. */
export interface SkillDetail {
  name: string;
  kind: SkillEditKind;
  draft: SkillDraft;
}

/** One recorded save/pre-edit snapshot. Mirrors `haily_db::queries::skill_versions::SkillVersion`. */
export interface SkillVersion {
  id: string;
  skill_name: string;
  kind: SkillEditKind;
  content_md: string;
  sha256: string;
  note: string | null;
  created_at: string;
}

/** Fetch one skill's structured-editor view — the editor's "open" action. */
export async function getSkillDetail(name: string, kind: SkillEditKind): Promise<SkillDetail> {
  return invoke('get_skill_detail', { name, kind });
}

/** Save a skill's structured fields. Returns the full saved content (informational). */
export async function editSkill(name: string, kind: SkillEditKind, draft: SkillDraft): Promise<string> {
  return invoke('edit_skill', { name, kind, draft });
}

/** Version history for one skill, newest first. */
export async function listSkillVersions(name: string): Promise<SkillVersion[]> {
  return invoke('list_skill_versions', { name });
}

/** Restore a skill to a prior recorded version. */
export async function revertSkill(name: string, versionId: string): Promise<string> {
  return invoke('revert_skill', { name, versionId });
}

/** Promote a synthesized skill to an authored kit-pack file — exits the decay lifecycle. */
export async function promoteSkill(name: string): Promise<string> {
  return invoke('promote_skill', { name });
}

/** Manually archive a synthesized skill (distinct from automatic confidence-decay archival). */
export async function archiveSkillManual(name: string): Promise<void> {
  return invoke('archive_skill_manual', { name });
}

/** Name-sorted authored skills currently served from a recovery snapshot because their on-disk
 * file failed manifest-hash verification (review MED-1: tamper/interrupted-edit signal). Empty
 * on a clean load — poll or check on the Skills screen to badge any name present here. */
export async function listRecoveredSkills(): Promise<string[]> {
  return invoke('list_recovered_skills');
}

/** The named permission ladder (Unified Chat UI phase 11, D5). Unset/unknown reads as
 * `manual` — the strictest rung — everywhere this is consumed. */
export type ApprovalMode = 'manual' | 'accept_edits' | 'auto';

/** Set the permission ladder. Backend normalizes any unrecognized string to `manual` and
 * persists BEFORE flipping the live handle (fails toward the stricter mode on a crash). */
export async function setApprovalMode(mode: ApprovalMode): Promise<void> {
  return invoke('set_approval_mode', { mode });
}

/** Per-run kill/pause/resume control (Unified Chat UI phase 6, D3). Local-GUI-only — never
 * exposed on the mobile/remote bridge; `run_id` carries no session-ownership check of its own. */

/** Cancel a run immediately. Returns `false` (not an error) if the run was already
 * terminal/unknown — nothing to do. */
export async function killRun(runId: string): Promise<boolean> {
  return invoke('kill_run', { runId });
}

/** Best-effort, stage-boundary pause. Returns `false` (not an error) if the run has no live
 * entry (already terminal/paused, or unknown). */
export async function pauseRun(runId: string): Promise<boolean> {
  return invoke('pause_run', { runId });
}

/** Resume an `interrupted` run, or a `paused` run whose reason is retries-exhausted or
 * explicit-stop — never an approval-wait pause (that resolves through its approval card).
 * Returns `false` (not an error) for any ineligible/unknown run. Throws if the run's workspace
 * was already reclaimed or its change already applied — surface that message verbatim. */
export async function resumeRun(runId: string): Promise<boolean> {
  return invoke('resume_run', { runId });
}

/** One run row for the Runs screen (Unified Chat UI phase 7, D6). Mirrors
 * `haily_app::runs_view::RunSummary`. Deliberately carries RAW status/reason fields rather
 * than a pre-rendered sentence — VN narration is derived client-side (`run-summary.ts`),
 * overlaid with the live reducer's richer per-event narration for an in-flight run.
 * `resumable` is server-computed from the SAME guard `resumeRun` itself enforces — never
 * re-derive it from `status` alone (mirrors `WorkspaceView.resumable`'s own contract). */
export interface RunSummary {
  id: string;
  session_id: string;
  work_item_id: string | null;
  status: string;
  pause_reason_class: string | null;
  task: string | null;
  stage_index: number;
  attempt: number;
  attempts_remaining: number;
  tier_used: string | null;
  backend_used: string | null;
  /** Raw per-attempt token JSON array (`[{stage,attempt,tier,backend,prompt_tokens,
   * completion_tokens}]`) — parsed by `RunTelemetry.svelte`; `null` until any attempt recorded
   * one. */
  per_attempt_tokens: string | null;
  created_at: string;
  updated_at: string;
  resumable: boolean;
}

/** Runs for the Runs screen: every active run plus a bounded window of recent terminal
 * history (server-side cap — see `haily_app::runs_view::list_runs`'s doc). Read-only,
 * local-GUI-only. */
export async function listRuns(): Promise<RunSummary[]> {
  return invoke('list_runs');
}

/** Rehydrate one run's persisted timeline for the drill-in (Unified Chat UI phase 7, D6) — the
 * backend ALSO reconciles a missing terminal event against `pipeline_runs.status`, so a
 * crashed/restarted run's timeline never renders as permanently "running" here. */
export async function listRunEvents(runId: string): Promise<RunEvent[]> {
  return invoke('list_run_events', { runId });
}
