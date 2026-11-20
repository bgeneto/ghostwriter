//! Typing-speed math, kept free of platform code so it can be unit tested.
//!
//! A "word" is standardised at 5 characters (the usual WPM convention), so the
//! per-character delay is `60_000 / (wpm * 5)` milliseconds:
//! 60 WPM -> 200 ms, 80 WPM -> 150 ms, 160 WPM -> 75 ms, 300 WPM -> 40 ms.
//!
//! A perfectly constant delay reads as a machine. Real typing is bursty, so
//! spaces are shorter, punctuation adds a beat to breathe, and every delay gets
//! +-15% of noise.

use std::time::Duration;

pub const CHARS_PER_WORD: f64 = 5.0;
pub const WPM_MIN: u16 = 60;
pub const WPM_MAX: u16 = 300;
pub const WPM_DEFAULT: u16 = 60;

/// Fraction of the base delay that is randomised up or down.
const JITTER: f64 = 0.15;

/// Extra pause after a sentence-ending mark, in milliseconds.
const SENTENCE_PAUSE_MS: f64 = 180.0;
/// Extra pause after a clause separator, in milliseconds.
const CLAUSE_PAUSE_MS: f64 = 90.0;

/// Average time per character, in milliseconds.
pub fn base_delay_ms(wpm: u16) -> f64 {
    let wpm = wpm.clamp(1, 400) as f64;
    60_000.0 / (wpm * CHARS_PER_WORD)
}

/// Delay before the next character, including punctuation and jitter.
pub fn char_delay(wpm: u16, ch: char, rng: &mut Rng) -> Duration {
    let base = base_delay_ms(wpm);

    let shaped = match ch {
        // A newline is a deliberate break, not just another key.
        '\n' => base * 3.0 + SENTENCE_PAUSE_MS,
        '.' | '!' | '?' | '…' => base + SENTENCE_PAUSE_MS,
        ',' | ';' | ':' | '—' | '–' => base + CLAUSE_PAUSE_MS,
        // Spaces are the fastest keystrokes a person makes.
        ' ' => base * 0.75,
        _ => base,
    };

    // Uniformly distributed in [1 - JITTER, 1 + JITTER].
    let factor = 1.0 + (rng.next_f64() * 2.0 - 1.0) * JITTER;
    Duration::from_millis((shaped * factor).max(1.0).round() as u64)
}

/// Total duration estimate for a whole text, without jitter.
pub fn estimate(chars: usize, wpm: u16) -> Duration {
    Duration::from_millis((chars as f64 * base_delay_ms(wpm)).round() as u64)
}

/// Tiny xorshift generator: jitter does not need cryptography, and this keeps
/// the dependency list short.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        // A zero state would make xorshift emit only zeros.
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    pub fn seeded() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
            .unwrap_or(0x5DEE_CE66);
        Self::new(nanos)
    }

    /// Uniform value in `[0, 1)`.
    pub fn next_f64(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        // Keep the top 53 bits, the mantissa width of f64.
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_matches_the_reference_table() {
        assert!((base_delay_ms(60) - 200.0).abs() < 0.001);
        assert!((base_delay_ms(80) - 150.0).abs() < 0.001);
        assert!((base_delay_ms(160) - 75.0).abs() < 0.001);
    }

    #[test]
    fn speed_is_clamped_instead_of_dividing_by_zero() {
        assert!(base_delay_ms(0).is_finite());
    }

    #[test]
    fn jitter_stays_within_bounds() {
        let mut rng = Rng::new(42);
        let base = base_delay_ms(60);
        for _ in 0..2_000 {
            let ms = char_delay(60, 'a', &mut rng).as_millis() as f64;
            assert!(
                ms >= (base * (1.0 - JITTER)).floor() - 1.0,
                "too fast: {ms}"
            );
            assert!(ms <= (base * (1.0 + JITTER)).ceil() + 1.0, "too slow: {ms}");
        }
    }

    #[test]
    fn punctuation_pauses_longer_than_letters() {
        let mut rng = Rng::new(7);
        let letter = char_delay(60, 'a', &mut rng);
        let comma = char_delay(60, ',', &mut rng);
        let period = char_delay(60, '.', &mut rng);
        let newline = char_delay(60, '\n', &mut rng);
        assert!(comma > letter);
        assert!(period > comma);
        assert!(newline > period);
    }

    #[test]
    fn spaces_are_faster_than_letters() {
        let mut rng = Rng::new(11);
        assert!(char_delay(60, ' ', &mut rng) < char_delay(60, 'a', &mut rng));
    }

    #[test]
    fn never_returns_a_zero_delay() {
        let mut rng = Rng::new(3);
        assert!(char_delay(400, 'a', &mut rng) >= Duration::from_millis(1));
    }

    #[test]
    fn rng_stays_inside_the_unit_interval() {
        let mut rng = Rng::new(99);
        for _ in 0..10_000 {
            let v = rng.next_f64();
            assert!((0.0..1.0).contains(&v), "out of range: {v}");
        }
    }
}
