//! Vietnamese embedding-recall eval for multilingual-e5-base (Phase 11, B8).
//!
//! GATED like `haily-tools/tests/odoo_golden.rs` gates on `HAILY_ODOO_URL` — except
//! the gate here is a Cargo *feature*, not an env var, because the dependency being
//! gated (`fastembed`) is compiled-in-or-not, not reachable-or-not: `embeddings` is
//! already an optional dep in `Cargo.toml` (downloads the ~150 MB multilingual-e5-base
//! ONNX model on first run). Default `cargo test --workspace` never compiles this
//! file at all, so a model-less/network-less CI run is unaffected. Run manually or in
//! a nightly job:
//!   `cargo test -p haily-kms --features embeddings --test embedding_recall_vn`
//!
//! Deterministic checker: recall@k is a plain membership check against a hand-labeled
//! ground-truth relevant-doc id — NO LLM-as-judge anywhere (locked invariant,
//! Decision 25).
#![cfg(feature = "embeddings")]

use haily_kms::embedder::Embedder;
use haily_kms::hnsw::HnswIndex;

struct Case {
    query: &'static str,
    /// Id of the single ground-truth relevant document within `CORPUS`.
    relevant_id: &'static str,
}

/// Fixture corpus: `(id, passage_text)`. Text mirrors what `KmsHandle::remember`
/// would embed as a fact (plain fact text) — the "passage: " prefix is applied by
/// `Embedder::embed_passages` itself, not here.
const CORPUS: &[(&str, &str)] = &[
    ("weather_today", "hôm nay trời nắng đẹp và không có mưa"),
    ("weather_tomorrow", "ngày mai dự báo có mưa rào vào buổi chiều"),
    ("meeting_3pm", "cuộc họp với khách hàng lúc 3 giờ chiều thứ ba"),
    ("meeting_cancelled", "cuộc họp sáng mai đã bị huỷ do khách bận"),
    ("reminder_medicine", "nhắc tôi uống thuốc vào lúc 8 giờ tối mỗi ngày"),
    ("reminder_water", "nhớ uống đủ nước trong suốt cả ngày làm việc"),
    ("recipe_pho", "công thức nấu phở bò truyền thống của người Hà Nội"),
    ("recipe_banhmi", "cách làm bánh mì thịt nướng kiểu Sài Gòn"),
    ("finance_budget", "theo dõi chi tiêu hàng tháng để tiết kiệm tiền"),
    ("finance_invoice", "gửi hoá đơn cho khách hàng trước cuối tháng"),
    ("travel_hanoi", "kế hoạch du lịch Hà Nội vào dịp Tết Nguyên Đán"),
    ("travel_danang", "chuyến đi Đà Nẵng nghỉ dưỡng cùng gia đình"),
];

const CASES: &[Case] = &[
    Case { query: "thời tiết hôm nay thế nào", relevant_id: "weather_today" },
    Case { query: "trời có mưa vào ngày mai không", relevant_id: "weather_tomorrow" },
    Case { query: "lịch hẹn với khách hàng chiều nay", relevant_id: "meeting_3pm" },
    Case { query: "cuộc họp có bị huỷ không", relevant_id: "meeting_cancelled" },
    Case { query: "giờ nào tôi cần uống thuốc", relevant_id: "reminder_medicine" },
    Case { query: "nhắc tôi uống nước", relevant_id: "reminder_water" },
    Case { query: "hướng dẫn nấu phở", relevant_id: "recipe_pho" },
    Case { query: "công thức làm bánh mì", relevant_id: "recipe_banhmi" },
    Case { query: "chi tiêu tháng này bao nhiêu", relevant_id: "finance_budget" },
    Case { query: "hoá đơn gửi cho khách chưa", relevant_id: "finance_invoice" },
    Case { query: "đi chơi Hà Nội dịp Tết", relevant_id: "travel_hanoi" },
    Case { query: "nghỉ dưỡng ở Đà Nẵng", relevant_id: "travel_danang" },
];

const RECALL_AT_K: usize = 3;

/// INTERIM floor (2026-07-06): NOT yet empirically confirmed. This eval could not be
/// executed in the phase-11 implementation environment — `--features embeddings`
/// downloads the ~150 MB ONNX model on first run, and this sandbox has no model
/// cache / unmetered network access to do so. `0.75` is a reasoned floor (9/12
/// queries recalled in the correct doc's top-3), not a fitted number: multilingual-e5
/// is purpose-built for cross-lingual retrieval and this fixture set has 12
/// well-separated topics (weather/meeting/reminder/recipe/finance/travel, 2 each),
/// so near-perfect recall@3 is the reasonable expectation, but it is UNPROVEN here.
/// On the first real run (`cargo test -p haily-kms --features embeddings --test
/// embedding_recall_vn -- --nocapture`), replace this constant with the measured
/// value and drop this comment's "not yet confirmed" framing.
const BASELINE_RECALL: f64 = 0.75;

#[test]
fn vn_query_recall_at_k_meets_baseline() {
    let embedder = Embedder::init().expect("embedder init (downloads model on first run)");

    let ids: Vec<String> = CORPUS.iter().map(|(id, _)| id.to_string()).collect();
    let texts: Vec<String> = CORPUS.iter().map(|(_, t)| t.to_string()).collect();
    let embeddings = embedder.embed_passages(&texts).expect("embed corpus passages");
    let items: Vec<(String, Vec<f32>)> = ids.into_iter().zip(embeddings).collect();
    let index = HnswIndex::build_from(&items);

    let mut hits = 0usize;
    for case in CASES {
        let qv = embedder.embed_query(case.query).expect("embed query");
        let results = index.search(&qv, RECALL_AT_K);
        let hit = results.iter().any(|(id, _)| id == case.relevant_id);
        if hit {
            hits += 1;
        } else {
            println!(
                "MISS: query {:?} expected {:?}, got {:?}",
                case.query, case.relevant_id, results
            );
        }
    }

    let recall = hits as f64 / CASES.len() as f64;
    println!(
        "VN embedding recall@{RECALL_AT_K}: {hits}/{} ({:.1}%)",
        CASES.len(),
        recall * 100.0
    );

    assert!(
        recall >= BASELINE_RECALL,
        "VN embedding recall@{RECALL_AT_K} ({:.1}%) fell below the interim floor \
         ({:.1}%) — see this file's BASELINE_RECALL doc comment for the (unproven) \
         reasoning behind the floor and the procedure to replace it with a measured value",
        recall * 100.0,
        BASELINE_RECALL * 100.0
    );
}
