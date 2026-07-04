//! Golden-task eval harness (Harness Completion phase 5, Implementation Step 8).
//!
//! Deterministic, offline, replay-only: a fixture LLM (see `fixtures/mod.rs`)
//! drives the REAL `Orchestrator::process` → `run_turn` → `tool_call::dispatch`
//! path against a REAL SQLite DB, with NO network call and NO LLM-as-judge —
//! researcher-03 §5 explicitly disqualifies a local model judging its own output
//! (self-preference bias), so every check here is a structural/DB assertion:
//! expected tool dispatched, expected DB row written, expected `TaskOutcome`
//! recorded, and no tool-protocol tag leaking into user-visible text.
//!
//! This is a normal `cargo test` target — it runs in CI exactly like any other
//! test, entirely offline (the only "network" is a loopback TCP mock server bound
//! to `127.0.0.1:0`).
#[path = "fixtures/mod.rs"]
mod fixtures;

use fixtures::{assert_no_tag_leak, run_golden_task, tool_call_content, GoldenTask};
use haily_db::queries::{facts, notes, reminders, skills as db_skills};

async fn latest_outcome(db: &haily_db::DbHandle, session_id: uuid::Uuid) -> String {
    db_skills::recent_traces(db, 10)
        .await
        .expect("recent_traces")
        .into_iter()
        .find(|t| t.session_id == session_id.to_string())
        .expect("a trace for this session must exist")
        .outcome
}

/// Runs `task`, then applies the standard checks every golden task shares: expected
/// tool dispatched (if any), no tag leak, and the expected `TaskOutcome`. Returns the
/// `RunOutcome` so callers can layer task-specific DB assertions on top.
async fn run_and_check_common(task: &GoldenTask) -> fixtures::RunOutcome {
    let outcome = run_golden_task(task).await;

    assert_no_tag_leak(&outcome.visible_text);

    if let Some(expected_tool) = task.expected_tool {
        assert!(
            outcome.tool_results.iter().any(|(name, _)| name == expected_tool),
            "[{}] expected tool '{expected_tool}' to have been dispatched, got: {:?}",
            task.id,
            outcome.tool_results
        );
    } else {
        assert!(
            outcome.tool_results.is_empty(),
            "[{}] expected no tool dispatch, got: {:?}",
            task.id,
            outcome.tool_results
        );
    }

    let actual_outcome = latest_outcome(&outcome.db, outcome.session_id).await;
    assert_eq!(
        actual_outcome, task.expected_outcome,
        "[{}] expected TaskOutcome '{}', got '{actual_outcome}'",
        task.id, task.expected_outcome
    );

    outcome
}

// ---------------------------------------------------------------------------
// Plain Q&A — no tool call (VN + EN)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn qa_01_en_capital_of_vietnam() {
    let task = GoldenTask {
        id: "qa_01_en_capital_of_vietnam",
        message: "what is the capital of vietnam",
        scripted_responses: vec!["Hanoi is the capital of Vietnam.".to_string()],
        expected_tool: None,
        expected_outcome: "success",
    };
    run_and_check_common(&task).await;
}

#[tokio::test]
async fn qa_02_vi_thoi_tiet_hom_nay() {
    let task = GoldenTask {
        id: "qa_02_vi_thoi_tiet_hom_nay",
        message: "hôm nay thời tiết thế nào",
        scripted_responses: vec!["Hôm nay trời nắng đẹp.".to_string()],
        expected_tool: None,
        expected_outcome: "success",
    };
    run_and_check_common(&task).await;
}

#[tokio::test]
async fn qa_03_en_simple_greeting() {
    let task = GoldenTask {
        id: "qa_03_en_simple_greeting",
        message: "hi there",
        scripted_responses: vec!["Hello! How can I help you today?".to_string()],
        expected_tool: None,
        expected_outcome: "success",
    };
    run_and_check_common(&task).await;
}

#[tokio::test]
async fn qa_04_vi_cam_on() {
    let task = GoldenTask {
        id: "qa_04_vi_cam_on",
        message: "cảm ơn bạn nhiều nhé",
        scripted_responses: vec!["Không có gì, rất vui được giúp bạn!".to_string()],
        expected_tool: None,
        expected_outcome: "success",
    };
    run_and_check_common(&task).await;
}

#[tokio::test]
async fn qa_05_en_explain_concept() {
    let task = GoldenTask {
        id: "qa_05_en_explain_concept",
        message: "explain what an EMA is in three sentences",
        scripted_responses: vec![
            "An EMA (exponential moving average) weights recent data more heavily. \
             It smooths a series while staying responsive to new values. It's \
             commonly used in both trading and confidence-tracking systems."
                .to_string(),
        ],
        expected_tool: None,
        expected_outcome: "success",
    };
    run_and_check_common(&task).await;
}

