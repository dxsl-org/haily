<script lang="ts">
  let { prefs, save }: {
    prefs: Record<string, string>;
    save: (key: string, value: string) => Promise<void>;
  } = $props();

  const souls = [
    { id: 'haily',   label: 'Haily',    emoji: '💜', desc: 'Nghiêm túc, chuyên nghiệp, thân mật vừa phải' },
    { id: 'tete',    label: 'Tê tê',    emoji: '🤖', desc: 'Máy móc, tối giản, data-first, không filler' },
    { id: 'hoami',   label: 'Họa mi',   emoji: '🌸', desc: 'Ngọt ngào, dễ thương, ấm áp, hay dùng nhé/nha' },
    { id: 'lungmat', label: 'Lửng mật', emoji: '🍯', desc: 'Phá cách, hài hước, năng lượng cao, slang ok' },
  ];

  const currentSoul = () => prefs['agent.soul'] ?? 'haily';
</script>

<div class="section">
  <div class="field-label">Soul — phong cách trả lời</div>
  <div class="souls">
    {#each souls as s}
      <button
        class="soul-card"
        class:active={currentSoul() === s.id}
        onclick={() => save('agent.soul', s.id)}
      >
        <span class="soul-emoji">{s.emoji}</span>
        <span class="soul-name">{s.label}</span>
        <span class="soul-desc">{s.desc}</span>
      </button>
    {/each}
  </div>

  <label>Tên trợ lý
    <input type="text"
      value={prefs['agent.name'] ?? 'Haily'}
      onblur={e => save('agent.name', e.currentTarget.value)}
      placeholder="Haily" />
  </label>

  <label>Trợ lý tự xưng là
    <input type="text"
      value={prefs['agent.pronoun'] ?? 'tôi'}
      onblur={e => save('agent.pronoun', e.currentTarget.value)}
      placeholder="tôi" />
  </label>

  <label>Gọi người dùng là
    <input type="text"
      value={prefs['user.address'] ?? 'bạn'}
      onblur={e => save('user.address', e.currentTarget.value)}
      placeholder="bạn" />
  </label>
</div>

<style>
  .section { display: flex; flex-direction: column; gap: 20px; }

  .field-label { font-size: 12px; color: #8884aa; }

  .souls {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px;
  }

  .soul-card {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 4px;
    padding: 12px;
    border: 1px solid #2e2e4a;
    border-radius: 10px;
    background: #16162a;
    cursor: pointer;
    text-align: left;
    transition: border-color 0.15s, background 0.15s;
  }
  .soul-card:hover { border-color: #4a3a7a; }
  .soul-card.active {
    border-color: #7c3aed;
    background: #1a1040;
  }

  .soul-emoji { font-size: 18px; line-height: 1; }
  .soul-name  { font-size: 13px; color: #e0dff5; font-weight: 600; }
  .soul-desc  { font-size: 11px; color: #6b6b8a; line-height: 1.4; }

  label {
    display: flex;
    flex-direction: column;
    gap: 6px;
    font-size: 12px;
    color: #8884aa;
  }
  input {
    background: #16162a;
    border: 1px solid #2e2e4a;
    border-radius: 8px;
    color: #e0dff5;
    font: inherit;
    font-size: 13px;
    padding: 8px 10px;
    outline: none;
    transition: border-color 0.15s;
  }
  input:focus { border-color: #7c3aed; }
  input::placeholder { color: #4a4a6a; }
</style>
