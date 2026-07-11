//! Pure accumulation/eviction logic for the GUI's proactive-card watch channel
//! (phase 08). Split out of `gui.rs` so the `Adapter` impl stays thin and this
//! logic is unit-testable without spinning up a `GuiAdapter`.

use crate::{ProactiveCard, ProactiveCardKind};

/// Max discrete `Alert` cards the GUI panel holds; see `upsert_proactive_card`.
pub(crate) const MAX_ALERT_CARDS: usize = 10;
/// Max discrete `ReminderFired` cards the GUI panel holds; see `upsert_proactive_card`.
pub(crate) const MAX_REMINDER_CARDS: usize = 10;
/// Max discrete `DistillationProposal` cards the GUI panel holds (phase 8) — kept small: a
/// proposal is a considered, user-approved action, not a high-frequency event.
pub(crate) const MAX_DISTILLATION_CARDS: usize = 5;

/// Discriminator string for `ProactiveCardKind`, used to group same-kind cards for
/// the panel's per-kind eviction policy without a full `match` at every call site.
fn kind_label(kind: &ProactiveCardKind) -> &'static str {
    match kind {
        ProactiveCardKind::MorningBrief { .. } => "morning_brief",
        ProactiveCardKind::Alert { .. } => "alert",
        ProactiveCardKind::ReminderFired { .. } => "reminder_fired",
        ProactiveCardKind::DistillationProposal { .. } => "distillation_proposal",
    }
}

/// How many cards of `kind`'s variant the panel holds at once. `MorningBrief` is a
/// singleton slot (cap 1) — the generic eviction logic below then naturally replaces
/// "the" brief instead of needing a special case.
fn kind_cap(kind: &ProactiveCardKind) -> usize {
    match kind {
        ProactiveCardKind::MorningBrief { .. } => 1,
        ProactiveCardKind::Alert { .. } => MAX_ALERT_CARDS,
        ProactiveCardKind::ReminderFired { .. } => MAX_REMINDER_CARDS,
        ProactiveCardKind::DistillationProposal { .. } => MAX_DISTILLATION_CARDS,
    }
}

/// Fold a newly-arrived `ProactiveCard` into the panel's current snapshot.
///
/// Each `ProactiveCardKind` gets its OWN eviction bucket (keyed by `kind_label`), so
/// a burst of one kind can never evict a card of a DIFFERENT kind — the red-team
/// requirement that a run of `Alert`s must not evict a still-unread `MorningBrief`.
/// Once a kind is at its cap (`kind_cap`), the OLDEST card of that same kind is
/// dropped to make room (oldest-first, since `cards` is in arrival order) and the
/// drop is logged — this is a display cache, not a queue with delivery guarantees,
/// so silently favoring the newest event within a kind is the correct trade-off.
pub(crate) fn upsert_proactive_card(
    current: &[ProactiveCard],
    new_card: ProactiveCard,
) -> Vec<ProactiveCard> {
    let label = kind_label(&new_card.kind);
    let cap = kind_cap(&new_card.kind);

    let mut cards: Vec<ProactiveCard> = current.to_vec();
    cards.push(new_card);

    let same_kind_count = cards.iter().filter(|c| kind_label(&c.kind) == label).count();
    if same_kind_count > cap {
        let mut to_drop = same_kind_count - cap;
        tracing::debug!(kind = label, dropped = to_drop, "proactive panel: evicting oldest same-kind card(s)");
        cards.retain(|c| {
            if to_drop > 0 && kind_label(&c.kind) == label {
                to_drop -= 1;
                false
            } else {
                true
            }
        });
    }
    cards
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn brief(text: &str) -> ProactiveCard {
        ProactiveCard {
            id: Uuid::new_v4(),
            created_at: "t".into(),
            kind: ProactiveCardKind::MorningBrief { text: text.into() },
        }
    }

    fn reminder(title: &str) -> ProactiveCard {
        ProactiveCard {
            id: Uuid::new_v4(),
            created_at: "t".into(),
            kind: ProactiveCardKind::ReminderFired { reminder_id: Uuid::new_v4(), title: title.into() },
        }
    }

    /// A second `MorningBrief` replaces the first (singleton slot, cap 1) rather than
    /// accumulating — there is only ever "the" current brief.
    #[test]
    fn morning_brief_is_a_singleton_slot() {
        let after_first = upsert_proactive_card(&[], brief("first"));
        let after_second = upsert_proactive_card(&after_first, brief("second"));

        assert_eq!(after_second.len(), 1);
        assert!(matches!(&after_second[0].kind, ProactiveCardKind::MorningBrief { text } if text == "second"));
    }

    /// Direct unit test of the eviction contract: pushing past `MAX_REMINDER_CARDS`
    /// drops the OLDEST reminder card, not an arbitrary one.
    #[test]
    fn upsert_evicts_oldest_same_kind_card_when_over_cap() {
        let mut cards = Vec::new();
        for i in 0..(MAX_REMINDER_CARDS + 1) {
            cards = upsert_proactive_card(&cards, reminder(&format!("r{i}")));
        }

        assert_eq!(cards.len(), MAX_REMINDER_CARDS);
        // "r0" was the oldest reminder — it must be the one evicted.
        assert!(cards.iter().all(|c| !matches!(&c.kind, ProactiveCardKind::ReminderFired { title, .. } if title == "r0")));
        assert!(cards.iter().any(|c| matches!(&c.kind, ProactiveCardKind::ReminderFired { title, .. } if title == "r1")));
        let expected_newest = format!("r{MAX_REMINDER_CARDS}");
        assert!(cards.iter().any(|c| matches!(&c.kind, ProactiveCardKind::ReminderFired { title, .. } if *title == expected_newest)));
    }

    /// A burst of same-kind cards never touches a DIFFERENT kind's slot — the
    /// building block `gui.rs`'s `alert_burst_does_not_evict_morning_brief`
    /// integration test relies on.
    #[test]
    fn different_kind_cards_never_evict_each_other() {
        let mut cards = upsert_proactive_card(&[], brief("today"));
        for i in 0..(MAX_REMINDER_CARDS + 3) {
            cards = upsert_proactive_card(&cards, reminder(&format!("r{i}")));
        }

        let briefs = cards.iter().filter(|c| matches!(c.kind, ProactiveCardKind::MorningBrief { .. })).count();
        assert_eq!(briefs, 1, "morning brief must survive a reminder burst");
    }
}
