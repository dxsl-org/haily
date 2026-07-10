/// Domain agent configurations for the Phase 12 multi-agent system.
///
/// Each `DomainConfig` describes one L1 domain agent: the tool name exposed to L0,
/// the description shown to the L0 LLM for routing, the system prompt injected into
/// the sub-turn, and the tool whitelist the domain agent is allowed to use.
///
/// L2 specialists are not listed here — they are registered by their parent L1 agent
/// as additional delegate tools in the sub-registry (Phase 12 Phase B+).
///
// PHASE 4 (C2): connector op-names are whitelisted for the delegable "business" domain
// below (`CONNECTOR_OP_WHITELIST`) so a sub-agent can SEE connector tools once the
// orchestrator registers them into `base_v1` (see `Orchestrator::init`). `sub_registry`
// silently skips a whitelisted name that is not currently registered, so listing a
// connector op here is inert when no manifest declares it and live when one does — no
// phantom-name failure. The `all_domain_whitelists_resolve` test (lib.rs) registers a
// representative manifest declaring these exact op-names before asserting resolution.
pub struct DomainConfig {
    /// Tool name exposed to the L0 LLM, e.g. "delegate_to_developer".
    pub tool_name: &'static str,
    /// One-sentence description used by the L0 LLM to decide when to delegate.
    pub description: &'static str,
    /// System prompt for the L1 domain agent sub-turn.
    pub system_prompt: &'static str,
    /// Subset of V1 tool names the domain agent may call.
    pub allowed_tools: &'static [&'static str],
    /// Model tier this domain's sub-turns should prefer (Phase 7 tier foundation —
    /// wired but inert). `None` means "use the router's default model", which is
    /// every domain's value today — full complexity-based auto-routing is YAGNI
    /// until a task-outcome quality signal exists.
    pub model_tier: Option<haily_llm::Tier>,
}

/// Connector op-names the "business" (CRM-in-a-box) domain is allowed to call once a
/// human-approved manifest registers them into `base_v1` (C2). These are the CONVENTIONAL
/// op-names a first-party Odoo manifest declares (phase 5); listing them in the business
/// domain's `allowed_tools` is inert until a manifest actually declares them
/// (`sub_registry` skips unknown names) and live the moment one does.
///
/// Test-only: the canonical list the C2 whitelist-resolution tests build a representative
/// manifest from. The runtime whitelist lives inline in `delegate_to_business`'s
/// `allowed_tools`; this const keeps the two in sync under test.
#[cfg(test)]
pub const CONNECTOR_OP_WHITELIST: &[&str] = &[
    "odoo_contact_create",
    "odoo_contact_write",
    "odoo_contact_search_read",
];

/// The read-only coding subset (Sub-Agent + Skill Architecture phase 1): the whitelist a
/// future scout stage (P5) gets — pure reads over a workspace, no mutation/exec. Kept as its
/// own const (distinct from the developer domain's full surface) so the scout stage can be
/// wired to exactly this set, and so `wiring_tests` proves every name resolves in `build_v1`.
///
/// Test-only for now (mirrors `CONNECTOR_OP_WHITELIST`): P5 promotes it to a runtime scout
/// `sub_registry` whitelist. Until then only the wiring test consumes it.
#[cfg(test)]
pub const SCOUT_CODING_TOOLS: &[&str] = &["fs_read", "fs_list", "fs_grep"];

