<script lang="ts">
  // One skill row for `SkillsBrowser.svelte` (split out mirroring ConnectorConfig/
  // ConnectorRow). Enable/pin persist immediately via the existing `set_skill_enabled`/
  // `pin_skill` commands; ENFORCEMENT (excluding disabled skills from injection,
  // prioritizing pinned) is a backend concern the P11a deviation log defers past this
  // admin surface — this row only reflects and toggles the persisted state.
  import { setSkillEnabled, pinSkill, type SkillView } from '$lib/tauri';

  let { skill, activated, onChanged }: { skill: SkillView; activated: boolean; onChanged: () => void } = $props();

  let togglingEnabled = $state(false);
  let togglingPin = $state(false);
  let error = $state('');

  async function toggleEnabled() {
    if (togglingEnabled) return;
    togglingEnabled = true;
    error = '';
    try {
      await setSkillEnabled(skill.name, !skill.enabled);
      onChanged();
    } catch (e) {
      error = String(e);
    } finally {
      togglingEnabled = false;
    }
  }

  async function togglePin() {
    if (togglingPin) return;
    togglingPin = true;
    error = '';
    try {
      await pinSkill(skill.name, !skill.pinned);
      onChanged();
    } catch (e) {
      error = String(e);
    } finally {
      togglingPin = false;
    }
  }
</script>

<div class="row">
  <div class="head">
    <span class="name">{skill.name}</span>
    <span class="badge source-{skill.source}">{skill.source}</span>
    {#if activated}<span class="badge activated">used this run</span>{/if}
    {#if !skill.enabled}<span class="badge off">disabled</span>{/if}
  </div>
  <p class="desc">{skill.description}</p>
  {#if skill.source === 'synthesized'}
    <div class="meta">
      {#if skill.confidence !== null}<span>confidence {(skill.confidence * 100).toFixed(0)}%</span>{/if}
      {#if skill.use_count !== null}<span>used {skill.use_count}×</span>{/if}
      {#if skill.last_used_at}<span>last {skill.last_used_at}</span>{/if}
    </div>
  {/if}
  <div class="actions">
    <button class="pin-btn" class:pinned={skill.pinned} onclick={togglePin} disabled={togglingPin}>
      {skill.pinned ? '★ Pinned' : '☆ Pin'}
    </button>
    <button
      class="switch"
      class:on={skill.enabled}
      role="switch"
      aria-checked={skill.enabled}
      aria-label={skill.enabled ? 'Disable skill' : 'Enable skill'}
      disabled={togglingEnabled}
      onclick={toggleEnabled}
    >
      <span class="knob"></span>
    </button>
  </div>
  {#if error}<div class="status-error">⚠️ {error}</div>{/if}
</div>

<style>
  .row {
    display: flex;
    flex-direction: column;
    gap: 6px;
    padding: 10px;
    background: #0f0f18;
    border: 1px solid #23233a;
    border-radius: 8px;
  }

  .head { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
  .name { font-size: 12px; font-weight: 600; color: #e0dff5; }

  .badge {
    font-size: 9px;
    padding: 2px 7px;
    border-radius: 999px;
    background: #1e1e35;
    border: 1px solid #2e2e4a;
    color: #a09ac0;
  }
  .badge.source-authored { color: #60a5fa; }
  .badge.source-synthesized { color: #c084fc; }
  .badge.activated { color: #4ade80; border-color: #166534; }
  .badge.off { color: #6b6b8a; }

  .desc { font-size: 11px; color: #8884aa; line-height: 1.5; }

  .meta { display: flex; gap: 10px; font-size: 10px; color: #6b6b8a; }

  .actions { display: flex; align-items: center; gap: 8px; }

  .pin-btn {
    padding: 4px 10px;
    min-height: 28px;
    border: 1px solid #2e2e4a;
    border-radius: 999px;
    background: #16162a;
    color: #8884aa;
    font-size: 11px;
    cursor: pointer;
  }
  .pin-btn.pinned { color: #fbbf24; border-color: #7f5a1d; }
  .pin-btn:disabled { opacity: 0.5; cursor: default; }

  .switch {
    flex-shrink: 0;
    width: 42px;
    height: 24px;
    border-radius: 999px;
    border: 1px solid #2e2e4a;
    background: #0f0f1e;
    cursor: pointer;
    position: relative;
  }
  .switch:disabled { opacity: 0.5; cursor: default; }
  .switch .knob {
    position: absolute;
    top: 2px;
    left: 2px;
    width: 18px;
    height: 18px;
    border-radius: 50%;
    background: #6b6b8a;
    transition: transform 0.15s, background 0.15s;
  }
  .switch.on { background: #4c1d95; border-color: #7c3aed; }
  .switch.on .knob { transform: translateX(18px); background: #e0dff5; }

  .status-error {
    font-size: 11px;
    padding: 6px 10px;
    border-radius: 6px;
    background: #2a0f0f;
    color: #f87171;
    border: 1px solid #7f1d1d;
    word-break: break-word;
  }
</style>
