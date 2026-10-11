//! Effort classifier: decides, per message, how hard the engine should
//! think (`low`, `medium`, `high`) and whether the request is clearly hard
//! enough to move from the `chat` slot to `chat_strong`.
//!
//! The default is deterministic, zero-cost rule scoring (no model call).
//! It looks at the request itself (multi-step tasks, code, research,
//! planning, math, long inputs, explicit "think hard") and at the
//! conversation (the user says the last answer was wrong or asks to try
//! again, keeps returning to the same problem, or the previous turn
//! failed), carries some effort over between related follow-ups, and decays
//! back to `low` as soon as the conversation turns casual.
//!
//! Optionally (`router.classifier_model = true`, off by default) the
//! `classifier` slot's cheap model is asked about ambiguous messages; see
//! `turn::refine_with_model`.

use serde::Serialize;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "low" | "minimal" | "none" => Some(Effort::Low),
            "medium" => Some(Effort::Medium),
            "high" | "xhigh" | "max" | "ultra" => Some(Effort::High),
            _ => None,
        }
    }
}

/// A previous message, as the classifier sees it.
#[derive(Debug, Clone)]
pub struct Turn<'a> {
    pub role: &'a str,
    pub content: &'a str,
    /// Assistant messages: ended in an error (including max turns).
    pub failed: bool,
    /// Assistant messages: effort the router chose.
    pub effort: Option<Effort>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Assessment {
    pub effort: Effort,
    /// Clearly hard: use the `chat_strong` slot.
    pub escalate: bool,
    pub score: i32,
    /// Machine-readable signals that fired, strongest first.
    pub signals: Vec<&'static str>,
    /// In the gray zone where an optional model classifier may help.
    pub ambiguous: bool,
}

impl Assessment {
    pub fn reason(&self) -> String {
        if self.signals.is_empty() {
            "everyday message".into()
        } else {
            self.signals.join(", ")
        }
    }
}

pub const MEDIUM_AT: i32 = 3;
pub const HIGH_AT: i32 = 6;
pub const ESCALATE_AT: i32 = 7;

const CODE_WORDS: &[&str] = &[
    "debug",
    "refactor",
    "compile",
    "compiler",
    "regex",
    "sql",
    "script",
    "function",
    "bug",
    "stack trace",
    "traceback",
    "exception",
    "segfault",
    "implement",
    "algorithm",
    "api",
    "unit test",
    "code",
    "error message",
    "syntax",
];
const RESEARCH_WORDS: &[&str] = &[
    "research",
    "compare",
    "comparison",
    "analyze",
    "analyse",
    "analysis",
    "pros and cons",
    "deep dive",
    "in depth",
    "in-depth",
    "comprehensive",
    "sources",
    "evaluate",
    "investigate",
    "literature",
    "trade-offs",
    "tradeoffs",
];
const PLANNING_WORDS: &[&str] = &[
    "plan",
    "planning",
    "strategy",
    "roadmap",
    "design",
    "architecture",
    "itinerary",
    "budget",
    "outline",
    "proposal",
    "timeline",
];
const MATH_WORDS: &[&str] = &[
    "prove",
    "proof",
    "calculate",
    "equation",
    "integral",
    "derivative",
    "probability",
    "solve",
    "theorem",
    "statistics",
    "optimize",
    "optimise",
];
const DEPTH_PHRASES: &[&str] = &[
    "think hard",
    "think harder",
    "think carefully",
    "think it through",
    "take your time",
    "be thorough",
    "thoroughly",
    "step by step",
    "carefully",
];
const MULTI_STEP_PHRASES: &[&str] = &[
    "step by step",
    "first,",
    "and then",
    "after that",
    "for each",
    "each of",
    "multiple",
    "several",
    "all of the",
];
const CORRECTION_PHRASES: &[&str] = &[
    "that's wrong",
    "that is wrong",
    "thats wrong",
    "you're wrong",
    "not right",
    "incorrect",
    "try again",
    "doesn't work",
    "does not work",
    "didn't work",
    "did not work",
    "still broken",
    "still not",
    "still doesn't",
    "still wrong",
    "not what i asked",
    "you missed",
    "that's not it",
    "think harder",
    "wrong answer",
    "that's not correct",
    "nope",
];
const CASUAL: &[&str] = &[
    "hi",
    "hey",
    "hello",
    "thanks",
    "thank you",
    "thx",
    "ty",
    "ok",
    "okay",
    "cool",
    "nice",
    "great",
    "lol",
    "haha",
    "good morning",
    "good night",
    "gn",
    "yes",
    "no",
    "sure",
    "bye",
    "awesome",
    "perfect",
    "got it",
    "sounds good",
    "love it",
    "ok thanks",
    "ok cool",
];
const STOP: &[&str] = &[
    "the", "a", "an", "and", "or", "but", "is", "are", "was", "it", "this", "that", "to", "of",
    "in", "on", "for", "with", "my", "me", "i", "you", "can", "do", "does", "how", "what", "why",
    "please", "again", "still", "not", "be", "have", "just", "so", "if", "at", "your",
];

