//! List prices in USD per million tokens, mirrored from the server's `config/pricing.php` so the
//! local view can show spend without an account. Models match by the longest configured prefix;
//! unknown models fall back to the tool's default, as on the server.

struct Rates {
    input: f64,
    output: f64,
    cache_write: f64,
    cache_read: f64,
}

const fn r(input: f64, output: f64, cache_write: f64, cache_read: f64) -> Rates {
    Rates { input, output, cache_write, cache_read }
}

const MODELS: &[(&str, Rates)] = &[
    // Claude
    ("claude-fable-5-1", r(10.00, 50.00, 12.50, 0.25)),
    ("claude-mythos-5-1", r(10.00, 50.00, 12.50, 0.25)),
    ("claude-fable-5", r(10.00, 50.00, 12.50, 1.00)),
    ("claude-mythos-5", r(10.00, 50.00, 12.50, 1.00)),
    ("claude-opus-5-5", r(4.00, 20.00, 5.00, 0.20)),
    ("claude-opus-5", r(5.00, 25.00, 6.25, 0.50)),
    ("claude-opus-4-5", r(5.00, 25.00, 6.25, 0.50)),
    ("claude-opus-4-6", r(5.00, 25.00, 6.25, 0.50)),
    ("claude-opus-4-7", r(5.00, 25.00, 6.25, 0.50)),
    ("claude-opus-4-8", r(5.00, 25.00, 6.25, 0.50)),
    ("claude-opus-4", r(15.00, 75.00, 18.75, 1.50)),
    ("claude-sonnet-5", r(2.00, 10.00, 2.50, 0.20)),
    ("claude-sonnet-4", r(3.00, 15.00, 3.75, 0.30)),
    ("claude-3-7-sonnet", r(3.00, 15.00, 3.75, 0.30)),
    ("claude-haiku-4", r(1.00, 5.00, 1.25, 0.10)),
    ("claude-3-5-haiku", r(0.80, 4.00, 1.00, 0.08)),
    // Codex (OpenAI). Reasoning tokens are already included in output.
    ("gpt-5-codex-mini", r(0.25, 2.00, 0.25, 0.025)),
    ("gpt-5.1-codex-mini", r(0.25, 2.00, 0.25, 0.025)),
    ("gpt-5-mini", r(0.25, 2.00, 0.25, 0.025)),
    ("gpt-5-nano", r(0.05, 0.40, 0.05, 0.005)),
    ("gpt-5", r(1.25, 10.00, 1.25, 0.125)),
    ("o4-mini", r(1.10, 4.40, 1.10, 0.275)),
    ("o3", r(2.00, 8.00, 2.00, 0.50)),
];

fn rates_for(tool: &str, model: &str) -> &'static Rates {
    let model = model.to_lowercase();
    let best = MODELS
        .iter()
        .filter(|(prefix, _)| model.starts_with(prefix))
        .max_by_key(|(prefix, _)| prefix.len());
    let fallback = if tool == "codex" { "gpt-5" } else { "claude-sonnet-4" };
    &best.or_else(|| MODELS.iter().find(|(p, _)| *p == fallback)).expect("fallback model is configured").1
}

/// Cost in USD at list price.
pub fn cost(tool: &str, model: &str, input: u64, output: u64, cache_write: u64, cache_read: u64) -> f64 {
    let r = rates_for(tool, model);
    (input as f64 * r.input + output as f64 * r.output + cache_write as f64 * r.cache_write + cache_read as f64 * r.cache_read)
        / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn longest_prefix_wins_and_unknown_models_fall_back() {
        // Same figures as the server's UsagePushTest: 1M in + 1M out of opus 5.5 = $24.
        assert_eq!(cost("claude", "claude-opus-5-5-20260101", 1_000_000, 1_000_000, 0, 0), 24.0);
        assert_eq!(cost("codex", "gpt-5-codex-mini", 1_000_000, 0, 0, 0), 0.25);
        assert_eq!(cost("codex", "something-new", 1_000_000, 0, 0, 0), 1.25);
        assert_eq!(cost("claude", "claude-next", 1_000_000, 0, 0, 0), 3.0);
    }
}
