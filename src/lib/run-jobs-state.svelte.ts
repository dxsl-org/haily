// Page-lifetime pipeline-run job state (P04 review-fix MED-1). Originally lived inside
// the route-gated `ChatStream`, which meant leaving the chat route mid-run tore down both
// the `onRunEvents` subscription AND the `jobs` map — on return, the next event re-created
// the job from scratch (`startedAt = Date.now()` resets elapsed to ~00:00, retry/escalation
// counts undercount because only post-remount events were folded). `+page.svelte` now owns
// one instance of this state for the whole app lifetime, same as `sessionIndex`/
// `seenSessionIds`; `ChatStream` only reads from it.
//
// `getSessionIndex`/`getMessagesLength` are closures over the caller's own state (mirrors
// `chat-palette-state.svelte.ts`'s `getInput` pattern) so the anchor lookup always sees the
// live map/array without a prop-drilled copy.
import { applyRunEvent, orderedJobs, type Job } from './run-events';
import type { RunEvent } from './tauri';

export function createRunJobsState(getSessionIndex: () => Map<string, number>, getMessagesLength: () => number) {
  let jobs = $state<Map<string, Job>>(new Map());
  let showAllActive = $state(false);
  // Per-run anchor (message index to render the card after), captured ONCE at first
  // sight and never overwritten — `sessionIndex` entries are deleted once that turn's
  // `Complete` chunk lands, often well before a long pipeline run finishes, so reading it
  // live at render time would go stale. Plain (non-reactive) Map: it's write-once/read-
  // -many, keyed off `jobs` for reactivity instead.
  const jobAnchors = new Map<string, number>();

  /** Folds one `haily-run-events` payload into state. Called from `+page.svelte`'s own
   * `onRunEvents` subscription (page lifetime — never torn down by a route switch). */
  function ingest(sessionId: string, event: RunEvent) {
    const runId = event.data.run_id;
    if (!jobAnchors.has(runId)) {
      jobAnchors.set(runId, getSessionIndex().get(sessionId) ?? Math.max(getMessagesLength() - 1, 0));
    }
    jobs = applyRunEvent(jobs, sessionId, event);
  }

  const jobList = $derived(orderedJobs(jobs));
  const activeJobs = $derived(jobList.filter((j) => j.status === 'running' || j.status === 'paused'));

  // Groups jobs by their anchor message index so `ChatStream`'s render loop can splice a
  // run's card in right after the bubble that launched it. Only anchors that fall within
  // the CURRENT message list are included here — an anchor computed while `messages` was
  // empty (edge case; the seeded welcome bubble makes this unreachable today, but nothing
  // guarantees that forever) would otherwise silently vanish, never matching any `{#each
  // messages}` iteration. `unanchoredJobs` below is `ChatStream`'s fallback for exactly
  // that case (review fix LOW-6).
  const jobsByAnchor = $derived.by(() => {
    const map = new Map<number, Job[]>();
    const messagesLength = getMessagesLength();
    for (const job of jobList) {
      const anchor = jobAnchors.get(job.runId) ?? Math.max(messagesLength - 1, 0);
      if (anchor < 0 || anchor >= messagesLength) continue;
      const arr = map.get(anchor) ?? [];
      arr.push(job);
      map.set(anchor, arr);
    }
    return map;
  });

  const unanchoredJobs = $derived.by(() => {
    const messagesLength = getMessagesLength();
    return jobList.filter((job) => {
      const anchor = jobAnchors.get(job.runId) ?? Math.max(messagesLength - 1, 0);
      return anchor < 0 || anchor >= messagesLength;
    });
  });

  // Chat-overload mitigation (phase-04 risk assessment): a finished job always renders
  // its own card (it's a result, not noise), but once MORE THAN ONE run is active at
  // once, all active cards collapse behind a single "N running" chip until the user
  // opts to expand — mirrors the Claude-Desktop background-tasks pattern.
  function showCard(job: Job): boolean {
    if (job.status !== 'running' && job.status !== 'paused') return true;
    return activeJobs.length <= 1 || showAllActive;
  }

  return {
    ingest,
    showCard,
    get jobsByAnchor() {
      return jobsByAnchor;
    },
    get unanchoredJobs() {
      return unanchoredJobs;
    },
    get activeJobs() {
      return activeJobs;
    },
    get showAllActive() {
      return showAllActive;
    },
    expandAllActive() {
      showAllActive = true;
    },
  };
}

export type RunJobsState = ReturnType<typeof createRunJobsState>;