fn has_word(lower: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    let mut start = 0;
    while let Some(pos) = lower[start..].find(needle) {
        let begin = start + pos;
        let end = begin + needle.len();
        let before_ok = lower[..begin]
            .chars()
            .next_back()
            .is_none_or(|c| !is_word(c) || !needle.starts_with(is_word));
        let after_ok = lower[end..]
            .chars()
            .next()
            .is_none_or(|c| !is_word(c) || !needle.ends_with(is_word));
        if before_ok && after_ok {
            return true;
        }
        start = begin + needle.chars().next().map_or(1, char::len_utf8);
    }
    false
}

fn any_word(lower: &str, words: &[&str]) -> bool {
    words.iter().any(|w| has_word(lower, w))
}

fn content_words(text: &str) -> HashSet<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 2 && !STOP.contains(w))
        .map(String::from)
        .collect()
}

fn overlap(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let inter = a.intersection(b).count() as f64;
    inter / (a.len().min(b.len()) as f64)
}

/// Small talk with nothing to work on.
pub fn is_casual(text: &str) -> bool {
    let t = text
        .trim()
        .trim_end_matches(|c: char| {
            matches!(c, '!' | '.' | '?' | ' ' | ':' | ')' | '(' | ',')
                || ('\u{1F300}'..='\u{1FAFF}').contains(&c)
        })
        .to_lowercase();
    if t.is_empty() {
        return true;
    }
    if t.chars().count() > 40 {
        return false;
    }
    CASUAL.contains(&t.as_str())
        || CASUAL
            .iter()
            .any(|c| t.starts_with(&format!("{c} ")) && t.chars().count() <= 24)
}

fn looks_like_math(text: &str) -> bool {
    // "3x + 2 = 11", "2^10", "sqrt(", "∫"
    let ops = text.matches(['=', '^', '∑', '∫', '√']).count();
    let digits = text.chars().filter(char::is_ascii_digit).count();
    (ops >= 1 && digits >= 2 && text.contains(['+', '-', '*', '/', '^'])) || text.contains("sqrt(")
}

fn looks_like_error_output(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "traceback (most recent call last)",
        "panicked at",
        "exception in thread",
        "error[e",
        "segmentation fault",
        "    at ",
    ]
    .iter()
    .any(|p| lower.contains(p))
}

fn numbered_steps(text: &str) -> usize {
    text.lines()
        .filter(|l| {
            let l = l.trim_start();
            let digits = l.chars().take_while(char::is_ascii_digit).count();
            (digits > 0 && l[digits..].starts_with(['.', ')']))
                || l.starts_with("- ")
                || l.starts_with("* ")
        })
        .count()
}

pub struct Thresholds {
    pub long_chars: usize,
    pub very_long_chars: usize,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            long_chars: 1200,
            very_long_chars: 6000,
        }
    }
}

