<script lang="ts">
  // Placeholder destination — Track B (P07) fills this with the run list, live status,
  // and control actions (kill/pause/resume). Empty state only, no data fetch here.
</script>

<div class="screen">
  <div class="empty">
    <span class="glyph">▶</span>
    <p>Chưa có tác vụ nào.</p>
    <p class="hint">Các tác vụ chạy pipeline sẽ xuất hiện ở đây.</p>
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
