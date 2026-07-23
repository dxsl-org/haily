<script lang="ts">
  // Placeholder destination — Track C (P10) fills this with resumable coding workspaces
  // (branch, dirty status, diff) reusing `WorkspacePanel`/`DiffViewer`. Empty state only.
</script>

<div class="screen">
  <div class="empty">
    <span class="glyph">🗂</span>
    <p>Chưa có không gian làm việc nào.</p>
    <p class="hint">Không gian làm việc từ các tác vụ pipeline sẽ xuất hiện ở đây.</p>
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