/// Score one message in its conversation. `history` is chronological and
/// excludes the current message.
pub fn assess(text: &str, history: &[Turn], t: &Thresholds) -> Assessment {
    let lower = text.to_lowercase();
    let chars = text.chars().count();
    let mut score = 0;
    let mut signals: Vec<(&'static str, i32)> = Vec::new();
    let mut add = |name: &'static str, pts: i32, signals: &mut Vec<(&'static str, i32)>| {
        score += pts;
        signals.push((name, pts));
    };

    let last_assistant = history.iter().rev().find(|m| m.role == "assistant");
    let correction = any_word(&lower, CORRECTION_PHRASES)
        || (lower.trim_start().starts_with("no,") && chars < 400)
        || lower.trim() == "wrong";

    // Casual chat decays straight back to low.
    if is_casual(text) && !correction {
        return Assessment {
            effort: Effort::Low,
            escalate: false,
            score: 0,
            signals: vec!["casual"],
            ambiguous: false,
        };
    }

    // --- the request itself
    if chars >= t.very_long_chars {
        add("very_long_input", 4, &mut signals);
    } else if chars >= t.long_chars {
        add("long_input", 2, &mut signals);
    }
    if text.contains("```") {
        add("code_block", 3, &mut signals);
    } else if looks_like_error_output(text) {
        add("error_output", 3, &mut signals);
    }
    if any_word(&lower, CODE_WORDS) {
        add("code", 3, &mut signals);
    }
    if any_word(&lower, RESEARCH_WORDS) {
        add("research", 3, &mut signals);
    }
    if any_word(&lower, PLANNING_WORDS) {
        add("planning", 3, &mut signals);
    }
    if any_word(&lower, MATH_WORDS) || looks_like_math(text) {
        add("math", 3, &mut signals);
    }
    if any_word(&lower, DEPTH_PHRASES) {
        add("asked_for_depth", 3, &mut signals);
    }
    if numbered_steps(text) >= 3 || any_word(&lower, MULTI_STEP_PHRASES) {
        add("multi_step", 2, &mut signals);
    }
    if text.matches('?').count() >= 3 {
        add("many_questions", 1, &mut signals);
    }

    // --- the conversation
    if correction {
        add("correction", 3, &mut signals);
    }
    if last_assistant.is_some_and(|m| m.failed) {
        add("previous_failed", 2, &mut signals);
    }
    let words = content_words(text);
    let repeats = history
        .iter()
        .rev()
        .filter(|m| m.role == "user")
        .take(3)
        .filter(|m| overlap(&words, &content_words(m.content)) >= 0.5)
        .count();
    if repeats > 0 && words.len() >= 2 {
        add("same_problem", repeats.min(2) as i32, &mut signals);
    }
    // Follow-ups to a hard turn keep some of its effort; unrelated or
    // casual messages do not.
    if let Some(prev) = last_assistant.and_then(|m| m.effort)
        && prev > Effort::Low
        && (repeats > 0 || correction || chars < 200)
    {
        add(
            "carryover",
            if prev == Effort::High { 2 } else { 1 },
            &mut signals,
        );
    }

    signals.sort_by_key(|s| std::cmp::Reverse(s.1));
    let effort = if score >= HIGH_AT {
        Effort::High
    } else if score >= MEDIUM_AT {
        Effort::Medium
    } else {
        Effort::Low
    };
    Assessment {
        effort,
        escalate: score >= ESCALATE_AT,
        ambiguous: (MEDIUM_AT - 1..=HIGH_AT - 1).contains(&score),
        score,
        signals: signals.into_iter().map(|(n, _)| n).collect(),
    }
}

/// Prompt for the optional model classifier (one word answer).
pub fn model_prompt(text: &str) -> String {
    format!(
        "You route requests for a personal assistant. Rate how much reasoning the \
         following request needs. Reply with exactly one word: low (casual chat, simple \
         facts), medium (some thought, a short plan, light code), or high (multi-step \
         work, real code, research, math, careful planning).\n\n<request>\n{}\n</request>",
        crate::util::truncate_chars(text, 4000)
    )
}

