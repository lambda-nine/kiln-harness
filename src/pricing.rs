//! Model metadata and list prices for estimating eval cost.
//!
//! The rates are Anthropic's list prices from
//! <https://platform.claude.com/docs/en/about-claude/pricing>, fetched
//! 2026-11-13 ([`PRICE_TABLE_VERSION`]), in USD per million tokens. Cache
//! writes are priced at the 5-minute rate: the harness only ever asks for
//! `cache_control: {"type": "ephemeral"}`, which is the 5-minute cache.
//! Cache reads are priced as "cache hits and refreshes".
//!
//! Claude Haiku 5.5 is priced by prompt length, per request: a request whose
//! prompt is over 100,000 tokens, counting uncached input, cache reads and
//! cache writes together, pays the higher rates on every token category,
//! output included. Each request is priced on its own, so the card is chosen
//! per model response, never per run.
//!
//! Money is integers only: rates are micro-dollars per million tokens, so
//! tokens times a rate is picodollars, summed in `u128` with saturation and
//! rounded to micro-dollars only when a run's total is stored.
//!
//! These are estimates from the usage each response reported, not billing,
//! and they are never a limit: the token, byte and time budgets stay the
//! controls on a session.

use serde::{Deserialize, Serialize};

use crate::provider::Usage;

/// The date the table below was taken from the pricing page; manifests pin
/// it with the rates used.
pub const PRICE_TABLE_VERSION: &str = "2026-11-13";

/// Picodollars in one micro-dollar.
const PICO_PER_MICRO: u128 = 1_000_000;

/// USD micro-dollars per million tokens, so $0.01/MTok is `10_000`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rates {
    /// Uncached input.
    pub input: u64,
    /// Input written to the 5-minute prompt cache.
    pub cache_write: u64,
    /// Input read from the prompt cache.
    pub cache_read: u64,
    /// Output, hidden reasoning included.
    pub output: u64,
}

/// A second rate card for requests whose prompt is longer than
/// `over_tokens`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LongPrompt {
    pub over_tokens: u64,
    pub rates: Rates,
}

/// One model's limits and list prices.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ModelPricing {
    /// The exact model ID.
    pub model: &'static str,
    /// Context window in tokens.
    pub context_window: u64,
    /// Most output tokens one synchronous Messages API response may have.
    pub max_output_tokens: u32,
    /// The rates at any prompt length, or up to `long_prompt`'s threshold.
    pub standard: Rates,
    /// Higher rates for long prompts, for a model that has them.
    pub long_prompt: Option<LongPrompt>,
}

const fn per_mtok(input: u64, cache_write: u64, cache_read: u64, output: u64) -> Rates {
    Rates {
        input,
        cache_write,
        cache_read,
        output,
    }
}

/// The priced models, in the pricing page's order.
pub const MODELS: &[ModelPricing] = &[
    ModelPricing {
        model: "claude-fable-5-2",
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        standard: per_mtok(8_000_000, 10_000_000, 200_000, 40_000_000),
        long_prompt: None,
    },
    ModelPricing {
        model: "claude-opus-5-5",
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        standard: per_mtok(4_000_000, 5_000_000, 200_000, 20_000_000),
        long_prompt: None,
    },
    ModelPricing {
        model: "claude-sonnet-5-5",
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        standard: per_mtok(2_000_000, 2_500_000, 100_000, 10_000_000),
        long_prompt: None,
    },
    ModelPricing {
        model: "claude-haiku-5-5",
        context_window: 1_000_000,
        max_output_tokens: 128_000,
        standard: per_mtok(100_000, 125_000, 10_000, 500_000),
        long_prompt: Some(LongPrompt {
            over_tokens: 100_000,
            rates: per_mtok(500_000, 625_000, 50_000, 2_500_000),
        }),
    },
];

/// The table entry for `model`: an exact, case-sensitive match on the model
/// ID; `None` for anything else.
#[must_use]
pub fn lookup(model: &str) -> Option<&'static ModelPricing> {
    MODELS.iter().find(|pricing| pricing.model == model)
}

impl ModelPricing {
    /// Whether one response with this usage was billed on the long-prompt
    /// card: its prompt, every input token counted, is over the threshold.
    #[must_use]
    pub fn is_long_prompt(&self, usage: &Usage) -> bool {
        self.long_prompt
            .is_some_and(|long| usage.total_input_tokens() > long.over_tokens)
    }

    /// The rate card for one response with this usage.
    #[must_use]
    pub fn rates_for(&self, usage: &Usage) -> &Rates {
        match &self.long_prompt {
            Some(long) if self.is_long_prompt(usage) => &long.rates,
            _ => &self.standard,
        }
    }

    /// Picodollars for one response, saturating at `u128::MAX`.
    #[must_use]
    pub fn cost_picodollars(&self, usage: &Usage) -> u128 {
        let rates = self.rates_for(usage);
        // Each product fits: (2^64 - 1)^2 < 2^128.
        let part = |tokens: u64, rate: u64| u128::from(tokens) * u128::from(rate);
        part(usage.input_tokens, rates.input)
            .saturating_add(part(usage.cache_creation_input_tokens, rates.cache_write))
            .saturating_add(part(usage.cache_read_input_tokens, rates.cache_read))
            .saturating_add(part(usage.output_tokens, rates.output))
    }
}