// ---------------------------------------------------------------------------
// note_save — ReversibleWrite, journaled, no approval prompt
// ---------------------------------------------------------------------------

#[tokio::test]
async fn note_01_en_save_shopping_list() {
    let task = GoldenTask {
        id: "note_01_en_save_shopping_list",
        message: "save a note titled 'Shopping List' with content 'milk, eggs, bread'",
        scripted_responses: vec![
            tool_call_content(
                "note_save",
                serde_json::json!({"title": "Shopping List", "content": "milk, eggs, bread"}),
            ),
            "Đã lưu note Shopping List cho bạn.".to_string(),
        ],
        expected_tool: Some("note_save"),
        expected_outcome: "success",
    };
    let outcome = run_and_check_common(&task).await;

    let rows = notes::search_fts(&outcome.db, "milk", 10).await.expect("search_fts");
    assert!(
        rows.iter().any(|n| n.title == "Shopping List"),
        "expected a persisted note titled 'Shopping List', got: {rows:?}"
    );
}

#[tokio::test]
async fn note_02_vi_ghi_chu_hop() {
    let task = GoldenTask {
        id: "note_02_vi_ghi_chu_hop",
        message: "ghi chú lại nội dung cuộc họp sáng nay, tiêu đề 'Họp sáng', nội dung 'bàn về ngân sách quý 3'",
        scripted_responses: vec![
            tool_call_content(
                "note_save",
                serde_json::json!({"title": "Họp sáng", "content": "bàn về ngân sách quý 3"}),
            ),
            "Đã ghi chú cuộc họp cho bạn.".to_string(),
        ],
        expected_tool: Some("note_save"),
        expected_outcome: "success",
    };
    let outcome = run_and_check_common(&task).await;

    let rows = notes::search_fts(&outcome.db, "ngân", 10).await.expect("search_fts");
    assert!(
        rows.iter().any(|n| n.title == "Họp sáng"),
        "expected a persisted note titled 'Họp sáng', got: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// memory_remember — ReversibleWrite, KMS fact insert
// ---------------------------------------------------------------------------

#[tokio::test]
async fn memory_01_en_remember_preference() {
    let task = GoldenTask {
        id: "memory_01_en_remember_preference",
        message: "remember that I prefer dark roast coffee",
        scripted_responses: vec![
            tool_call_content(
                "memory_remember",
                serde_json::json!({"subject": "user", "predicate": "prefers", "object": "dark roast coffee"}),
            ),
            "Got it, I'll remember that.".to_string(),
        ],
        expected_tool: Some("memory_remember"),
        expected_outcome: "success",
    };
    let outcome = run_and_check_common(&task).await;

    let rows = facts::list_top(&outcome.db, 10).await.expect("list_top");
    assert!(
        rows.iter().any(|f| f.object.contains("dark roast")),
        "expected a persisted fact about dark roast coffee, got: {rows:?}"
    );
}

#[tokio::test]
async fn memory_02_vi_nho_thong_tin() {
    let task = GoldenTask {
        id: "memory_02_vi_nho_thong_tin",
        message: "nhớ giúp mình là mình làm việc tại công ty ABC",
        scripted_responses: vec![
            tool_call_content(
                "memory_remember",
                serde_json::json!({"subject": "user", "predicate": "làm việc tại", "object": "công ty ABC"}),
            ),
            "Mình đã ghi nhớ rồi nhé.".to_string(),
        ],
        expected_tool: Some("memory_remember"),
        expected_outcome: "success",
    };
    let outcome = run_and_check_common(&task).await;

    let rows = facts::list_top(&outcome.db, 10).await.expect("list_top");
    assert!(
        rows.iter().any(|f| f.object.contains("công ty ABC")),
        "expected a persisted fact about công ty ABC, got: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// reminder_add — ReversibleWrite, journaled
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reminder_01_en_set_reminder() {
    let task = GoldenTask {
        id: "reminder_01_en_set_reminder",
        message: "remind me to call the dentist tomorrow at 9am",
        scripted_responses: vec![
            tool_call_content(
                "reminder_add",
                serde_json::json!({"title": "Call the dentist", "fire_at": "2026-07-05T09:00:00+00:00"}),
            ),
            "I'll remind you tomorrow at 9am.".to_string(),
        ],
        expected_tool: Some("reminder_add"),
        expected_outcome: "success",
    };
    let outcome = run_and_check_common(&task).await;

    let rows = reminders::list_all(&outcome.db).await.expect("list_all");
    assert!(
        rows.iter().any(|r| r.title == "Call the dentist"),
        "expected a persisted reminder 'Call the dentist', got: {rows:?}"
    );
}

#[tokio::test]
async fn reminder_02_vi_dat_nhac_nho() {
    let task = GoldenTask {
        id: "reminder_02_vi_dat_nhac_nho",
        message: "đặt nhắc nhở uống thuốc lúc 8 giờ tối nay",
        scripted_responses: vec![
            tool_call_content(
                "reminder_add",
                serde_json::json!({"title": "Uống thuốc", "fire_at": "2026-07-04T20:00:00+00:00"}),
            ),
            "Mình đã đặt nhắc nhở uống thuốc cho bạn.".to_string(),
        ],
        expected_tool: Some("reminder_add"),
        expected_outcome: "success",
    };
    let outcome = run_and_check_common(&task).await;

    let rows = reminders::list_all(&outcome.db).await.expect("list_all");
    assert!(
        rows.iter().any(|r| r.title == "Uống thuốc"),
        "expected a persisted reminder 'Uống thuốc', got: {rows:?}"
    );
}

// ---------------------------------------------------------------------------
// feedback_react — explicit feedback signal tool
// ---------------------------------------------------------------------------

#[tokio::test]
async fn feedback_01_en_explicit_positive() {
    let task = GoldenTask {
        id: "feedback_01_en_explicit_positive",
        message: "that answer was great, thanks",
        scripted_responses: vec![
            tool_call_content("feedback_react", serde_json::json!({"reaction": "positive"})),
            "Glad it helped!".to_string(),
        ],
        expected_tool: Some("feedback_react"),
        expected_outcome: "success",
    };
    run_and_check_common(&task).await;
}

#[tokio::test]
async fn feedback_02_vi_explicit_negative() {
    let task = GoldenTask {
        id: "feedback_02_vi_explicit_negative",
        message: "câu trả lời vừa rồi chưa đúng lắm",
        scripted_responses: vec![
            tool_call_content(
                "feedback_react",
                serde_json::json!({"reaction": "negative", "about": "accuracy"}),
            ),
            "Xin lỗi, mình sẽ kiểm tra lại thông tin.".to_string(),
        ],
        expected_tool: Some("feedback_react"),
        expected_outcome: "success",
    };
    run_and_check_common(&task).await;
}

// ---------------------------------------------------------------------------
// Read-only local tools (calendar_list, work_item_list) — empty-state checks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_01_en_check_calendar() {
    let task = GoldenTask {
        id: "read_01_en_check_calendar",
        message: "what's on my calendar today",
        scripted_responses: vec![
            tool_call_content("calendar_list", serde_json::json!({})),
            "You have nothing scheduled today.".to_string(),
        ],
        expected_tool: Some("calendar_list"),
        expected_outcome: "success",
    };
    run_and_check_common(&task).await;
}

#[tokio::test]
async fn read_02_vi_kiem_tra_cong_viec() {
    let task = GoldenTask {
        id: "read_02_vi_kiem_tra_cong_viec",
        message: "kiểm tra xem có việc nào đang dang dở không",
        scripted_responses: vec![
            tool_call_content("work_item_list", serde_json::json!({})),
            "Hiện tại không có việc nào đang dang dở.".to_string(),
        ],
        expected_tool: Some("work_item_list"),
        expected_outcome: "success",
    };
    run_and_check_common(&task).await;
}

// ---------------------------------------------------------------------------
// Failure outcome — the tool call itself fails (missing required arg)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failure_01_en_note_save_missing_content() {
    let task = GoldenTask {
        id: "failure_01_en_note_save_missing_content",
        message: "save a note but somehow forget the content",
        // Deliberately omits the required "content" field — NoteSaveTool::execute
        // returns Err("content required"), which dispatch reports as ok=false.
        scripted_responses: vec![
            tool_call_content("note_save", serde_json::json!({"title": "Broken Note"})),
            "Xin lỗi, đã có lỗi xảy ra khi lưu note.".to_string(),
        ],
        expected_tool: Some("note_save"),
        expected_outcome: "failure",
    };
    let outcome = run_and_check_common(&task).await;
    assert!(
        outcome.tool_results.iter().any(|(name, ok)| name == "note_save" && !ok),
        "expected note_save to have failed: {:?}",
        outcome.tool_results
    );
}

#[tokio::test]
async fn failure_02_vi_reminder_missing_fire_at() {
    let task = GoldenTask {
        id: "failure_02_vi_reminder_missing_fire_at",
        message: "đặt nhắc nhở nhưng không nói rõ giờ",
        // Missing required "fire_at" — ReminderAddTool::execute errors.
        scripted_responses: vec![
            tool_call_content("reminder_add", serde_json::json!({"title": "Việc gì đó"})),
            "Xin lỗi, mình cần biết giờ cụ thể để đặt nhắc nhở.".to_string(),
        ],
        expected_tool: Some("reminder_add"),
        expected_outcome: "failure",
    };
    let outcome = run_and_check_common(&task).await;
    assert!(
        outcome.tool_results.iter().any(|(name, ok)| name == "reminder_add" && !ok),
        "expected reminder_add to have failed: {:?}",
        outcome.tool_results
    );
}

// ---------------------------------------------------------------------------
// Multi-turn call chains — several tool calls in one turn (Partial vs Success)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn multi_01_en_two_successful_calls_stay_success() {
    let task = GoldenTask {
        id: "multi_01_en_two_successful_calls_stay_success",
        message: "save two notes: 'Note A' with content 'first' and 'Note B' with content 'second'",
        scripted_responses: vec![
            tool_call_content("note_save", serde_json::json!({"title": "Note A", "content": "first"})),
            tool_call_content("note_save", serde_json::json!({"title": "Note B", "content": "second"})),
            "Đã lưu cả hai note cho bạn.".to_string(),
        ],
        expected_tool: Some("note_save"),
        expected_outcome: "success",
    };
    let outcome = run_and_check_common(&task).await;
    let save_count = outcome
        .tool_results
        .iter()
        .filter(|(name, ok)| name == "note_save" && *ok)
        .count();
    assert_eq!(save_count, 2, "both note_save calls must have succeeded");
}

#[tokio::test]
async fn multi_02_en_one_fail_one_succeed_is_partial() {
    let task = GoldenTask {
        id: "multi_02_en_one_fail_one_succeed_is_partial",
        message: "save a note, one attempt will be malformed and one will work",
        scripted_responses: vec![
            // 1/2 calls fail — TaskOutcome::compute: failure_ratio=0.5 is NOT >0.5,
            // and failed_calls>0 ⇒ Partial (not Failure).
            tool_call_content("note_save", serde_json::json!({"title": "Missing Content"})),
            tool_call_content("note_save", serde_json::json!({"title": "OK Note", "content": "fine"})),
            "Một note đã lưu thành công, note kia gặp lỗi.".to_string(),
        ],
        expected_tool: Some("note_save"),
        expected_outcome: "partial",
    };
    run_and_check_common(&task).await;
}

// ---------------------------------------------------------------------------
// Loop-guard interaction — a duplicate call in a row must not crash the harness
// ---------------------------------------------------------------------------

#[tokio::test]
async fn guard_01_en_duplicate_call_ends_turn_gracefully() {
    let task = GoldenTask {
        id: "guard_01_en_duplicate_call_ends_turn_gracefully",
        message: "check my calendar (this fixture will stubbornly repeat itself)",
        // Two IDENTICAL tool_call requests in a row — tool_call::LoopGuard trips on
        // the second, ending the turn with the model's own (empty-tool-tag-stripped)
        // text rather than looping forever.
        scripted_responses: vec![
            tool_call_content("calendar_list", serde_json::json!({})),
            tool_call_content("calendar_list", serde_json::json!({})),
        ],
        expected_tool: Some("calendar_list"),
        // Exactly one call actually dispatches (the loop guard trips BEFORE the
        // second dispatch), so this stays a Success (1 successful call, 0 failed).
        expected_outcome: "success",
    };
    let outcome = run_and_check_common(&task).await;
    let calendar_calls = outcome
        .tool_results
        .iter()
        .filter(|(name, _)| name == "calendar_list")
        .count();
    assert_eq!(
        calendar_calls, 1,
        "the loop guard must stop the SECOND identical call before it dispatches"
    );
}

// ---------------------------------------------------------------------------
// Additional VN + EN Q&A variety to round the suite out to ≥15 cases total
// (20 tasks defined above + below).
// ---------------------------------------------------------------------------

#[tokio::test]
async fn qa_06_en_short_factual_question() {
    let task = GoldenTask {
        id: "qa_06_en_short_factual_question",
        message: "how many days are in a leap year",
        scripted_responses: vec!["A leap year has 366 days.".to_string()],
        expected_tool: None,
        expected_outcome: "success",
    };
    run_and_check_common(&task).await;
}

#[tokio::test]
async fn qa_07_vi_hoi_thong_tin_chung() {
    let task = GoldenTask {
        id: "qa_07_vi_hoi_thong_tin_chung",
        message: "một năm có bao nhiêu tháng",
        scripted_responses: vec!["Một năm có 12 tháng.".to_string()],
        expected_tool: None,
        expected_outcome: "success",
    };
    run_and_check_common(&task).await;
}

#[tokio::test]
async fn qa_08_en_apology_no_tool() {
    let task = GoldenTask {
        id: "qa_08_en_apology_no_tool",
        message: "never mind, forget it",
        scripted_responses: vec!["No problem, let me know if you need anything else.".to_string()],
        expected_tool: None,
        expected_outcome: "success",
    };
    run_and_check_common(&task).await;
}
