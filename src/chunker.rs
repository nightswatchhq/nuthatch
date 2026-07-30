//! Adaptive `eth_getLogs` range sizing (RFC-0004 §2).
//!
//! Providers cap `getLogs` by *result count* (and differ wildly: 10k here, "response size" there),
//! and a good block-window for dense USDC is a terrible one for sparse Horizon. Instead of a
//! per-chain constant that has to be hand-tuned per provider, one controller targets a response
//! budget (~2,000 logs) and adjusts the window multiplicatively from feedback: an overshoot or a
//! "result too large" error shrinks it, an undershoot grows it. The same code handles both density
//! extremes and self-heals into any provider's cap.

/// Target logs per `getLogs` response. Providers cap on result count, so this is a log (not decoded
/// event) budget - comfortably under the common 10k caps, with headroom for a growth step.
pub const TARGET_LOGS_PER_RESPONSE: u64 = 2_000;
/// Never go below one block (a single very dense block can still exceed the target - that's fine, we
/// can't split a block) or above this ceiling (keeps sparse-chain growth sane).
pub const MIN_WINDOW: u64 = 1;
pub const MAX_WINDOW: u64 = 100_000;

/// A block-window controller that converges on `target` logs per response.
#[derive(Debug, Clone)]
pub struct AdaptiveWindow {
    window: u64,
    target: u64,
    min: u64,
    max: u64,
}

impl AdaptiveWindow {
    /// Start from `initial` (typically the chain's default window), targeting `target` logs/response.
    pub fn new(initial: u64, target: u64, min: u64, max: u64) -> Self {
        Self {
            window: initial.clamp(min.max(1), max),
            target: target.max(1),
            min: min.max(1),
            max,
        }
    }

    /// A sensible controller for a chain whose default window is `initial`.
    pub fn for_window(initial: u64) -> Self {
        Self::new(initial, TARGET_LOGS_PER_RESPONSE, MIN_WINDOW, MAX_WINDOW)
    }

    /// The block span to request next.
    pub fn window(&self) -> u64 {
        self.window
    }

    /// Feed back how many logs the last response held; adjust toward the target. Change is damped to
    /// at most 4× per step so a single sparse (0-log) or spiky window doesn't swing the window wildly.
    pub fn observed(&mut self, logs: u64) {
        let next = if logs == 0 {
            // Nothing came back - grow to cover more ground, but only 4× at a time.
            self.window.saturating_mul(4)
        } else {
            // Proportional to hit the target, clamped to a 4× move either direction.
            let scaled = (self.window as u128 * self.target as u128 / logs as u128) as u64;
            scaled.clamp((self.window / 4).max(1), self.window.saturating_mul(4))
        };
        self.window = next.clamp(self.min, self.max);
    }

    /// The provider rejected the range as too large - halve hard and (the caller) retry the range.
    pub fn too_large(&mut self) {
        self.window = (self.window / 2).max(self.min);
    }
}

/// Whether an RPC error looks like a result-size / range cap (so the caller shrinks and retries the
/// same range) rather than a transient failure.
///
/// **This list is a liability, and it has already cost us.** It was written against Alchemy and Infura,
/// and `arb1.arbitrum.io` - an endpoint nuthatch *ships as an Arbitrum default* - says
/// `"logs matched by query exceeds limit of 10000"`, which matched **none** of the original markers. So
/// splitting never fired on our own zero-setup path, and a busy Arbitrum contract retried the same
/// oversized window until the operator intervened (RFC-0028 §1, measured 2026-07-28).
///
/// Widening the list fixes the providers we have met. The durable fix is the caller's speculative split
/// for failures this cannot classify (RFC-0028 §3b) - because any design that needs to have seen a
/// provider's phrasing in advance will keep failing on providers we have not met.
pub fn is_result_too_large(err: &anyhow::Error) -> bool {
    // Prefer the structured classification when the error came from `RpcClient`, which inspected the
    // HTTP status and the JSON-RPC error object rather than a flattened string (RFC-0028 §3e).
    //
    // Two independent classifiers that can disagree about the same error is worse than one: which
    // answer wins would depend on the code path, and the resulting bug would be unreproducible. So the
    // text matching below is now the *fallback* - for `Source` implementations that are not the RPC
    // client (the test tapes, the ExEx source) and raise plain `anyhow!` errors.
    if let Some(class) = crate::rpc::class_of(err) {
        match class {
            crate::rpc::FailureClass::Narrowable { .. } => return true,
            // **`Transient` does not mean "not a cap"** (RFC-0029 §3b). It is the fall-through default
            // of `classify_status` - the *absence* of a classification rather than a positive finding -
            // so it must never outrank direct textual evidence below. Returning `false` here is what
            // made the widened marker list unreachable for Alchemy's HTTP 400, and turned a splittable
            // window into five same-width retries and a dead backfill.
            crate::rpc::FailureClass::Transient => {}
            // RateLimited and Terminal *are* positive findings: back off or stop, never split.
            _ => return false,
        }
    }
    let s = format!("{err:#}").to_ascii_lowercase();
    const CAP_MARKERS: &[&str] = &[
        "response size",            // Alchemy: "Log response size exceeded"
        "too many results",         // generic
        "query returned more than", // Infura: "query returned more than 10000 results"
        "more than 10000",
        "result set too large",
        "range is too", // "block range is too wide/large"
        "range too large",
        "too large", // catch-all for "* too large"
        "limit exceeded",
        // arb1.arbitrum.io: "logs matched by query exceeds limit of 10000" - measured, and the reason
        // this list grew. Two markers because providers phrase the same cap either way round.
        "exceeds limit of",
        "exceeds max",
        "logs matched by query",
        "exceeds the limit",
    ];
    CAP_MARKERS.iter().any(|m| s.contains(m))
}

