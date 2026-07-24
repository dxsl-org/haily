<script lang="ts">
  // Workspaces destination (Unified Chat UI phase 10, D6) — plain-language coding-workspace
  // list + resume. Polls the shared approvals queue itself (no push channel for it exists) so
  // `WorkspacesList`/`WorkspaceRow` can correlate a pending `worktree_apply` approval with its
  // owning workspace without a second component reaching into the same state.
  import { onDestroy, onMount } from 'svelte';
  import { listApprovals, type QueuedApproval } from '$lib/tauri';
  import WorkspacesList from './workspaces/WorkspacesList.svelte';

  let approvals = $state<QueuedApproval[]>([]);
  let pollHandle: ReturnType<typeof setInterval> | undefined;

  async function loadApprovals() {
    try {
      approvals = await listApprovals();
    } catch {
      // Best-effort correlation source only — a failed poll just means the generic pending-
      // approval notice stays hidden on every row until the next tick; the row's own Discard/
      // Continue actions are unaffected.
    }
  }

  onMount(() => {
    loadApprovals();
    pollHandle = setInterval(loadApprovals, 5000);
  });

  onDestroy(() => {
    if (pollHandle) clearInterval(pollHandle);
  });
</script>

<div class="screen">
  <WorkspacesList {approvals} />
</div>

<style>
  .screen {
    flex: 1;
    min-height: 0;
    overflow-y: auto;
    padding: 16px;
  }
</style>
