<script lang="ts">
  // Placeholder destination — Track C (P09) fills this with the structured skill editor
  // (authored + synthesized skills, draft-with-Haily) reusing `SkillsBrowser`. Empty state only.
</script>

<div class="screen">
  <div class="empty">
    <span class="glyph">✨</span>
    <p>Chưa có kỹ năng nào.</p>
    <p class="hint">Kỹ năng đã học và kỹ năng có sẵn sẽ xuất hiện ở đây.</p>
  </div>
</div>

<style>
  .screen {
    flex: 1;
    min-height: 0;
    display: flex;
    align-items: center;
    justify-content: center;
  }

  .empty { text-align: center; color: #6b6b8a; }
  .empty .glyph { display: block; font-size: 32px; margin-bottom: 10px; opacity: 0.6; }
  .empty p { margin: 4px 0; font-size: 13px; }
  .empty .hint { font-size: 12px; color: #4a4a6a; }
</style>
