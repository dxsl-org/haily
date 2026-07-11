<script lang="ts">
  // Primary cockpit dashboard (P11b) — composes the run timeline, workspaces, skills,
  // approvals queue, and channels/kill-switch panel. Mounted as an alternate main view
  // from `+page.svelte` (toggled next to the existing chat), not a separate route — this
  // app has no client-side router, so the toggle mirrors the existing Settings-drawer
  // pattern (conditional render off local state).
  import RunTimeline from './RunTimeline.svelte';
  import WorkspacePanel from './WorkspacePanel.svelte';
  import SkillsBrowser from './SkillsBrowser.svelte';
  import ApprovalsQueue from './ApprovalsQueue.svelte';
  import ChannelsPanel from './ChannelsPanel.svelte';
  import type { QueuedApproval } from '$lib/tauri';

  // Lifted here (not fetched twice) so `WorkspacePanel`'s diff-accept correlation sees
  // the same snapshot `ApprovalsQueue` is displaying.
  let approvals = $state<QueuedApproval[]>([]);

  // Capped rolling corpus of `StageOutput` text across every run this cockpit instance
  // has observed — feeds `SkillsBrowser`'s best-effort "activated this run" heuristic.
  // Capped so a long-running build can't grow this without bound.
  const MAX_CORPUS = 20000;
  let outputCorpus = $state('');

  function appendOutput(chunk: string) {
    outputCorpus = (outputCorpus + '\n' + chunk).slice(-MAX_CORPUS);
  }
</script>

<div class="cockpit">
  <section class="col-main">
    <RunTimeline onOutputText={appendOutput} />
  </section>
  <section class="col-side">
    <ApprovalsQueue onUpdate={(a) => (approvals = a)} />
    <WorkspacePanel {approvals} />
    <SkillsBrowser runOutputText={outputCorpus} />
    <ChannelsPanel />
  </section>
</div>

<style>
  .cockpit {
    display: flex;
    flex: 1;
    min-height: 0;
    overflow: hidden;
  }

  .col-main {
    flex: 2;
    min-width: 0;
    overflow-y: auto;
    padding: 16px;
    border-right: 1px solid #1e1e2e;
  }

  .col-side {
    flex: 1;
    min-width: 260px;
    max-width: 380px;
    overflow-y: auto;
    padding: 16px;
    display: flex;
    flex-direction: column;
    gap: 22px;
  }

  /* No horizontal page scroll on narrow viewports — columns stack, each still scrolls
     independently within itself. */
  @media (max-width: 768px) {
    .cockpit { flex-direction: column; overflow-y: auto; }
    .col-main, .col-side { max-width: none; overflow: visible; border-right: none; }
  }
</style>