/// Round picodollars to micro-dollars, half up, saturating at `u64::MAX`.
#[must_use]
pub fn picodollars_to_micro(pico: u128) -> u64 {
    let micro = pico.saturating_add(PICO_PER_MICRO / 2) / PICO_PER_MICRO;
    u64::try_from(micro).unwrap_or(u64::MAX)
}

/// What a manifest pins: the table version, the model, and the rates used,
/// so a report can be checked later without trusting a newer table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricingRecord {
    /// [`PRICE_TABLE_VERSION`] when the run was priced.
    pub version: String,
    pub model: String,
    pub context_window: u64,
    pub max_output_tokens: u32,
    pub standard: Rates,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub long_prompt: Option<LongPrompt>,
}

impl From<&ModelPricing> for PricingRecord {
    fn from(pricing: &ModelPricing) -> Self {
        Self {
            version: PRICE_TABLE_VERSION.to_string(),
            model: pricing.model.to_string(),
            context_window: pricing.context_window,
            max_output_tokens: pricing.max_output_tokens,
            standard: pricing.standard,
            long_prompt: pricing.long_prompt,
        }
    }
}

/// Micro-dollars as dollars: `$0.0123`, four decimals, below $1, and
/// `$1.49`, two decimals, at or above it; each rounded half up.
#[must_use]
pub fn dollars(micro: u64) -> String {
    // Hundreds of micro-dollars: the fourth decimal.
    let ten_thousandths = micro.saturating_add(50) / 100;
    if ten_thousandths < 10_000 {
        return format!("$0.{ten_thousandths:04}");
    }
    let cents = micro.saturating_add(5_000) / 10_000;
    format!("${}.{:02}", cents / 100, cents % 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: u64, cache_write: u64, cache_read: u64, output: u64) -> Usage {
        Usage {
            input_tokens: input,
            output_tokens: output,
            cache_read_input_tokens: cache_read,
            cache_creation_input_tokens: cache_write,
        }
    }

    fn haiku() -> &'static ModelPricing {
        lookup("claude-haiku-5-5").expect("haiku is priced")
    }

    #[test]
    fn each_row_has_the_pages_rates_and_limits() {
        let rows = [
            (
                "claude-fable-5-2",
                per_mtok(8_000_000, 10_000_000, 200_000, 40_000_000),
            ),
            (
                "claude-opus-5-5",
                per_mtok(4_000_000, 5_000_000, 200_000, 20_000_000),
            ),
            (
                "claude-sonnet-5-5",
                per_mtok(2_000_000, 2_500_000, 100_000, 10_000_000),
            ),
            (
                "claude-haiku-5-5",
                per_mtok(100_000, 125_000, 10_000, 500_000),
            ),
        ];
        assert_eq!(MODELS.len(), rows.len());
        for ((model, rates), pricing) in rows.iter().zip(MODELS) {
            assert_eq!(pricing.model, *model);
            assert_eq!(lookup(model), Some(pricing));
            assert_eq!(pricing.standard, *rates, "{model}");
            assert_eq!(pricing.context_window, 1_000_000, "{model}");
            assert_eq!(pricing.max_output_tokens, 128_000, "{model}");
            assert_eq!(pricing.long_prompt.is_some(), *model == "claude-haiku-5-5");
        }
        assert_eq!(
            haiku().long_prompt,
            Some(LongPrompt {
                over_tokens: 100_000,
                rates: per_mtok(500_000, 625_000, 50_000, 2_500_000),
            })
        );
        // $0.01/MTok is ten thousand micro-dollars per million tokens.
        assert_eq!(haiku().standard.cache_read, 10_000);
    }

    #[test]
    fn haiku_switches_cards_only_over_100000_prompt_tokens() {
        let haiku = haiku();
        let long = haiku.long_prompt.expect("long card").rates;
        for at_limit in [
            usage(100_000, 0, 0, 10),
            usage(1, 2, 99_997, 10),
            usage(0, 100_000, 0, 10),
        ] {
            assert!(!haiku.is_long_prompt(&at_limit), "{at_limit:?}");
            assert_eq!(haiku.rates_for(&at_limit), &haiku.standard);
        }
        // Output never counts toward the prompt.
        assert!(!haiku.is_long_prompt(&usage(100_000, 0, 0, 1_000_000)));
        for over in [
            usage(100_001, 0, 0, 10),
            // Mostly cache reads: a cache hit still counts.
            usage(1, 0, 100_000, 10),
            usage(0, 50_001, 50_000, 10),
        ] {
            assert!(haiku.is_long_prompt(&over), "{over:?}");
            assert_eq!(haiku.rates_for(&over), &long);
        }
        // The other models have one card at any length.
        let opus = lookup("claude-opus-5-5").expect("opus");
        let huge = usage(900_000, 0, 0, 0);
        assert!(!opus.is_long_prompt(&huge));
        assert_eq!(opus.rates_for(&huge), &opus.standard);
    }

    #[test]
    fn a_mixed_usage_is_priced_by_hand() {
        // Sonnet 5.5: 1,000 × $2 + 2,000 × $2.50 + 30,000 × $0.10 + 500 × $10
        // per million = $0.002 + $0.005 + $0.003 + $0.005 = $0.015.
        let sonnet = lookup("claude-sonnet-5-5").expect("sonnet");
        let mixed = usage(1_000, 2_000, 30_000, 500);
        assert_eq!(sonnet.cost_picodollars(&mixed), 15_000_000_000);
        assert_eq!(
            picodollars_to_micro(sonnet.cost_picodollars(&mixed)),
            15_000
        );
        // Haiku 5.5 over the threshold pays five times on every category:
        // 1,000 × $0.50 + 2,000 × $0.625 + 100,000 × $0.05 + 500 × $2.50
        // per million = 500 + 1,250 + 5,000 + 1,250 micro-dollars.
        let long = usage(1_000, 2_000, 100_000, 500);
        assert_eq!(haiku().cost_picodollars(&long), 8_000_000_000);
        // At exactly 100,000 prompt tokens the standard card applies.
        let short = usage(1_000, 2_000, 97_000, 500);
        assert_eq!(
            haiku().cost_picodollars(&short),
            100_000_000 + 250_000_000 + 970_000_000 + 250_000_000
        );
    }

    #[test]
    fn huge_counts_saturate() {
        // At list rates every count at its limit is still exact in u128,
        // and rounds to micro-dollars saturated at u64::MAX.
        let fable = lookup("claude-fable-5-2").expect("fable");
        let max = usage(u64::MAX, u64::MAX, u64::MAX, u64::MAX);
        let per_token = 8_000_000u128 + 10_000_000 + 200_000 + 40_000_000;
        assert_eq!(
            fable.cost_picodollars(&max),
            u128::from(u64::MAX) * per_token
        );
        assert_eq!(picodollars_to_micro(fable.cost_picodollars(&max)), u64::MAX);
        // Rates at their limit too: the sum saturates instead of wrapping.
        let extreme = ModelPricing {
            standard: per_mtok(u64::MAX, u64::MAX, u64::MAX, u64::MAX),
            long_prompt: None,
            ..*fable
        };
        assert_eq!(extreme.cost_picodollars(&max), u128::MAX);
        assert_eq!(picodollars_to_micro(u128::MAX), u64::MAX);
        // The usage sum saturates the prompt length, which is then long.
        assert!(haiku().is_long_prompt(&max));
    }

    #[test]
    fn picodollars_round_half_up_to_micro_dollars() {
        assert_eq!(picodollars_to_micro(0), 0);
        assert_eq!(picodollars_to_micro(499_999), 0);
        assert_eq!(picodollars_to_micro(500_000), 1);
        assert_eq!(picodollars_to_micro(1_499_999), 1);
        assert_eq!(picodollars_to_micro(1_500_000), 2);
    }

    #[test]
    fn lookup_is_exact() {
        for miss in [
            "claude-haiku-4-5",
            "Claude-Haiku-5-5",
            "",
            "claude-haiku-5-5 ",
            "claude-haiku-5",
        ] {
            assert_eq!(lookup(miss), None, "{miss:?}");
        }
    }

    #[test]
    fn dollars_use_four_decimals_below_one_and_two_above() {
        assert_eq!(dollars(0), "$0.0000");
        assert_eq!(dollars(49), "$0.0000");
        assert_eq!(dollars(50), "$0.0001");
        assert_eq!(dollars(12_345), "$0.0123");
        assert_eq!(dollars(12_350), "$0.0124");
        assert_eq!(dollars(999_949), "$0.9999");
        // Rounds to a dollar: shown with two decimals.
        assert_eq!(dollars(999_950), "$1.00");
        assert_eq!(dollars(1_000_000), "$1.00");
        assert_eq!(dollars(1_494_999), "$1.49");
        assert_eq!(dollars(1_495_000), "$1.50");
        assert_eq!(dollars(123_456_789), "$123.46");
        assert_eq!(dollars(u64::MAX), "$18446744073709.55");
    }

    #[test]
    fn the_record_pins_the_version_and_rates() {
        let record = PricingRecord::from(haiku());
        assert_eq!(record.version, PRICE_TABLE_VERSION);
        assert_eq!(record.model, "claude-haiku-5-5");
        assert_eq!(record.standard, haiku().standard);
        assert_eq!(record.long_prompt, haiku().long_prompt);
        let json = serde_json::to_value(&record).expect("record JSON");
        assert_eq!(json["version"], "2026-11-13");
        assert_eq!(json["long_prompt"]["over_tokens"], 100_000);
        assert_eq!(json["standard"]["cache_read"], 10_000);
        let back: PricingRecord = serde_json::from_value(json).expect("round-trips");
        assert_eq!(back, record);
        // One card: no `long_prompt` key.
        let opus = PricingRecord::from(lookup("claude-opus-5-5").expect("opus"));
        let json = serde_json::to_value(&opus).expect("record JSON");
        assert!(json.get("long_prompt").is_none(), "{json}");
    }
}
