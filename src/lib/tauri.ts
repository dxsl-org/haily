// Tauri IPC helpers — thin wrappers keeping Svelte components testable.
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';

export interface Chunk {
  type: 'Text' | 'ToolResult' | 'ToolApprovalRequest' | 'Complete';
  data?: string;
}

export interface ChunkPayload {
  session_id: string;
  chunk: Chunk;
}

/** Send a message and return the session UUID. */
export async function sendMessage(message: string): Promise<string> {
  return invoke('send_message', { message });
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