pub const DOMAINS: &[DomainConfig] = &[
    DomainConfig {
        tool_name: "delegate_to_developer",
        description: "Software development tasks: coding, debugging, architecture, code review, testing, git, devops, security, database design.",
        system_prompt: "Bạn là Developer agent của Haily — chuyên gia kỹ thuật phần mềm. \
Nhiệm vụ của bạn: phân tích yêu cầu kỹ thuật, lên kế hoạch implement, review code, debug, và tư vấn kiến trúc. \
Trả lời súc tích, kỹ thuật, chính xác. Dùng code block khi cần. \
Không làm những việc ngoài phạm vi kỹ thuật phần mềm.",
        allowed_tools: &[
            "web_search", "url_fetch",
            "note_save", "note_search", "note_update",
            "task_create", "task_list", "task_complete",
            "memory_remember", "memory_search",
            // Coding tool surface (Sub-Agent + Skill Architecture phase 1) — the developer
            // domain gets the full file/search/shell/git surface + domain-agnostic code_exec.
            "fs_read", "fs_list", "fs_grep",
            "fs_write", "fs_edit", "fs_move", "fs_delete",
            "shell_exec", "code_exec",
            "git_status", "git_diff", "git_commit",
        ],
        model_tier: None,
    },
    DomainConfig {
        tool_name: "delegate_to_researcher",
        description: "Deep research tasks: literature review, fact-checking, synthesizing information from multiple sources, building knowledge graphs.",
        system_prompt: "Bạn là Researcher agent của Haily — chuyên gia nghiên cứu và tổng hợp thông tin. \
Nhiệm vụ: tìm kiếm thông tin từ nhiều nguồn, đánh giá độ tin cậy, tổng hợp insights, và fact-check. \
Luôn trích dẫn nguồn. Phân biệt rõ fact vs opinion. Không bịa đặt thông tin.",
        allowed_tools: &[
            "web_search", "url_fetch",
            "note_save", "note_search", "note_update",
            "memory_remember", "memory_search", "memory_list",
            // Domain-agnostic sandboxed execution for data scripts (harness-first).
            "code_exec",
        ],
        model_tier: None,
    },
    DomainConfig {
        tool_name: "delegate_to_finance",
        description: "Personal finance tasks: budgeting, expense tracking, investment advice, market data lookup, tax planning.",
        system_prompt: "Bạn là Finance agent của Haily — chuyên gia tài chính cá nhân. \
Nhiệm vụ: theo dõi thu chi, lên ngân sách, phân tích đầu tư, tra cứu thị trường, và tư vấn thuế. \
Luôn nhắc nhở rủi ro khi tư vấn đầu tư. Không đưa ra lời khuyên pháp lý.",
        allowed_tools: &[
            "web_search",
            "note_save", "note_search", "note_update",
            "memory_remember", "memory_search",
            // Domain-agnostic sandboxed execution for financial calculations.
            "code_exec",
        ],
        model_tier: None,
    },
    DomainConfig {
        tool_name: "delegate_to_life",
        description: "Personal life assistance: health tracking, travel planning, learning schedules, entertainment recommendations, relationship management.",
        system_prompt: "Bạn là Life Assistant agent của Haily — trợ lý cuộc sống cá nhân. \
Nhiệm vụ: hỗ trợ sức khỏe, lên kế hoạch du lịch, theo dõi học tập, gợi ý giải trí, và quản lý các mối quan hệ. \
Ưu tiên sức khỏe và wellbeing của người dùng. Không thay thế tư vấn y tế chuyên nghiệp.",
        allowed_tools: &[
            "calendar_list", "calendar_add",
            "reminder_add", "reminder_list",
            "note_save", "note_search", "note_update",
            "memory_remember", "memory_search",
        ],
        model_tier: None,
    },
    DomainConfig {
        tool_name: "delegate_to_creator",
        description: "Content creation tasks: writing, scriptwriting, content editing, social media scheduling, media production planning.",
        system_prompt: "Bạn là Creator agent của Haily — chuyên gia sáng tạo nội dung. \
Nhiệm vụ: viết lách, chỉnh sửa nội dung, lên kịch bản, lên lịch đăng bài, và hỗ trợ sản xuất media. \
Giữ giọng văn nhất quán theo yêu cầu của người dùng. Sáng tạo nhưng phải phù hợp mục đích.",
        allowed_tools: &[
            "web_search",
            "note_save", "note_search", "note_update",
            "task_create", "task_list",
            "memory_remember", "memory_search",
        ],
        model_tier: None,
    },
    DomainConfig {
        tool_name: "delegate_to_business",
        description: "Business work tasks: CRM management, email drafting, meeting preparation, report writing, project tracking.",
        system_prompt: "Bạn là Business Worker agent của Haily — trợ lý công việc doanh nghiệp. \
Nhiệm vụ: quản lý CRM, soạn email, chuẩn bị meeting, viết báo cáo, và theo dõi dự án business. \
Chuyên nghiệp, súc tích, đúng deadline. Ưu tiên action items rõ ràng.",
        allowed_tools: &[
            "calendar_list", "calendar_add",
            "note_save", "note_search", "note_update",
            "task_create", "task_list", "task_complete",
            "memory_remember", "memory_search",
            // Connector ops (C2) — inert until a manifest declares them, live once one
            // does. Kept in sync with `CONNECTOR_OP_WHITELIST`.
            "odoo_contact_create", "odoo_contact_write", "odoo_contact_search_read",
        ],
        model_tier: None,
    },
];
