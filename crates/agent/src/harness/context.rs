use serde::{Deserialize, Serialize};

/// Rough token estimate used for text that has not been measured by a provider.
///
/// This deliberately matches the cheap estimate used by the v2 compaction
/// planner: four characters per token, rounded up. It is only a fallback until
/// the next provider response gives us exact usage numbers.
pub const CHARS_PER_TOKEN: u64 = 4;

pub const DEFAULT_RESERVE_FRACTION: f64 = 0.10;
pub const DEFAULT_RESERVE_MIN_TOKENS: u64 = 16_000;

/// Exact usage reported by a provider for one request.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasuredUsage {
    pub input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub output: u64,
    pub reasoning: u64,
}

impl MeasuredUsage {
    pub fn total(self) -> u64 {
        self.input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
            .saturating_add(self.output)
            .saturating_add(self.reasoning)
    }
}

/// The identity of the request that produced a measured usage snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextAnchor {
    pub provider: String,
    pub model: String,
    pub variant: Option<String>,
    pub usage: MeasuredUsage,
}

/// Measured and estimated context sizes for a thread.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextEstimate {
    pub measured: u64,
    pub estimated: u64,
}

impl ContextEstimate {
    pub fn total(self) -> u64 {
        self.measured.saturating_add(self.estimated)
    }

    pub fn is_empty(self) -> bool {
        self.total() == 0
    }
}

/// How much room to leave below the model's input window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContextReserve {
    pub fraction: f64,
    pub minimum_tokens: u64,
}

impl Default for ContextReserve {
    fn default() -> Self {
        Self {
            fraction: DEFAULT_RESERVE_FRACTION,
            minimum_tokens: DEFAULT_RESERVE_MIN_TOKENS,
        }
    }
}

/// Tracks the last provider-measured prompt plus a character estimate for
/// everything appended after that request.
///
/// A measured usage snapshot covers the prompt up to and including the response
/// that reported it. The caller must add any messages appended after that
/// response with [`ContextLedger::add_estimated_text`] or
/// [`ContextLedger::add_estimated_tokens`]. Recording a new measurement resets
/// the estimate because the new measurement includes the previous tail.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextLedger {
    anchor: Option<ContextAnchor>,
    estimated_since_anchor: u64,
}

impl ContextLedger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn anchor(&self) -> Option<&ContextAnchor> {
        self.anchor.as_ref()
    }

    /// Records exact provider usage for the request identified by
    /// `provider`/`model`/`variant`.
    pub fn record_measured(
        &mut self,
        provider: impl Into<String>,
        model: impl Into<String>,
        variant: Option<String>,
        usage: MeasuredUsage,
    ) {
        self.anchor = Some(ContextAnchor {
            provider: provider.into(),
            model: model.into(),
            variant,
            usage,
        });
        self.estimated_since_anchor = 0;
    }

    /// Records exact provider usage without changing the anchor identity.
    ///
    /// This is useful when a provider reports usage for a follow-up request
    /// against the same model, but the caller wants to keep an existing anchor
    /// label. Prefer [`ContextLedger::record_measured`] when the identity is
    /// available.
    pub fn record_measured_usage(&mut self, usage: MeasuredUsage) {
        if let Some(anchor) = &mut self.anchor {
            anchor.usage = usage;
            self.estimated_since_anchor = 0;
        } else {
            self.anchor = Some(ContextAnchor {
                provider: String::new(),
                model: String::new(),
                variant: None,
                usage,
            });
            self.estimated_since_anchor = 0;
        }
    }

    pub fn clear_measurement(&mut self) {
        self.anchor = None;
        self.estimated_since_anchor = 0;
    }

    /// Adds a cheap character-based estimate for text appended after the last
    /// measured request.
    pub fn add_estimated_text(&mut self, text: &str) -> u64 {
        let tokens = estimate_text_tokens(text);
        self.add_estimated_tokens(tokens);
        tokens
    }

    pub fn add_estimated_tokens(&mut self, tokens: u64) {
        self.estimated_since_anchor = self.estimated_since_anchor.saturating_add(tokens);
    }

    pub fn estimate(&self) -> ContextEstimate {
        ContextEstimate {
            measured: self
                .anchor
                .as_ref()
                .map_or(0, |anchor| anchor.usage.total()),
            estimated: self.estimated_since_anchor,
        }
    }

    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Returns the largest prompt size that leaves room for the configured
    /// reserve, or `None` when the model's input window is unknown.
    pub fn ceiling(&self, input_window: u64, reserve: ContextReserve) -> Option<u64> {
        usable_context_ceiling(input_window, reserve)
    }

    /// Returns whether the current estimate has reached the configured fraction
    /// of the usable context ceiling.
    pub fn should_compact(
        &self,
        input_window: u64,
        threshold: f64,
        reserve: ContextReserve,
    ) -> bool {
        let Some(ceiling) = self.ceiling(input_window, reserve) else {
            return false;
        };
        if !threshold.is_finite() {
            return false;
        }
        let threshold = threshold.clamp(0.0, 1.0);
        self.estimate().total() as f64 >= ceiling as f64 * threshold
    }
}

