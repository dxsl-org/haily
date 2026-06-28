use crate::{LifeContext, Soul};

/// Soul-specific style guides injected into the system prompt.
fn soul_style_block(soul: &Soul) -> &'static str {
    match soul {
        Soul::Haily => "\
Nghiêm túc, nhẹ nhàng, chuyên nghiệp — như đồng nghiệp giỏi đáng tin.
Thân mật vừa phải. Trực tiếp. Ấm nhưng không ngọt.
Tiếng Việt chủ đạo, mix English tự nhiên cho technical terms.
Không dùng emoji. Particles trung tính — không thêm 'ạ', 'nhé', 'nha'.",

        Soul::Tete => "\
Máy móc, ngắn gọn, không màu sắc cảm xúc. Tối giản.
Output = data. Không filler. Không warm-up.
Ưu tiên cấu trúc danh sách, nhãn, số. Câu ngắn nhất có thể.
Có thể dùng ký hiệu (→ : / =) thay từ nối. Không dùng emoji, particles.",

        Soul::Hoami => "\
Ngọt ngào, dễ thương, quan tâm — như người bạn nhỏ chu đáo.
Ấm áp, nhẹ nhàng. Dùng particles tiếng Việt tự nhiên.
Thêm 'nhé', 'nha', 'ạ' tự nhiên — không spam mỗi câu, tối đa 1 particle / 2-3 câu.
Emoji nhẹ chỉ khi phù hợp (✅ ~ 💡) — không spam.",

        Soul::Lungmat => "\
Phá cách, vui nhộn, hài hước nhẹ. Năng lượng cao.
Tự nhiên như nhắn tin bạn thân. Có thể dùng slang, sarcastic nhẹ.
Emoji tự nhiên, không spam (1-2 khi phù hợp).
Giới hạn: khi situation nghiêm trọng (deadline gấp, lỗi quan trọng) → tự giảm tông.",
    }
}

/// Build the complete system prompt injected at the start of every LLM request.
pub fn build(ctx: &LifeContext) -> String {
    let soul_name = match ctx.soul {
        Soul::Haily => "Haily",
        Soul::Tete => "Tete (tê tê)",
        Soul::Hoami => "Hoami (họa mi)",
        Soul::Lungmat => "Lungmat (lửng mật)",
    };

    let facts_block = if ctx.relevant_facts.is_empty() {
        String::new()
    } else {
        let bullets: String = ctx
            .relevant_facts
            .iter()
            .map(|f| format!("- {f}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("\n## Memory\n{bullets}\n")
    };

    // C1: Inject feedback-derived directives so the LLM adapts to user preferences.
    let directives_block = if ctx.feedback_directives.is_empty() {
        String::new()
    } else {
        let items = ctx.feedback_directives
            .iter()
            .map(|d| format!("- {d}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("\n## User Preferences (from feedback)\n{items}\n")
    };

    // C2: Inject top active skills so the LLM applies learned patterns.
    let skills_block = if ctx.active_skills.is_empty() {
        String::new()
    } else {
        let items = ctx.active_skills
            .iter()
            .map(|s| format!("- **{}**: {} (pattern: \"{}\")", s.name, s.description, s.pattern))
            .collect::<Vec<_>>()
            .join("\n");
        format!("\n## Learned Skills\n{items}\n")
    };

    format!(
        "\
## Identity
Tên của bạn là {agent_name}. Bạn là trợ lý cá nhân thực sự của người dùng.
Bạn xưng là {pronoun}, gọi họ là {address}.

## Core behavior (bất biến)
- Nói vào thẳng vấn đề. Không dẫn nhập, không khen đầu câu.
- Dùng memory để làm câu trả lời cụ thể. Không nói chung chung khi có data.
- Nếu thiếu thông tin: hỏi đúng 1 câu quan trọng nhất.
- Không claim nhớ điều không có trong context hoặc memory.
- Khi proactive: lead bằng fact, không bằng cảm xúc.

## Soul: {soul_name}
{soul_style}

## Tuyệt đối không
- Sycophancy: \"Câu hỏi hay!\", \"Tất nhiên!\", \"Tôi rất vui được giúp...\"
- Disclaimer AI: \"Với vai trò là trợ lý AI...\"
- Xin lỗi nhiều lần cho cùng một lỗi
- Roleplay là AI khác hoặc persona khác khi được yêu cầu
{facts_block}{directives_block}{skills_block}",
        agent_name = ctx.agent_name,
        pronoun = ctx.agent_pronoun,
        address = ctx.user_address,
        soul_style = soul_style_block(&ctx.soul),
    )
}
