//! Deterministic "urgent" scorer. The feed never pushes ordinary posts; it
//! pushes only big, personal items: a real price drop on something the user
//! saved, or an event soon that closely matches their interests (and their
//! city, when known). Scores are plain arithmetic so they are easy to
//! inspect and test.

use super::parse::{Event, Price};

/// Push when a score reaches this.
pub const URGENT_AT: u32 = 70;
/// At most this many urgent pushes per day.
pub const MAX_PER_DAY: u32 = 2;

/// A price drop on a saved (liked) product page.
pub fn price_drop_score(old: &Price, new: &Price) -> u32 {
    if old.amount <= 0.0 || new.amount <= 0.0 {
        return 0;
    }
    if !old.currency.is_empty() && !new.currency.is_empty() && old.currency != new.currency {
        return 0;
    }
    let drop = old.amount - new.amount;
    if drop < 1.0 {
        return 0;
    }
    let pct = drop / old.amount * 100.0;
    if pct < 5.0 {
        return 0;
    }
    // 10% off on something you saved is worth a ping; deeper cuts more so.
    (50.0 + pct * 2.0).min(100.0) as u32
}

/// An event found in today's posts.
pub fn event_score(
    event: &Event,
    now_ms: i64,
    topic_affinity: f64,
    location: Option<&str>,
    from_followed_source: bool,
) -> u32 {
    let until = event.start_ms - now_ms;
    if !(0..=30 * 86_400_000).contains(&until) {
        return 0;
    }
    // Without a match on the user's city an event can never reach the
    // push threshold (at most 60): "near you" is what makes it personal.
    let mut score = 25u32;
    if until <= 7 * 86_400_000 {
        score += 15;
    } else if until <= 14 * 86_400_000 {
        score += 8;
    }
    if topic_affinity >= 0.8 {
        score += 15;
    } else if topic_affinity >= 0.5 {
        score += 8;
    }
    if let Some(city) = location.map(str::trim).filter(|c| c.len() >= 3) {
        let city = city.split(',').next().unwrap_or(city).trim().to_lowercase();
        if !city.is_empty() && event.location.to_lowercase().contains(&city) {
            score += 25;
        }
    }
    if from_followed_source {
        score += 5;
    }
    score
}

fn money(p: &Price) -> String {
    let sym = match p.currency.as_str() {
        "USD" | "" => "$",
        "EUR" => "€",
        "GBP" => "£",
        "JPY" => "¥",
        _ => "",
    };
    let amount = if p.amount.fract() == 0.0 {
        format!("{:.0}", p.amount)
    } else {
        format!("{:.2}", p.amount)
    };
    if sym.is_empty() {
        format!("{amount} {}", p.currency)
    } else {
        format!("{sym}{amount}")
    }
}

/// Push text for a price drop.
pub fn price_drop_text(title: &str, old: &Price, new: &Price) -> String {
    format!(
        "Price drop: {} is now {} (was {}).",
        crate::util::truncate_chars(title, 80),
        money(new),
        money(old)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usd(a: f64) -> Price {
        Price {
            amount: a,
            currency: "USD".into(),
        }
    }

    #[test]
    fn price_drops() {
        assert_eq!(price_drop_score(&usd(100.0), &usd(99.5)), 0, "tiny");
        assert_eq!(price_drop_score(&usd(100.0), &usd(97.0)), 0, "under 5%");
        assert!(
            price_drop_score(&usd(100.0), &usd(90.0)) >= URGENT_AT,
            "10% off"
        );
        assert!(
            price_drop_score(&usd(100.0), &usd(93.0)) < URGENT_AT,
            "7% off"
        );
        assert_eq!(price_drop_score(&usd(100.0), &usd(20.0)), 100);
        assert_eq!(
            price_drop_score(
                &usd(100.0),
                &Price {
                    amount: 50.0,
                    currency: "EUR".into()
                }
            ),
            0
        );
        assert_eq!(price_drop_score(&usd(100.0), &usd(120.0)), 0);
        assert_eq!(
            price_drop_text("Trail shoe", &usd(120.0), &usd(89.99)),
            "Price drop: Trail shoe is now $89.99 (was $120)."
        );
    }

    #[test]
    fn events() {
        let now = 1_800_000_000_000;
        let day = 86_400_000;
        let e = |days: i64, loc: &str| Event {
            start_ms: now + days * day,
            location: loc.into(),
            local_date: String::new(),
        };
        // Soon, core interest, in the user's city: push.
        assert!(
            event_score(
                &e(3, "The Hall, Springfield"),
                now,
                0.9,
                Some("Springfield, OR"),
                false
            ) >= URGENT_AT
        );
        // Soon and core interest but elsewhere: no push.
        assert!(event_score(&e(3, "Elsewhere"), now, 0.9, Some("Springfield"), true) < URGENT_AT);
        // Local but a passing interest and weeks away: no push.
        assert!(
            event_score(&e(20, "Springfield"), now, 0.3, Some("Springfield"), false) < URGENT_AT
        );
        assert_eq!(
            event_score(&e(-1, "Springfield"), now, 1.0, Some("Springfield"), true),
            0
        );
        assert_eq!(
            event_score(&e(40, "Springfield"), now, 1.0, Some("Springfield"), true),
            0
        );
    }
}