#[cfg(test)]
mod tests {

    /// **The RFC-0029 regression, at the layer that decides whether to split.**
    ///
    /// `class_of` returning `Transient` used to short-circuit to `false`, which made the cap markers
    /// below it unreachable for any error the status classifier had not recognised. `Transient` is the
    /// fall-through default - the *absence* of a finding - so it must defer to direct textual evidence
    /// rather than outrank it.
    #[test]
    fn a_transient_classification_does_not_veto_textual_cap_evidence() {
        let err = anyhow::Error::new(crate::rpc::ClassifiedError {
            class: crate::rpc::FailureClass::Transient,
            detail: "HTTP 400: {\"error\":{\"message\":\"Log response size exceeded. You can make \
                     eth_getLogs requests with up to a 10,000 block range\"}}"
                .into(),
        });
        assert!(
            is_result_too_large(&err),
            "a Transient classification carrying cap language must still split - it is the absence of \
             a finding, not a finding that this is not a cap"
        );
    }

    /// The other half of the same rule: a positive finding *does* win. Splitting on a rate limit would
    /// turn one throttled request into two, and splitting on a terminal auth failure is pointless work
    /// against an endpoint that will keep refusing.
    #[test]
    fn positive_classifications_still_veto_a_split() {
        for class in [
            crate::rpc::FailureClass::RateLimited,
            crate::rpc::FailureClass::Terminal,
        ] {
            let err = anyhow::Error::new(crate::rpc::ClassifiedError {
                class,
                // Cap language present *and* ignored, deliberately.
                detail: "response size exceeded".into(),
            });
            assert!(
                !is_result_too_large(&err),
                "a positive classification must outrank the marker list"
            );
        }
    }
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn shrinks_on_overshoot_grows_on_undershoot() {
        let mut w = AdaptiveWindow::new(100, 2_000, 1, 100_000);
        // Way over target → shrink (proportional, damped to 4× min).
        w.observed(8_000); // 100 * 2000/8000 = 25
        assert_eq!(w.window(), 25);
        // Under target → grow toward it.
        let before = w.window();
        w.observed(500); // 25 * 2000/500 = 100
        assert!(w.window() > before);
        assert_eq!(w.window(), 100);
    }

    #[test]
    fn zero_logs_grows_capped_at_4x() {
        let mut w = AdaptiveWindow::new(1_000, 2_000, 1, 100_000);
        w.observed(0);
        assert_eq!(w.window(), 4_000); // 4× growth, no division by zero
    }

    #[test]
    fn respects_bounds_and_damping() {
        let mut w = AdaptiveWindow::new(1_000, 2_000, 10, 5_000);
        w.observed(1_000_000); // huge overshoot → clamp to 4× shrink then min bound
        assert_eq!(w.window(), 250); // 1000/4, above min 10
        let mut w2 = AdaptiveWindow::new(4_000, 2_000, 1, 5_000);
        w2.observed(0); // 4× = 16000 but capped at max 5000
        assert_eq!(w2.window(), 5_000);
    }

