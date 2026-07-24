<script lang="ts">
  // Per-attempt token/tier telemetry for the Runs drill-in (Unified Chat UI phase 7).
  // `per_attempt_tokens` is a raw JSON array the runner appends to (FMA-m5); `backend`/
  // `prompt_tokens`/`completion_tokens` are currently always `null` (a wired-but-inert scaffold
  // — see `crates/haily-core/src/pipeline/runner.rs`'s `attempt_token_record`), rendered as
  // "—" rather than hidden so the columns are already in place once a future phase populates
  // them.
  import type { RunSummary } from '$lib/tauri';

  let { run }: { run: RunSummary } = $props();

  interface AttemptRecord {
    stage: string;
    attempt: number;
    tier: string | null;
    backend: string | null;
    prompt_tokens: number | null;
    completion_tokens: number | null;
  }

  function parseAttempts(json: string | null): AttemptRecord[] {
    if (!json) return [];
    try {
      const parsed: unknown = JSON.parse(json);
      return Array.isArray(parsed) ? (parsed as AttemptRecord[]) : [];
    } catch {
      return [];
    }
  }

  function tokenLabel(rec: AttemptRecord): string {
    return rec.prompt_tokens != null && rec.completion_tokens != null
      ? `${rec.prompt_tokens}/${rec.completion_tokens}`
      : '—';
  }

  const attempts = $derived(parseAttempts(run.per_attempt_tokens));
</script>

<div class="telemetry">
  <h3>Thông số</h3>
  <div class="summary">
    <span>Lần thử hiện tại: {run.attempt} · còn lại: {run.attempts_remaining}</span>
    {#if run.tier_used}<span>Mô hình gần nhất: {run.tier_used}</span>{/if}
    {#if run.backend_used}<span>Backend: {run.backend_used}</span>{/if}
  </div>
  {#if attempts.length > 0}
    <table>
      <thead>
        <tr>
          <th>Giai đoạn</th>
          <th>Lần thử</th>
          <th>Mô hình</th>
          <th>Token (vào/ra)</th>
        </tr>
      </thead>
      <tbody>
        {#each attempts as rec, i (i)}
          <tr>
            <td>{rec.stage}</td>
            <td>{rec.attempt}</td>
            <td>{rec.tier ?? '—'}</td>
            <td>{tokenLabel(rec)}</td>
          </tr>
        {/each}
      </tbody>
    </table>
  {:else}
    <p class="empty">Chưa có dữ liệu token cho lượt chạy này.</p>
  {/if}
</div>

<style>
  .telemetry { display: flex; flex-direction: column; gap: 8px; font-size: 12px; }

  h3 { font-size: 12px; color: #e0dff5; margin: 0; }

  .summary { display: flex; gap: 12px; flex-wrap: wrap; color: #a09ac0; font-size: 11px; }

  table { width: 100%; border-collapse: collapse; font-size: 11px; }
  th, td {
    text-align: left;
    padding: 4px 8px;
    border-bottom: 1px solid #1e1e2e;
    color: #a09ac0;
  }
  th { color: #6b6b8a; font-weight: 600; }

  .empty { font-size: 11px; color: #6b6b8a; }
</style>
