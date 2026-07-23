//! Maps one `/name arg` slash lookup against the current [`SlashRegistry`] snapshot to a
//! dispatch-layer [`TriggerAction`] (Unified Chat UI phase 2). Reproduces today's
//! `trigger::resolve_slash` built-in mapping exactly (`plan`/`code`/`build`), plus the new
//! `SkillTurn` case: tag `Request.forced_skill` and route as an ordinary chat turn — the
//! actual gate re-validation happens downstream at the injection site
//! (`haily-kms::KmsHandle::resolve_forced_skill`), never here.
use super::{BuiltInKind, SlashAction, SlashRegistry};
use crate::trigger::TriggerAction;
use haily_core::RunKind;
use haily_types::Request;

/// `req` is mutated ONLY on the `SkillTurn` branch (sets `forced_skill`); every other branch
/// leaves it untouched. An unregistered `name` yields `TriggerAction::UnknownSlashHint` — a
/// slash command never silently disappears.
pub fn resolve(req: &mut Request, name: &str, arg: &str, registry: &SlashRegistry) -> TriggerAction {
    let Some(cmd) = registry.lookup(name) else {
        return TriggerAction::UnknownSlashHint(name.to_string());
    };
    match cmd.action {
        SlashAction::BuiltIn(BuiltInKind::Plan) => {
            if arg.is_empty() {
                TriggerAction::PromptTask(RunKind::Plan)
            } else {
                TriggerAction::LaunchPlan(arg.to_string())
            }
        }
        SlashAction::BuiltIn(BuiltInKind::Build) => {
            if arg.is_empty() {
                TriggerAction::PromptTask(RunKind::Build)
            } else {
                TriggerAction::LaunchBuild(arg.to_string())
            }
        }
        SlashAction::BuiltIn(BuiltInKind::PassThrough) => TriggerAction::NormalTurn,
        SlashAction::SkillTurn(skill_name) => {
            req.forced_skill = Some(skill_name);
            TriggerAction::NormalTurn
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use haily_db::queries::skills as db_skills;
    use haily_kms::KmsHandle;
    use haily_types::{DepthMode, RequestOrigin};
    use uuid::Uuid;

    fn make_request(message: &str) -> Request {
        Request {
            session_id: Uuid::new_v4(),
            adapter_id: "mock".to_string(),
            message: message.to_string(),
            user_ref: None,
            depth: DepthMode::Normal,
            origin: RequestOrigin::Chat,
            forced_skill: None,
        }
    }

    async fn built_registry() -> (SlashRegistry, haily_db::DbHandle, KmsHandle, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = haily_db::DbHandle::init(&dir.path().join("t.db")).await.unwrap();
        db_skills::insert_skill(&db, "fix-bug", "diagnose and fix a bug", "pattern", "[]")
            .await
            .unwrap();
        let kms = KmsHandle::init(db.clone(), dir.path()).await.unwrap();
        let registry = SlashRegistry::new();
        registry.rebuild(&kms, &db).await;
        (registry, db, kms, dir)
    }

    #[tokio::test]
    async fn plan_with_task_launches_plan() {
        let (registry, ..) = built_registry().await;
        let mut req = make_request("/plan add dark mode");
        match resolve(&mut req, "plan", "add dark mode", &registry) {
            TriggerAction::LaunchPlan(task) => assert_eq!(task, "add dark mode"),
            other => panic!("expected LaunchPlan, got {other:?}"),
        }
        assert_eq!(req.forced_skill, None);
    }

    #[tokio::test]
    async fn plan_with_no_arg_prompts_instead_of_launching() {
        let (registry, ..) = built_registry().await;
        let mut req = make_request("/plan");
        assert!(matches!(
            resolve(&mut req, "plan", "", &registry),
            TriggerAction::PromptTask(RunKind::Plan)
        ));
    }

    #[tokio::test]
    async fn code_and_build_alias_launch_build() {
        let (registry, ..) = built_registry().await;
        for name in ["code", "build"] {
            let mut req = make_request("irrelevant");
            match resolve(&mut req, name, "fix the login bug", &registry) {
                TriggerAction::LaunchBuild(task) => assert_eq!(task, "fix the login bug"),
                other => panic!("expected LaunchBuild for {name}, got {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn registered_passthrough_command_forwards_as_normal_turn() {
        let (registry, ..) = built_registry().await;
        let mut req = make_request("/review");
        assert!(matches!(resolve(&mut req, "review", "", &registry), TriggerAction::NormalTurn));
    }

    #[tokio::test]
    async fn unknown_command_returns_hint_not_a_swallow() {
        let (registry, ..) = built_registry().await;
        let mut req = make_request("/frobnicate");
        match resolve(&mut req, "frobnicate", "", &registry) {
            TriggerAction::UnknownSlashHint(name) => assert_eq!(name, "frobnicate"),
            other => panic!("expected UnknownSlashHint, got {other:?}"),
        }
    }

    /// A synthesized-skill slash command tags `forced_skill` and routes as a normal turn — the
    /// actual gate check happens later, at context assembly, not here.
    #[tokio::test]
    async fn skill_turn_tags_forced_skill_and_routes_normal_turn() {
        let (registry, ..) = built_registry().await;
        let mut req = make_request("/fix-bug");
        assert!(matches!(resolve(&mut req, "fix-bug", "", &registry), TriggerAction::NormalTurn));
        assert_eq!(req.forced_skill.as_deref(), Some("fix-bug"));
    }
}
