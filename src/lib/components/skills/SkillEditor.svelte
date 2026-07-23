<script lang="ts">
  // Structured 4-field editor (D4). NEVER a raw markdown textarea — the 4 fields are the only
  // editable surface; `render_markdown`/`parse_markdown` (server-side) own the markdown mapping,
  // including the section-injection defense (see Security Considerations in the phase file).
  import { editSkill, getSkillDetail, listRecoveredSkills, type SkillDraft, type SkillEditKind } from '$lib/tauri';
  import { mapSkillSaveError, skillFieldLabel } from '$lib/skill-draft-format';
  import SkillVersionPanel from './SkillVersionPanel.svelte';
  import DraftWithHaily from './DraftWithHaily.svelte';
  import SkillEditorHeader from './SkillEditorHeader.svelte';

  let { name, kind, onBack }: { name: string; kind: SkillEditKind; onBack: () => void } = $props();

  const EMPTY_DRAFT: SkillDraft = { procedure: '', success_conditions: '', forbidden_actions: '', required_from_user: '' };

  let draft = $state<SkillDraft>({ ...EMPTY_DRAFT });
  let loading = $state(true);
  let loadError = $state('');
  let saving = $state(false);
  let saveError = $state('');
  let saveErrorField = $state<keyof SkillDraft | null>(null);
  let saveOk = $state(false);
  let recoveredNames = $state<string[]>([]);
  let showDraftHaily = $state(false);
  let versionsKey = $state(0);

  const isRecovered = $derived(kind === 'authored' && recoveredNames.includes(name));
  const canSave = $derived(draft.procedure.trim().length > 0 && draft.success_conditions.trim().length > 0);

  $effect(() => {
    load();
  });

  async function load() {
    loading = true;
    loadError = '';
    try {
      const detail = await getSkillDetail(name, kind);
      draft = { ...detail.draft };
      if (kind === 'authored') {
        recoveredNames = await listRecoveredSkills();
      }
    } catch (e) {
      loadError = String(e);
    } finally {
      loading = false;
    }
  }

  async function save() {
    if (saving || !canSave) return;
    saving = true;
    saveError = '';
    saveErrorField = null;
    saveOk = false;
    try {
      await editSkill(name, kind, draft);
      saveOk = true;
      versionsKey += 1;
      await load();
    } catch (e) {
      const mapped = mapSkillSaveError(String(e));
      saveError = mapped.message;
      saveErrorField = mapped.field;
    } finally {
      saving = false;
    }
  }

  function handleDraftFill(filled: SkillDraft) {
    const hasContent =
      draft.procedure.trim() || draft.success_conditions.trim() || draft.forbidden_actions.trim() || draft.required_from_user.trim();
    if (hasContent && !confirm('Nội dung do Haily soạn sẽ THAY THẾ nội dung hiện tại trong 4 ô bên dưới. Tiếp tục?')) {
      return;
    }
    draft = { ...filled };
    showDraftHaily = false;
    saveOk = false;
  }

  function handleReverted() {
    versionsKey += 1;
    load();
  }
</script>

<div class="editor">
  <button class="back-btn" onclick={onBack}>← Danh sách kỹ năng</button>

  {#if loading}
    <div class="empty">Đang tải…</div>
  {:else if loadError}
    <div class="status-error">⚠️ {loadError}</div>
  {:else}
    <SkillEditorHeader {name} {kind} recovered={isRecovered} />

    {#each (['procedure', 'success_conditions', 'forbidden_actions', 'required_from_user'] as const) as field (field)}
      <div class="field">
        <label for={`skill-field-${field}`}>{skillFieldLabel(field)}</label>
        <textarea
          id={`skill-field-${field}`}
          bind:value={draft[field]}
          rows={field === 'procedure' ? 5 : 3}
          class:invalid={saveErrorField === field}
        ></textarea>
      </div>
    {/each}

    {#if saveError}<div class="status-error">⚠️ {saveError}</div>{/if}
    {#if saveOk}<div class="status-ok">Đã lưu.</div>{/if}

    <div class="actions">
      <button class="save-btn" onclick={save} disabled={!canSave || saving}>{saving ? 'Đang lưu…' : 'Lưu'}</button>
      <button class="toggle-btn" onclick={() => (showDraftHaily = !showDraftHaily)}>
        {showDraftHaily ? 'Đóng "Nhờ Haily soạn"' : 'Nhờ Haily soạn'}
      </button>
    </div>

    {#if showDraftHaily}
      <DraftWithHaily onFill={handleDraftFill} />
    {/if}

    {#key versionsKey}
      <SkillVersionPanel {name} {kind} onReverted={handleReverted} />
    {/key}
  {/if}
</div>

<style>
  .editor { display: flex; flex-direction: column; gap: 12px; }

  .back-btn {
    align-self: flex-start;
    padding: 4px 0;
    border: none;
    background: none;
    color: #8884aa;
    font-size: 12px;
    cursor: pointer;
  }
  .back-btn:hover { color: #c084fc; }

  .field { display: flex; flex-direction: column; gap: 4px; }
  .field label { font-size: 12px; color: #e0dff5; font-weight: 600; }
  .field textarea {
    resize: vertical;
    padding: 8px;
    border-radius: 6px;
    border: 1px solid #2e2e4a;
    background: #16162a;
    color: #e0dff5;
    font-size: 12px;
    font-family: inherit;
  }
  .field textarea.invalid { border-color: #7f1d1d; }

  .actions { display: flex; gap: 8px; }
  .save-btn, .toggle-btn {
    padding: 6px 14px;
    min-height: 32px;
    border-radius: 7px;
    border: 1px solid #2e2e4a;
    background: #16162a;
    color: #c084fc;
    font-size: 12px;
    cursor: pointer;
  }
  .save-btn { border-color: #4a3a7a; background: #1e1e35; }
  .save-btn:disabled { opacity: 0.5; cursor: default; }

  .empty { font-size: 12px; color: #6b6b8a; }
  .status-error {
    font-size: 11px;
    padding: 6px 10px;
    border-radius: 6px;
    background: #2a0f0f;
    color: #f87171;
    border: 1px solid #7f1d1d;
    word-break: break-word;
  }
  .status-ok {
    font-size: 11px;
    padding: 6px 10px;
    border-radius: 6px;
    background: #0f2a17;
    color: #4ade80;
    border: 1px solid #166534;
  }
</style>
