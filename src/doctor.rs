//! `nuthatch doctor --rpc <url>` - probe an endpoint before trusting a backfill to it.
//!
//! Issue #241 item 6: a usable endpoint needs **wide `eth_getLogs`**, **JSON-RPC batches above a
//! handful**, and **archive depth**. Each of those failed only at runtime, as a retry loop that looks
//! like slowness rather than misconfiguration, and finding out which one cost real bisection time:
//!
//! | Endpoint | Wide getLogs | Batch > 3 | Archive |
//! |---|---|---|---|
//! | `arb1.arbitrum.io/rpc` | yes | yes | yes |
//! | `arbitrum-one.public.blastapi.io` | **capped** | yes | yes |
//! | `1rpc.io/arb` | **50-block cap** | yes | yes |
//! | `arbitrum.drpc.org` | - | **free plan caps at 3** | - |
//! | `arb-pokt.nodies.app` | **403** | - | - |
//!
//! The point is not to grade providers. It is that each limit is *discoverable in seconds* and was
//! instead discovered in minutes of watching a backfill not finish. So this asks the questions
//! directly and prints the largest safe `--window`.
//!
//! **Probing costs requests.** Each run spends a couple of dozen calls - doubling `getLogs` spans,
//! eight batch sizes, one historical read. On a metered free tier that is not free, and repeated runs
//! can exhaust a small quota (measured: several probes in a row used up a 1rpc.io free allowance,
//! after which every subsequent answer was a plan error). Probe once, keep the output.
//!
//! **It probes, it does not judge.** An endpoint that fails the archive check is perfectly good for
//! tip-following; one with a narrow window is fine for a dense contract over a short range. The
//! output says what the endpoint *is*, and which nests it suits.

use anyhow::{Context, Result};
use serde_json::json;

use crate::rpc::RpcClient;
use crate::source::Source;

/// What one endpoint can actually do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    /// Largest `eth_getLogs` span that came back without a cap error, in blocks. `None` if even the
    /// smallest probe failed - the endpoint is unusable rather than merely narrow.
    pub max_window: Option<u64>,
    /// Largest JSON-RPC batch the endpoint answered in full.
    pub max_batch: Option<usize>,
    /// Whether a pinned historical read far behind the tip returned data. Decides whether RFC-0023
    /// tier 3 and deep backfills are possible at all.
    pub archive: bool,
    /// The endpoint **refused** the archive probe (plan/quota/rate limit) rather than answering it.
    /// Distinct from `archive: false`, which means it answered and had no state: one is a billing
    /// problem, the other is a capability one, and they send an operator in opposite directions.
    pub archive_unknown: bool,
    /// Notes worth printing - a 403, an auth demand, a partial batch. Each is a fact about the
    /// endpoint the operator would otherwise meet mid-backfill.
    pub notes: Vec<String>,
}

impl Probe {
    /// The `--window` to actually use: the largest that worked, with headroom.
    ///
    /// Deliberately **below** the measured ceiling. A probe measures one moment on a sparse range;
    /// the same endpoint under load, or over a denser range, refuses smaller. Recommending the exact
    /// maximum would hand the operator a value that works until it doesn't - and RFC-0028's adaptive
    /// controller can grow from a conservative start, whereas it can only recover from an aggressive
    /// one by failing first.
    pub fn recommended_window(&self) -> Option<u64> {
        self.max_window.map(|w| (w / 2).max(1))
    }

