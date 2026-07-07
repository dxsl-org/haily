//! Section B of `.agents/260707-assistant-depth/reports/voice-eval-criteria.md` — per-soul
//! required/forbidden/density checks. Kept in its own module (rather than `mod.rs`) purely to
//! respect the <200-line file-size convention; `super::check_voice` is the only public entry
//! point that calls into this file.

use super::{contains_ci, contains_emoji, tokenize_words, VoiceFailure};

const FORBIDDEN_PARTICLES: &[&str] = &["nhé", "nha", "ạ"];
const HOAMI_REQUIRED_PARTICLES: &[&str] = &["nhé", "nha", "ạ", "đó"];
const TETE_MAX_WORDS: usize = 40;

fn count_particles(text: &str, particles: &[&str]) -> usize {
    let words = tokenize_words(text);
    words.iter().filter(|w| particles.contains(&w.as_str())).count()
}

fn count_sentences(text: &str) -> usize {
    text.split(['.', '!', '?']).filter(|s| !s.trim().is_empty()).count()
}

/// Approximates regex `\w+:\s` (a `Label: value` line) without adding `regex` for one
/// throwaway pattern (YAGNI): a `:` followed by whitespace, preceded by a word char.
fn has_label_colon_pattern(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if c == ':' && i > 0 && i + 1 < chars.len() && chars[i + 1].is_whitespace() {
            let prev = chars[i - 1];
            if prev.is_alphanumeric() || prev == '_' {
                return true;
            }
        }
    }
    false
}

/// Word-boundary check, not substring — a naive scan for "à" would false-positive on any
/// word containing that letter (e.g. "cà phê"), the same brittleness the criteria doc calls
/// out for excluding "đó" from Haily's forbidden set.
fn tete_forbidden_opener_present(text: &str) -> bool {
    contains_ci(text, "chào bạn") || tokenize_words(text).iter().any(|w| w == "dạ" || w == "à")
}

pub(super) fn check_haily(text: &str, out: &mut Vec<VoiceFailure>) {
    if contains_emoji(text) {
        out.push(VoiceFailure::new("haily_forbidden_emoji", "emoji present"));
    }
    let particles = count_particles(text, FORBIDDEN_PARTICLES);
    if particles > 0 {
        out.push(VoiceFailure::new("haily_forbidden_particle", format!("{particles} softening particle(s)")));
    }
    let bangs = text.matches('!').count();
    if bangs > 1 {
        out.push(VoiceFailure::new("haily_tone_exclamation", format!("{bangs} '!' in response")));
    }
}

pub(super) fn check_tete(text: &str, out: &mut Vec<VoiceFailure>) {
    let has_marker = ['→', ':', '/', '='].iter().any(|c| text.contains(*c)) || has_label_colon_pattern(text);
    if !has_marker {
        out.push(VoiceFailure::new("tete_required_marker", "no data-structure marker (→ : / = or Label:)"));
    }
    if contains_emoji(text) {
        out.push(VoiceFailure::new("tete_forbidden_emoji", "emoji present"));
    }
    let particles = count_particles(text, FORBIDDEN_PARTICLES);
    if particles > 0 {
        out.push(VoiceFailure::new("tete_forbidden_particle", format!("{particles} softening particle(s)")));
    }
    if tete_forbidden_opener_present(text) {
        out.push(VoiceFailure::new("tete_forbidden_opener", "social opener present"));
    }
    let words = text.split_whitespace().count();
    if words > TETE_MAX_WORDS {
        out.push(VoiceFailure::new("tete_length_bound", format!("{words} words > {TETE_MAX_WORDS}")));
    }
}

pub(super) fn check_hoami(text: &str, out: &mut Vec<VoiceFailure>) {
    let particles = count_particles(text, HOAMI_REQUIRED_PARTICLES);
    if particles == 0 {
        out.push(VoiceFailure::new("hoami_required_particle", "no particle from {nhé, nha, ạ, đó}"));
        return;
    }
    let sentences = count_sentences(text).max(1);
    let density = particles as f64 / sentences as f64;
    if density > 0.5 {
        out.push(VoiceFailure::new(
            "hoami_particle_density",
            format!("{particles} particles / {sentences} sentences = {density:.2} > 0.5"),
        ));
    }
}

pub(super) fn check_lungmat(text: &str, out: &mut Vec<VoiceFailure>) {
    let has_energy = contains_emoji(text) || text.contains('!') || text.contains("...") || text.contains('…');
    if !has_energy {
        out.push(VoiceFailure::new("lungmat_required_energy", "no emoji, '!' or '...' present"));
    }
}
