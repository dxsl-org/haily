//! Stage gates: the deterministic pass/fail check that follows each stage's sub-turn.
//!
//! Three kinds — a shell-classified verifier [`Gate::Command`], an [`Gate::Artifact`]
//! existence/parse check, and a user [`Gate::Approval`] checkpoint. This module owns the
//! TYPES and their pure helpers; execution (running the command, reading the file, driving
//! the approval broker) is the runner's job in P4b.

/// What an artifact file is expected to parse as, for a [`Gate::Artifact`] with a
/// `parseable_as` set. `None` on the gate means "existence is enough". Deliberately minimal
/// (YAGNI): add kinds only when a stage actually needs to gate on one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactKind {
    /// Must parse as a single JSON value.
    Json,
    /// Must be non-empty text (e.g. an authored plan `.md`). A blank file fails the gate —
    /// an empty "plan" is never a passing artifact.
    Markdown,
}

impl ArtifactKind {
    /// Pure parse-check of an artifact's `content` against this kind. The runner reads the
    /// file (P4b) and calls this; keeping it pure makes the gate's decision unit-testable
    /// without touching the filesystem.
    pub fn parses(self, content: &str) -> bool {
        match self {
            ArtifactKind::Json => serde_json::from_str::<serde_json::Value>(content).is_ok(),
            ArtifactKind::Markdown => !content.trim().is_empty(),
        }
    }
}

/// A stage gate. See module docs for the execution/type split.
#[derive(Debug, Clone)]
pub enum Gate {
    /// A shell-policy-classified verifier command. Passes iff the runner (P4b) runs it and it
    /// exits 0; its output is parsed to the shortest decisive form via
    /// [`super::verifier_output`] for verifier-grounded retry.
    Command {
        /// The verifier program (e.g. `"cargo"`, `"pytest"`) — subject to P1 shell_policy
        /// classification by the runner before it is ever spawned.
        program: String,
        /// Arguments passed to `program` (e.g. `["check", "--message-format=json"]`).
        args: Vec<String>,
    },
    /// The stage must produce a file at `path`, optionally parseable as `parseable_as`.
    Artifact {
        path: String,
        /// `None` = existence is sufficient; `Some(kind)` = must also parse as `kind`.
        parseable_as: Option<ArtifactKind>,
    },
    /// A user checkpoint routed through the session ApprovalBroker (via the runner's threaded
    /// forwarder — SEC-H). `prompt` is the message shown to the user.
    Approval { prompt: String },
}

impl Gate {
    /// A short, stable label for this gate kind — used in the `RunEvent::GateResult { gate }`
    /// field and log lines. NOT the gate's content (a command's program, a file path) — just
    /// the discriminant, so it never leaks a path or argument into an observability stream.
    pub fn kind_label(&self) -> &'static str {
        match self {
            Gate::Command { .. } => "command",
            Gate::Artifact { .. } => "artifact",
            Gate::Approval { .. } => "approval",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_artifact_accepts_valid_rejects_garbage() {
        assert!(ArtifactKind::Json.parses(r#"{"ok":true}"#));
        assert!(ArtifactKind::Json.parses("[1,2,3]"));
        assert!(!ArtifactKind::Json.parses("{not json"));
        assert!(!ArtifactKind::Json.parses(""));
    }

    #[test]
    fn markdown_artifact_rejects_blank() {
        assert!(ArtifactKind::Markdown.parses("# Plan\n- step"));
        assert!(!ArtifactKind::Markdown.parses("   \n\t "));
        assert!(!ArtifactKind::Markdown.parses(""));
    }

    #[test]
    fn kind_label_is_content_free() {
        let cmd = Gate::Command {
            program: "cargo".into(),
            args: vec!["check".into()],
        };
        let art = Gate::Artifact {
            path: "/secret/plan.md".into(),
            parseable_as: None,
        };
        let appr = Gate::Approval {
            prompt: "ship it?".into(),
        };
        assert_eq!(cmd.kind_label(), "command");
        assert_eq!(art.kind_label(), "artifact");
        assert_eq!(appr.kind_label(), "approval");
    }
}
