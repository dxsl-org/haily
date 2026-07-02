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
  data: { tool: string; args: string; approval_id: string };
}

export interface ToolResultChunk {
  type: 'ToolResult';
  data: { name: string; ok: boolean };
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