/// Computes the usable prompt ceiling for an input window.
pub fn usable_context_ceiling(input_window: u64, reserve: ContextReserve) -> Option<u64> {
    if input_window == 0 {
        return None;
    }
    let fraction = if reserve.fraction.is_finite() {
        reserve.fraction.clamp(0.0, 1.0)
    } else {
        DEFAULT_RESERVE_FRACTION
    };
    let reserve_tokens = ((input_window as f64) * fraction).ceil() as u64;
    let reserve_tokens = reserve_tokens.max(reserve.minimum_tokens);
    Some(input_window.saturating_sub(reserve_tokens))
}

/// Estimates tokens for text using [`CHARS_PER_TOKEN`], rounding up.
pub fn estimate_text_tokens(text: &str) -> u64 {
    let chars = text.chars().count() as u64;
    if chars == 0 {
        0
    } else {
        chars.saturating_add(CHARS_PER_TOKEN - 1) / CHARS_PER_TOKEN
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_estimate_is_empty_for_empty_text() {
        assert_eq!(estimate_text_tokens(""), 0);
    }

    #[test]
    fn text_estimate_rounds_up_and_counts_characters() {
        assert_eq!(estimate_text_tokens("a"), 1);
        assert_eq!(estimate_text_tokens("abcd"), 1);
        assert_eq!(estimate_text_tokens("abcde"), 2);
        // Unicode characters are counted individually, not as UTF-8 bytes.
        assert_eq!(estimate_text_tokens("Привет"), 2);
    }

    #[test]
    fn measured_usage_saturates() {
        let usage = MeasuredUsage {
            input: u64::MAX,
            cache_read: 1,
            ..Default::default()
        };
        assert_eq!(usage.total(), u64::MAX);
    }

    #[test]
    fn ledger_combines_measured_and_estimated_usage() {
        let mut ledger = ContextLedger::new();
        ledger.record_measured(
            "openai",
            "gpt-5",
            None,
            MeasuredUsage {
                input: 1_000,
                cache_read: 2_000,
                cache_write: 500,
                output: 300,
                reasoning: 200,
            },
        );
        ledger.add_estimated_text("abcd");
        ledger.add_estimated_tokens(3);

        assert_eq!(
            ledger.estimate(),
            ContextEstimate {
                measured: 4_000,
                estimated: 4,
            }
        );
        assert_eq!(
            ledger.anchor().map(|anchor| anchor.model.as_str()),
            Some("gpt-5")
        );
    }

    #[test]
    fn recording_new_measurement_resets_the_estimate() {
        let mut ledger = ContextLedger::new();
        ledger.add_estimated_tokens(100);
        ledger.record_measured(
            "openai",
            "gpt-5",
            Some("high".to_string()),
            MeasuredUsage {
                input: 5_000,
                ..Default::default()
            },
        );

        assert_eq!(
            ledger.estimate(),
            ContextEstimate {
                measured: 5_000,
                estimated: 0,
            }
        );
        assert_eq!(
            ledger.anchor().and_then(|anchor| anchor.variant.as_deref()),
            Some("high")
        );
    }

    #[test]
    fn ledger_ignores_estimates_after_clear() {
        let mut ledger = ContextLedger::new();
        ledger.record_measured(
            "openai",
            "gpt-5",
            None,
            MeasuredUsage {
                input: 100,
                ..Default::default()
            },
        );
        ledger.add_estimated_tokens(50);
        ledger.clear_measurement();
        ledger.add_estimated_tokens(10);

        assert_eq!(
            ledger.estimate(),
            ContextEstimate {
                measured: 0,
                estimated: 10,
            }
        );
    }

    #[test]
    fn ceiling_subtracts_the_larger_of_fraction_or_minimum() {
        let reserve = ContextReserve {
            fraction: 0.10,
            minimum_tokens: 16_000,
        };
        assert_eq!(usable_context_ceiling(200_000, reserve), Some(180_000));
        assert_eq!(usable_context_ceiling(100_000, reserve), Some(84_000));
        assert_eq!(
            usable_context_ceiling(
                100_000,
                ContextReserve {
                    fraction: 0.25,
                    minimum_tokens: 1_000,
                }
            ),
            Some(75_000)
        );
    }

    #[test]
    fn ceiling_is_unknown_for_an_unknown_window() {
        assert_eq!(usable_context_ceiling(0, ContextReserve::default()), None);
    }

    #[test]
    fn should_compact_at_the_threshold() {
        let mut ledger = ContextLedger::new();
        ledger.record_measured(
            "openai",
            "gpt-5",
            None,
            MeasuredUsage {
                input: 80_000,
                ..Default::default()
            },
        );
        let reserve = ContextReserve {
            fraction: 0.10,
            minimum_tokens: 16_000,
        };
        // 100k window, 16k reserve -> 84k ceiling; 80k is above 0.9 * 84k.
        assert!(ledger.should_compact(100_000, 0.9, reserve));
        // 80k is below 0.96 * 84k.
        assert!(!ledger.should_compact(100_000, 0.96, reserve));
    }

    #[test]
    fn should_compact_is_false_for_an_unknown_window() {
        let ledger = ContextLedger::new();
        assert!(!ledger.should_compact(0, 0.9, ContextReserve::default()));
    }
}