    /// One line per finding, in the order an operator cares about.
    pub fn report(&self) -> String {
        let mut out = String::new();
        match self.max_window {
            Some(w) => out.push_str(&format!(
                "  getLogs window   up to {w} blocks (recommend --window {})\n",
                self.recommended_window().unwrap_or(1)
            )),
            None => out.push_str("  getLogs window   FAILED - no probe succeeded\n"),
        }
        match self.max_batch {
            Some(n) if n >= 20 => out.push_str(&format!("  JSON-RPC batch   {n}+ (fine)\n")),
            Some(n) => out.push_str(&format!(
                "  JSON-RPC batch   {n} - narrow; timestamp fetches will split down to this\n"
            )),
            None => out.push_str("  JSON-RPC batch   FAILED - batching unusable\n"),
        }
        out.push_str(if self.archive {
            "  archive depth    yes - deep backfills and pinned eth_call work\n"
        } else if self.archive_unknown {
            "  archive depth    UNKNOWN - refused (plan/quota), not answered; re-probe with credit\n"
        } else {
            "  archive depth    no - tip-following only; a from-genesis backfill will fail\n"
        });
        for n in &self.notes {
            out.push_str(&format!("  · {n}\n"));
        }
        out
    }
}

/// Probe `url`. Never fails on a bad endpoint - a broken endpoint is the *finding*, not an error.
pub async fn probe(url: &str, address: Option<&str>) -> Result<Probe> {
    let rpc = RpcClient::new(vec![url.to_string()])
        .with_context(|| format!("'{url}' is not a usable URL"))?;
    let mut notes = Vec::new();

    let tip = match rpc.block_number().await {
        Ok(t) => t,
        Err(e) => {
            // Everything else needs a tip to probe against, so this is terminal for the probe - but
            // it is still a *result*, and the message is the useful part (403, auth, DNS).
            notes.push(format!("cannot read the chain tip: {e:#}"));
            // If the tip is unreadable, **nothing** downstream was measured - so archive is UNKNOWN,
            // not absent. Reporting "no archive" here would be asserting a capability verdict from
            // zero evidence, which is the failure this whole command exists to prevent. (Found by
            // running it: probing 1rpc.io repeatedly exhausted its free quota, after which the
            // endpoint could not answer anything and the report confidently called it non-archive.)
            return Ok(Probe {
                max_window: None,
                max_batch: None,
                archive: false,
                archive_unknown: true,
                notes,
            });
        }
    };

    // --- getLogs width -------------------------------------------------------------------------
    // Doubling from a small span, over a range just behind the tip. Probing *at* the tip risks the
    // endpoint refusing for reorg reasons rather than width, which would misreport the cap.
    //
    // A topic0 no event can produce, for the no-address case. Without it this probe sends an empty
    // address AND topic filter, which is not a width probe at all - it asks for *every log on the
    // chain* over the span (#432), so the endpoint refuses on result count and `doctor` reports that
    // as the provider's *width* cap. Filtering on a topic that matches nothing keeps the response
    // empty at every span, so the loop measures the range limit it claims to measure.
    const NO_MATCH_TOPIC0: &str =
        "0xffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff";
    let addrs: Vec<String> = address.map(|a| vec![a.to_string()]).unwrap_or_default();
    let mut max_window = None;
    let mut span = 10u64;
    while span <= 200_000 {
        let to = tip.saturating_sub(100);
        let from = to.saturating_sub(span - 1);
        // Probing the provider's window cap needs a filter that is actually asked: with no address to
        // probe with, an empty-on-both-halves filter would be the every-log-on-the-chain request
        // rather than a width probe (#432), so the probe uses a topic0 that matches nothing instead.
        let probe = crate::source::LogFilter::new(&addrs, &[]).unwrap_or_else(|| {
            crate::source::LogFilter::new(&[], &[NO_MATCH_TOPIC0.to_string()])
                .expect("a one-topic filter is non-empty")
        });
        match rpc.logs(&probe, from, to).await {
            Ok(_) => max_window = Some(span),
            Err(e) => {
                let msg = format!("{e:#}");
                if max_window.is_none() {
                    notes.push(format!("even a {span}-block getLogs failed: {msg}"));
                }
                break;
            }
        }
        span *= 2;
    }

    // --- batch size ----------------------------------------------------------------------------
    // Sizes an operator actually meets: the free-plan cap of 3 is real (issue #241), and 200 is what
    // the timestamp path uses. A partial answer counts as failure - a batch that silently returns
    // fewer results than asked is worse than one that refuses, because it seals wrong data.
    let mut max_batch = None;
    for n in [2usize, 3, 5, 10, 20, 50, 100, 200] {
        let batch: Vec<_> = (0..n)
            .map(|i| json!({"jsonrpc":"2.0","id":i,"method":"eth_blockNumber","params":[]}))
            .collect();
        match rpc.raw_batch(&serde_json::Value::Array(batch)).await {
            Ok(v) => {
                let got = v.as_array().map(Vec::len).unwrap_or(0);
                if got == n {
                    max_batch = Some(n);
                } else {
                    notes.push(format!(
                        "a batch of {n} returned {got} results - partial responses are worse than \
                         refusals, since a partial timestamp batch seals block_timestamp=0"
                    ));
                    break;
                }
            }
            Err(e) => {
                if max_batch.is_none() {
                    notes.push(format!("batching unusable at {n}: {e:#}"));
                }
                break;
            }
        }
    }

    // --- archive depth -------------------------------------------------------------------------
    // A pinned `eth_call` a long way behind the tip. `eth_getBalance` on the zero address is the
    // cheapest universally-valid historical read - no contract required, no ABI, and every node that
    // keeps state answers it.
    let deep = tip.saturating_sub(1_000_000).max(1);
    let mut archive_unknown = false;
    let archive = match rpc
        .call(
            "eth_getBalance",
            json!([
                "0x0000000000000000000000000000000000000000",
                format!("0x{deep:x}")
            ]),
        )
        .await
    {
        Ok(v) => !v.is_null(),
        Err(e) => {
            let msg = format!("{e:#}");
            // **"Refused" is not "absent".** Measured on 1rpc.io, whose archive probe fails with
            // "You've reached the usage limit for your current plan" - a billing fact, not a
            // capability one. Reporting that as "no archive" would send an operator hunting for a
            // different provider when they need a different *plan*, and this tool exists to stop
            // exactly that kind of misdirected bisection.
            let refused = [
                "usage limit",
                "upgrade",
                "plan",
                "rate limit",
                "429",
                "quota",
            ]
            .iter()
            .any(|m| msg.to_ascii_lowercase().contains(m));
            if refused {
                archive_unknown = true;
                notes.push(format!(
                    "archive depth UNKNOWN - the endpoint refused rather than answered: {msg}"
                ));
            } else {
                notes.push(format!("no state at block {deep} (~1M behind tip): {msg}"));
            }
            false
        }
    };

    // An **unfiltered** getLogs hits result-size caps far sooner than a nest's filtered one, so this
    // number understates what a real backfill can use. Measured: arb1.arbitrum.io probes at 640
    // blocks unfiltered while comfortably serving far wider windows for a single contract.
    if address.is_none() && max_window.is_some() {
        notes.push(
            "window measured UNFILTERED - a nest filters by address and topic0 and will sustain a \
             wider window; re-probe with --address <contract> for a number you can actually use"
                .to_string(),
        );
    }

    Ok(Probe {
        max_window,
        max_batch,
        archive,
        archive_unknown,
        notes,
    })
}

