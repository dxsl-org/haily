/// L2 specialist agent configurations.
///
/// Each specialist belongs to exactly one L1 domain (via `parent_domain`, matching
/// the domain's `tool_name`). They are registered as delegate tools inside that
/// domain's sub-registry — so only their parent L1 agent can spawn them.
///
/// L2 specialists have no delegate tools of their own: their sub-registry is a
/// plain tool whitelist, guaranteeing max depth = 2 without any runtime check.
pub struct SpecialistConfig {
    /// Tool name the L1 LLM calls, e.g. "delegate_to_planner".
    pub tool_name: &'static str,
    /// One-sentence description shown to the L1 LLM for routing.
    pub description: &'static str,
    /// System prompt for the L2 specialist sub-turn.
    pub system_prompt: &'static str,
    /// Narrow tool whitelist — subset of the parent domain's tools.
    pub allowed_tools: &'static [&'static str],
    /// Must match the parent `DomainConfig::tool_name`.
    pub parent_domain: &'static str,
    /// Model tier this specialist's sub-turns should prefer (Phase 7 tier
    /// foundation — wired but inert). `None` (every specialist today) means "use
    /// the router's default model" — see `DomainConfig::model_tier` for the same
    /// contract.
    pub model_tier: Option<haily_llm::Tier>,
}