/// Parse the model classifier's reply.
pub fn parse_model_reply(reply: &str) -> Option<Effort> {
    reply
        .split(|c: char| !c.is_alphabetic())
        .find_map(Effort::parse)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(text: &str) -> Assessment {
        assess(text, &[], &Thresholds::default())
    }

    fn user(c: &str) -> Turn<'_> {
        Turn {
            role: "user",
            content: c,
            failed: false,
            effort: None,
        }
    }

    fn bot(c: &str, effort: Effort, failed: bool) -> Turn<'_> {
        Turn {
            role: "assistant",
            content: c,
            failed,
            effort: Some(effort),
        }
    }

    #[test]
    fn everyday_messages_stay_low() {
        for m in [
            "hi!",
            "thanks",
            "what's a good name for a cat?",
            "remind me what time the dentist is",
            "tell me about the planet mars",
            "what is codex?",
            "good morning :)",
        ] {
            let x = a(m);
            assert_eq!(x.effort, Effort::Low, "{m}: {x:?}");
            assert!(!x.escalate, "{m}");
        }
    }

    #[test]
    fn single_signals_raise_to_medium() {
        for m in [
            "Help me debug my build",
            "Let's plan the week",
            "compare these two phone plans for me",
            "solve 3x + 2 = 11",
            "can you look at this?\n```rust\nfn main() {}\n```",
        ] {
            let x = a(m);
            assert!(x.effort >= Effort::Medium, "{m}: {x:?}");
            assert!(!x.escalate, "{m}: {x:?}");
        }
        // Whole words only.
        assert_eq!(a("we visited the planet's moons").effort, Effort::Low);
    }

    #[test]
    fn clearly_hard_requests_go_high_and_escalate() {
        let x = a(
            "Please research heat pumps vs furnaces in depth, compare costs over 10 \
                   years, and think carefully about the trade-offs. Step by step.",
        );
        assert_eq!(x.effort, Effort::High);
        assert!(x.escalate, "{x:?}");
        let code = format!(
            "Refactor this function and fix the bug:\n```python\n{}\n```",
            "x = 1\n".repeat(300)
        );
        let x = a(&code);
        assert_eq!(x.effort, Effort::High);
        assert!(x.escalate);
        assert!(x.signals.contains(&"code_block"));
        let long = "word ".repeat(1300);
        assert!(a(&long).signals.contains(&"very_long_input"));
        assert_eq!(a(&"word ".repeat(300)).signals, ["long_input"]);
    }

    #[test]
    fn stack_traces_count_as_code() {
        let x = a("Traceback (most recent call last):\n  File \"a.py\", line 1\nValueError: bad");
        assert!(x.signals.contains(&"error_output"));
        assert!(x.effort >= Effort::Medium);
    }

    #[test]
    fn corrections_and_retries_raise_effort() {
        let h = [
            user("what's the capital of australia?"),
            bot("Sydney.", Effort::Low, false),
        ];
        let x = assess("that's wrong", &h, &Thresholds::default());
        assert_eq!(x.effort, Effort::Medium, "{x:?}");
        assert!(x.signals.contains(&"correction"));
        let x = assess("no, try again and think harder", &h, &Thresholds::default());
        assert_eq!(x.effort, Effort::High, "{x:?}");
    }

    #[test]
    fn repeated_follow_ups_on_the_same_problem() {
        let h = [
            user("my sourdough bread comes out dense and flat"),
            bot("Try a longer proof.", Effort::Low, false),
            user("still dense, my sourdough bread is flat even with longer proof"),
            bot("Check your starter.", Effort::Medium, false),
        ];
        let x = assess(
            "sourdough bread still flat and dense, starter is active",
            &h,
            &Thresholds::default(),
        );
        assert!(x.signals.contains(&"same_problem"), "{x:?}");
        assert!(x.effort >= Effort::Medium, "{x:?}");
    }

    #[test]
    fn previous_failure_raises_effort() {
        let h = [
            user("summarize my notes on the trip"),
            bot("", Effort::Low, true),
        ];
        let x = assess("summarize my notes on the trip", &h, &Thresholds::default());
        assert!(x.signals.contains(&"previous_failed"));
        assert!(x.effort >= Effort::Medium, "{x:?}");
    }

    #[test]
    fn hard_follow_ups_carry_over_and_casual_decays() {
        let h = [
            user("Plan a 5 day itinerary for Japan step by step with a budget"),
            bot("Day 1 ...", Effort::High, false),
        ];
        let x = assess("make day 3 more relaxed", &h, &Thresholds::default());
        assert!(x.signals.contains(&"carryover"), "{x:?}");
        let x = assess("thanks!", &h, &Thresholds::default());
        assert_eq!(
            (x.effort, x.signals.as_slice()),
            (Effort::Low, ["casual"].as_slice())
        );
        // A long unrelated message does not inherit effort.
        let unrelated = format!(
            "Separately, {}",
            "tell me a fun fact about otters. ".repeat(8)
        );
        let x = assess(&unrelated, &h, &Thresholds::default());
        assert!(!x.signals.contains(&"carryover"), "{x:?}");
    }

    #[test]
    fn ambiguous_zone_and_model_reply() {
        assert!(a("Let's plan the week").ambiguous);
        assert!(!a("hi").ambiguous);
        assert_eq!(parse_model_reply("High."), Some(Effort::High));
        assert_eq!(parse_model_reply("  medium\n"), Some(Effort::Medium));
        assert_eq!(parse_model_reply("I think it is unclear"), None);
        assert!(model_prompt("x").contains("<request>\nx\n</request>"));
    }

    #[test]
    fn casual_detection() {
        assert!(is_casual("ok thanks!"));
        assert!(is_casual("Good morning"));
        assert!(!is_casual("ok so how do I fix the sink"));
        assert!(!is_casual(
            "thanks, now write me a cover letter for a job in Lisbon"
        ));
    }
}