/// `nuthatch doctor` - probe each endpoint and print what it can do.
pub async fn run(args: crate::cli::DoctorArgs) -> Result<()> {
    let urls = if args.rpc.is_empty() {
        // No `--rpc`: probe whatever the nest in `--dir` is configured to use, which is the case
        // where "my backfill is slow" usually starts.
        let cfg =
            crate::config::Config::load(std::path::Path::new(&args.dir)).with_context(|| {
                format!(
                    "no --rpc given and no nest at '{}' to read endpoints from",
                    args.dir
                )
            })?;
        cfg.nest.rpc_urls
    } else {
        args.rpc
    };

    let mut worst_window: Option<u64> = None;
    for url in &urls {
        // Host only: providers put API keys in the path, and this output gets pasted into issues.
        let host = url
            .split("://")
            .nth(1)
            .and_then(|r| r.split('/').next())
            .unwrap_or(url);
        println!("{host}");
        let p = probe(url, args.address.as_deref()).await?;
        print!("{}", p.report());
        if let Some(w) = p.recommended_window() {
            worst_window = Some(worst_window.map_or(w, |c: u64| c.min(w)));
        }
        println!();
    }

    // The pool is only as wide as its narrowest member: failover means any request may land on any
    // endpoint, so a window that suits the best one will fail intermittently on the worst.
    if urls.len() > 1 {
        match worst_window {
            Some(w) => println!(
                "across {} endpoints, use --window {w} - failover can route any request to the \
                 narrowest of them",
                urls.len()
            ),
            None => println!("no endpoint answered - nothing to recommend"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_of(max_window: Option<u64>, max_batch: Option<usize>, archive: bool) -> Probe {
        Probe {
            max_window,
            max_batch,
            archive,
            archive_unknown: false,
            notes: Vec::new(),
        }
    }

    /// The recommendation is deliberately below the measured ceiling. A probe measures one moment on
    /// one range; the same endpoint under load refuses smaller, and the adaptive controller can grow
    /// from a conservative start but can only shrink from an aggressive one *by failing first*.
    #[test]
    fn the_recommended_window_leaves_headroom() {
        assert_eq!(
            probe_of(Some(10_000), None, false).recommended_window(),
            Some(5_000)
        );
        // Never zero: a 1-block window is useless but at least valid.
        assert_eq!(probe_of(Some(1), None, false).recommended_window(), Some(1));
        assert_eq!(probe_of(None, None, false).recommended_window(), None);
    }

    /// A narrow batch limit must be *called out*, because it is the one that silently costs the most:
    /// the timestamp path splits down to it (RFC-0029), turning one round trip into dozens.
    #[test]
    fn a_narrow_batch_is_reported_as_a_problem_not_a_number() {
        let r = probe_of(Some(1000), Some(3), true).report();
        assert!(
            r.contains("narrow"),
            "a 3-batch cap must read as a warning: {r}"
        );
        let ok = probe_of(Some(1000), Some(200), true).report();
        assert!(ok.contains("fine"), "a wide batch should not alarm: {ok}");
    }

    /// A non-archive endpoint is not a failure - it is a fact that decides which nests it suits.
    #[test]
    fn no_archive_is_reported_as_a_capability_not_an_error() {
        let r = probe_of(Some(1000), Some(50), false).report();
        assert!(r.contains("tip-following only"), "{r}");
        assert!(
            r.contains("from-genesis backfill will fail"),
            "it must say what breaks, not just what is missing: {r}"
        );
    }

    /// Every probe failing still produces a report rather than an error - a broken endpoint is the
    /// finding, and `doctor` exists precisely to be run against endpoints that do not work.
    #[test]
    fn a_completely_dead_endpoint_still_reports() {
        let r = probe_of(None, None, false).report();
        assert!(r.contains("FAILED"));
        assert!(
            r.lines().count() >= 3,
            "all three checks must be reported: {r}"
        );
    }

    /// **"Refused" is not "absent".** Measured against 1rpc.io, whose archive probe fails with
    /// *"You've reached the usage limit for your current plan"* - a billing fact, not a capability
    /// one. Reporting that as "no archive" sends an operator hunting for a different provider when
    /// they need a different plan, which is exactly the misdirected bisection `doctor` exists to end.
    #[test]
    fn a_refused_archive_probe_is_unknown_not_absent() {
        let refused = Probe {
            max_window: Some(40),
            max_batch: Some(50),
            archive: false,
            archive_unknown: true,
            notes: Vec::new(),
        };
        let r = refused.report();
        assert!(r.contains("UNKNOWN"), "{r}");
        assert!(
            !r.contains("tip-following only"),
            "a quota refusal must not be reported as a missing capability: {r}"
        );

        let genuinely_absent = Probe {
            archive_unknown: false,
            ..refused
        };
        assert!(genuinely_absent.report().contains("tip-following only"));
    }
}