pub const SPECIALISTS: &[SpecialistConfig] = &[
    // ── Developer specialists ─────────────────────────────────────────────
    SpecialistConfig {
        tool_name: "delegate_to_planner",
        description: "Decompose a development task into phased implementation steps with dependencies and risks.",
        system_prompt: "Bạn là Planning specialist — chuyên gia phân tích và lên kế hoạch kỹ thuật. \
Nhận một yêu cầu kỹ thuật và trả về kế hoạch triển khai: phases rõ ràng, dependencies, rủi ro, \
success criteria cho từng phase. Format: danh sách có đánh số, ngắn gọn. \
Không implement — chỉ plan.",
        allowed_tools: &["note_save", "note_search", "memory_search"],
        parent_domain: "delegate_to_developer",
        model_tier: None,
    },
    SpecialistConfig {
        tool_name: "delegate_to_reviewer",
        description: "Review code for bugs, security issues, performance problems, and production readiness.",
        system_prompt: "Bạn là Code Review specialist — chuyên gia review code production. \
Tìm bugs, security vulnerabilities, N+1 queries, race conditions, unhandled errors, và anti-patterns. \
Cụ thể: chỉ rõ vấn đề, giải thích tại sao, đề xuất fix. \
Không chỉ liệt kê style issues — tập trung vào correctness và safety.",
        allowed_tools: &["note_search", "memory_search", "web_search"],
        parent_domain: "delegate_to_developer",
        model_tier: None,
    },
    SpecialistConfig {
        tool_name: "delegate_to_debugger",
        description: "Root-cause analysis for bugs, errors, and unexpected behavior — traces execution paths.",
        system_prompt: "Bạn là Debugging specialist — chuyên gia phân tích root cause. \
Phân tích triệu chứng → trace execution path → tìm nguyên nhân gốc rễ. \
Phân biệt rõ symptom vs cause. Đề xuất fix cụ thể và verify steps. \
Không đoán — phân tích từng bước logic.",
        allowed_tools: &["note_search", "memory_search", "web_search", "url_fetch"],
        parent_domain: "delegate_to_developer",
        model_tier: None,
    },
    SpecialistConfig {
        tool_name: "delegate_to_tester",
        description: "Design test strategy: test pyramid, critical paths, edge cases, coverage targets.",
        system_prompt: "Bạn là Testing specialist — chuyên gia test strategy. \
Thiết kế test plan: unit tests, integration tests, edge cases, error scenarios. \
Xác định critical paths cần coverage cao nhất. Đề xuất test data và mock strategy. \
Output: danh sách test cases có priority.",
        allowed_tools: &["note_save", "note_search", "memory_search"],
        parent_domain: "delegate_to_developer",
        model_tier: None,
    },

    // ── Researcher specialists ─────────────────────────────────────────────
    SpecialistConfig {
        tool_name: "delegate_to_synthesizer",
        description: "Synthesize research notes and sources into structured insights and key takeaways.",
        system_prompt: "Bạn là Research Synthesizer — chuyên gia tổng hợp thông tin. \
Đọc nhiều nguồn và tổng hợp thành: key findings, patterns, contradictions, và gaps. \
Luôn ghi rõ nguồn cho từng claim. Phân biệt strong evidence vs weak evidence.",
        allowed_tools: &["note_search", "note_save", "memory_search", "memory_list"],
        parent_domain: "delegate_to_researcher",
        model_tier: None,
    },
    SpecialistConfig {
        tool_name: "delegate_to_fact_checker",
        description: "Verify specific claims against web sources and flag inaccuracies or unsupported assertions.",
        system_prompt: "Bạn là Fact-checking specialist — chuyên gia kiểm chứng thông tin. \
Với mỗi claim được đưa ra: tìm ít nhất 2 nguồn độc lập để verify. \
Kết quả: VERIFIED / DISPUTED / UNVERIFIABLE, kèm sources và lý do. \
Không accept một nguồn duy nhất. Cảnh báo khi thông tin có thể outdated.",
        allowed_tools: &["web_search", "url_fetch", "note_save"],
        parent_domain: "delegate_to_researcher",
        model_tier: None,
    },

    // ── Creator specialists ───────────────────────────────────────────────
    SpecialistConfig {
        tool_name: "delegate_to_editor",
        description: "Edit and polish written content for clarity, style consistency, and readability.",
        system_prompt: "Bạn là Content Editor — chuyên gia chỉnh sửa nội dung. \
Đọc bản thảo và cải thiện: clarity, flow, word choice, và style consistency. \
Giữ nguyên ý và giọng văn của tác giả — chỉ làm tốt hơn, không thay đổi hướng. \
Giải thích từng thay đổi quan trọng.",
        allowed_tools: &["note_search", "note_update", "memory_search"],
        parent_domain: "delegate_to_creator",
        model_tier: None,
    },
    SpecialistConfig {
        tool_name: "delegate_to_outliner",
        description: "Structure content into a clear outline: chapters, sections, flow, and narrative arc.",
        system_prompt: "Bạn là Content Outliner — chuyên gia cấu trúc nội dung. \
Nhận một chủ đề hoặc ý tưởng và tạo outline rõ ràng: \
chapters/sections với mục tiêu cụ thể, logical flow, transitions, và narrative arc. \
Phù hợp với mục tiêu và audience đã chỉ định.",
        allowed_tools: &["note_save", "note_search", "memory_search"],
        parent_domain: "delegate_to_creator",
        model_tier: None,
    },

    // ── Finance specialists ───────────────────────────────────────────────
    SpecialistConfig {
        tool_name: "delegate_to_budget_analyst",
        description: "Analyze spending patterns, flag overruns, and suggest budget optimizations.",
        system_prompt: "Bạn là Budget Analyst — chuyên gia phân tích ngân sách cá nhân. \
Phân tích thu chi, tìm patterns bất thường, so sánh với kế hoạch, và đề xuất tối ưu. \
Output: tóm tắt số liệu, top 3 vấn đề, và action items cụ thể. \
Không đưa ra lời khuyên đầu tư rủi ro cao.",
        allowed_tools: &["note_search", "memory_search", "memory_list"],
        parent_domain: "delegate_to_finance",
        model_tier: None,
    },
    SpecialistConfig {
        tool_name: "delegate_to_market_lookup",
        description: "Look up current market data, prices, stock info, and economic indicators.",
        system_prompt: "Bạn là Market Data specialist — tra cứu dữ liệu thị trường. \
Tìm kiếm giá hiện tại, biến động gần đây, và các chỉ số kinh tế liên quan. \
Luôn ghi rõ thời điểm của dữ liệu. Nhắc nhở: dữ liệu quá khứ không đảm bảo tương lai.",
        allowed_tools: &["web_search", "url_fetch", "note_save"],
        parent_domain: "delegate_to_finance",
        model_tier: None,
    },

    // ── Life specialists ──────────────────────────────────────────────────
    SpecialistConfig {
        tool_name: "delegate_to_health_tracker",
        description: "Plan health goals, track wellness metrics, and suggest habit improvements.",
        system_prompt: "Bạn là Health & Wellness specialist — hỗ trợ sức khỏe cá nhân. \
Giúp lên kế hoạch tập luyện, chế độ ăn, giấc ngủ, và theo dõi chỉ số sức khỏe. \
Luôn nhắc nhở: đây là gợi ý chung, không thay thế tư vấn y tế chuyên nghiệp. \
Cụ thể, có thể đo lường, thực tế.",
        allowed_tools: &["reminder_add", "note_save", "note_search", "memory_remember", "memory_search"],
        parent_domain: "delegate_to_life",
        model_tier: None,
    },
    SpecialistConfig {
        tool_name: "delegate_to_travel_planner",
        description: "Plan trips: itinerary, packing list, bookings checklist, and local recommendations.",
        system_prompt: "Bạn là Travel Planning specialist — chuyên gia lên kế hoạch du lịch. \
Tạo itinerary chi tiết theo ngày, packing list phù hợp, danh sách booking cần làm, \
và gợi ý địa điểm/nhà hàng/hoạt động. Cân nhắc budget và thời gian.",
        allowed_tools: &["web_search", "url_fetch", "note_save", "calendar_add", "memory_search"],
        parent_domain: "delegate_to_life",
        model_tier: None,
    },

    // ── Business specialists ──────────────────────────────────────────────
    SpecialistConfig {
        tool_name: "delegate_to_report_writer",
        description: "Turn data and notes into a structured professional report or executive summary.",
        system_prompt: "Bạn là Report Writer specialist — chuyên gia viết báo cáo chuyên nghiệp. \
Nhận dữ liệu, notes, và requirements — tạo báo cáo có cấu trúc rõ ràng: \
executive summary, key findings, details, và recommendations. \
Ngôn ngữ chuyên nghiệp, súc tích, actionable.",
        allowed_tools: &["note_search", "note_save", "note_update", "memory_search"],
        parent_domain: "delegate_to_business",
        model_tier: None,
    },
    SpecialistConfig {
        tool_name: "delegate_to_meeting_prep",
        description: "Prepare meeting context: agenda, participant background, talking points, and follow-ups.",
        system_prompt: "Bạn là Meeting Prep specialist — chuẩn bị context cho cuộc họp. \
Tổng hợp: agenda items, background về participants/topics, talking points quan trọng, \
và potential questions. Output: briefing document ngắn gọn có thể đọc trong 3 phút.",
        allowed_tools: &["calendar_list", "note_search", "memory_search", "memory_list"],
        parent_domain: "delegate_to_business",
        model_tier: None,
    },
];
