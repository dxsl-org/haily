// Pure RunEvent → Vietnamese verb-phrase map for the collapsed progress card's "last
// action" line (P04). Reused by P07 for the Runs-list "last output" row and OS
// notification bodies (plan.md D6) — keep this the single place that phrase is authored.
// Single-language output by owner decision (plan.md "UI/narration language"): no
// `ui.language` pref, no i18n table, strings authored VN inline. Technical terms (stage
// names, "diff") stay in English inside the VN sentence.
//
// Every value interpolated below (`stage`, `gate`, `tier`, `reason`, `from`/`to`) is a
// fixed, small vocabulary the RUNNER itself assigns (e.g. "build"/"verify"/"review",
// "fast"/"thinking"/"ultra") — never raw LLM/tool output — so plain-text interpolation
// here is safe. This module must NEVER narrate `StageOutput.chunk`, `GateResult.decisive`,
// or `DiffAvailable.file` verbatim (those stay in `run-events.ts`'s `describeEvent`,
// rendered as inert text, never folded into a sentence here).
import type { RunEvent } from './tauri';

const FALLBACK_VERB = 'Đang xử lý…';

/**
 * Maps one `RunEvent` to a short Vietnamese verb phrase. Every variant `RunEvent` declares
 * today returns a distinct, meaningful phrase (see the unit test asserting full coverage);
 * an unrecognized future variant — a frontend build older than the backend that emitted it
 * — degrades to `FALLBACK_VERB` rather than throwing or rendering blank, so a schema
 * addition on one side can never break the card on the other.
 */
export function narrate(event: RunEvent): string {
  switch (event.type) {
    case 'RunStarted':
      return 'Đang khởi chạy tác vụ';
    case 'StageStarted':
      return event.data.tier
        ? `Đang chạy giai đoạn ${event.data.stage} (${event.data.tier})`
        : `Đang chạy giai đoạn ${event.data.stage}`;
    case 'StageOutput':
      return 'Đang ghi log từ giai đoạn hiện tại';
    case 'GateResult':
      return event.data.pass
        ? `Đã qua kiểm tra ${event.data.gate}`
        : `Không qua kiểm tra ${event.data.gate}`;
    case 'Retry':
      return `Đang thử lại — lần ${event.data.attempt}`;
    case 'Escalation':
      return `Đã nâng mô hình từ ${event.data.from} lên ${event.data.to}`;
    case 'DiffAvailable':
      return 'Đã có bản diff để xem lại';
    case 'ApprovalNeeded':
      return 'Đang chờ bạn phê duyệt';
    case 'PlanReady':
      return 'Kế hoạch đã sẵn sàng';
    case 'RunPaused':
      return `Đã tạm dừng (${event.data.reason})`;
    case 'RunComplete':
      // "interrupted" is checked BEFORE the fail/error heuristic — same fix as
      // `run-events.ts`'s `applyRunEvent` (review MED): a synthesized/persisted
      // `RunComplete{outcome:"interrupted"}` must never narrate as a success, since this
      // phrase feeds the SAME card whose status badge now correctly shows "Gián đoạn".
      if (/^interrupted$/i.test(event.data.outcome)) {
        return 'Đã gián đoạn — có thể tiếp tục';
      }
      return /fail|error/i.test(event.data.outcome) ? 'Đã hoàn tất — thất bại' : 'Đã hoàn tất — thành công';
    default:
      return FALLBACK_VERB;
  }
}