    #[test]
    fn too_large_halves() {
        let mut w = AdaptiveWindow::new(2_000, 2_000, 1, 100_000);
        w.too_large();
        assert_eq!(w.window(), 1_000);
    }

    #[test]
    fn detects_provider_cap_errors() {
        assert!(is_result_too_large(&anyhow!(
            "Log response size exceeded. Try a smaller range"
        )));
        assert!(is_result_too_large(&anyhow!(
            "query returned more than 10000 results"
        )));
        assert!(is_result_too_large(&anyhow!("block range is too wide")));
        assert!(!is_result_too_large(&anyhow!("connection reset by peer")));
        assert!(!is_result_too_large(&anyhow!("HTTP status 521")));
    }

    /// The regression that motivated RFC-0028. These are **measured** provider responses, captured on
    /// 2026-07-28 by asking each endpoint for a populated 199k-block range of Arbitrum One native USDC
    /// - not paraphrases. A marker list tested against invented strings tests our imagination.
    ///
    /// `arb1.arbitrum.io` is one of the endpoints nuthatch ships as an Arbitrum default, and its
    /// phrasing matched nothing, so `fetch_logs_splitting` never recursed and the zero-setup path
    /// stalled on any busy contract.
    #[test]
    fn detects_the_measured_cap_errors_of_endpoints_we_ship() {
        // arb1.arbitrum.io, HTTP 200, code -32000. Matched nothing before this fix.
        assert!(
            is_result_too_large(&anyhow!("logs matched by query exceeds limit of 10000")),
            "arb1.arbitrum.io is a shipped default; its cap message must trigger splitting"
        );
        // Alchemy, HTTP 200, code -32602 (generic invalid-params - hence matching on text, not code).
        assert!(is_result_too_large(&anyhow!(
            "Log response size exceeded. You can make eth_getLogs requests with up to a 10,000 block \
             range and no limit on the response size, or you can request any block range with a cap \
             of 10K logs in the response. Based on your parameters and the response size limit, this \
             block range should work: [0x1000000, 0x1007fff]"
        )));
        // Infura's phrasing, for completeness alongside the two we measured directly.
        assert!(is_result_too_large(&anyhow!(
            "query returned more than 10000 results"
        )));
    }

    /// RFC-0028 §3e: when the RPC client has already classified an error properly - from the HTTP
    /// status and the JSON-RPC error object, not a flattened string - that answer wins.
    ///
    /// Two classifiers that can disagree about the same error is worse than one, because which answer
    /// applies would depend on the code path and the resulting bug would be unreproducible.
    #[test]
    fn the_structured_classification_wins_over_text_matching() {
        // Narrowable by structure, with wording that matches no marker at all.
        let structured = anyhow::Error::new(crate::rpc::ClassifiedError {
            class: crate::rpc::FailureClass::Narrowable { suggested: None },
            detail: "every endpoint rate-limited this request".into(),
        });
        assert!(
            is_result_too_large(&structured),
            "a structurally-narrowable error must split even when its text matches nothing"
        );

        // Terminal by structure, with wording that *would* have matched the text fallback. Splitting
        // an auth failure would be pointless work against an endpoint that will keep refusing.
        let terminal = anyhow::Error::new(crate::rpc::ClassifiedError {
            class: crate::rpc::FailureClass::Terminal,
            detail: "forbidden: response size limit exceeded for your plan".into(),
        });
        assert!(
            !is_result_too_large(&terminal),
            "structure must override the text fallback, not merely supplement it"
        );

        // Unclassified errors (test tapes, the ExEx source) still fall back to text matching.
        assert!(is_result_too_large(&anyhow!(
            "logs matched by query exceeds limit of 10000"
        )));
    }

    /// The other half of the contract: widening the list must not start swallowing failures that are
    /// nothing to do with size. Shrinking on these would trade throughput for nothing and mask real
    /// faults - including the auth rejection RFC-0028 slice 1 made terminal.
    #[test]
    fn does_not_mistake_other_failures_for_a_cap() {
        for msg in [
            "Must be authenticated!",
            "Archive requests require a personal token",
            "429 Too Many Requests",
            "connection reset by peer",
            "error decoding response body",
            "HTTP status 503",
        ] {
            assert!(
                !is_result_too_large(&anyhow!("{msg}")),
                "{msg:?} is not a size cap and must not trigger a shrink"
            );
        }
    }
}
