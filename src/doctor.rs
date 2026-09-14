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
use serde_json::{json, Value};

use crate::rpc::RpcClient;
use crate::source::Source;

/// Ceiling for a **range-only** recommendation (`max_window` measured with no `--address`).
///
/// The no-address probe filters on a topic0 no event can produce (see `probe()`), so its response
/// is empty at every span and it can never meet a result-count cap - it only ever measures the
/// provider's raw block-range ceiling. The cap is a conservative lower bound: on endpoints whose
/// real result-count limit exceeds 320 (as measured at 81,920 for one archive RPC in
/// docs/launch/port-queue-nest.md §8), the actual usable window is much larger and can only be
/// found with `--address`.
///
/// Derived from `arb1.arbitrum.io` refusing a dense all-logs probe at 640 blocks (pre-#446),
/// halved for headroom. It is one cross-endpoint data point, not a universal ceiling.
const RANGE_ONLY_WINDOW_CAP: u64 = 320;

/// Spans tried, **narrowest first**, when sampling for a contract to probe with (#1323).
///
/// The order is the safety property, not a preference. This is the one unfiltered `eth_getLogs` in
/// the binary, and nothing bounds how large a provider's answer to it may be - so the first request
/// is the smallest one that could possibly work, and the ladder widens only when the sample already
/// in hand says the next rung is affordable (Jules on #1385). Asking for the widest window first and
/// narrowing on refusal, which is what the first cut did, relies on the provider to refuse - and a
/// provider that cheerfully answers is exactly the case that hurts.
///
/// One block is a perfectly good sample where there is anything to see: measured on 2026-09-14, an
/// unfiltered single block returns 810 logs on Base and 74 on Gnosis. Widening is for chains quiet
/// enough that one block shows too little, and on those the wider request is cheap by construction.
///
/// Cost in practice: one request on a busy chain, two on a quiet one, three where there is almost
/// nothing to find.
const DISCOVERY_SPANS: [u64; 3] = [1, 5, 25];

/// The most of one unfiltered answer [`sample_addresses`] will ever hold, in bytes.
///
/// The ladder decides what to *ask* for; this decides how much of the answer may exist in this
/// process, and the two are different guarantees. A narrow first rung makes an oversized answer
/// unlikely; only this makes it impossible, and "unlikely" is not a bound (Jules on #1385).
///
/// 8 MiB is generous against every measurement taken here - the largest sample seen was Base's
/// 5,116 logs in 5 blocks, well under it - and small enough that a provider having a very strange
/// day cannot turn a diagnostic into an out-of-memory.
const DISCOVERY_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Enough sampled logs to rank contracts by activity. Past this a wider sample cannot change the
/// answer, so it is not worth the request or the memory.
const DISCOVERY_ENOUGH: usize = 200;

/// Projected log count above which the ladder will not widen, whatever the provider would allow.
///
/// Log density is roughly stable block to block, so logs-per-block times the next span is a fair
/// forecast of what the next request returns. Refusing on that forecast is the difference between
/// bounding this by evidence and bounding it by hope that the endpoint says no.
const DISCOVERY_BUDGET: usize = 20_000;

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
    /// Whether `max_window` was measured **range-only**: no `--address` was given, so the probe
    /// matched on a topic0 no event can produce and never met a result-count cap. `false` means the
    /// probe matched real logs (an address was given) and `max_window` reflects both limits.
    pub range_only: bool,
}

impl Probe {
    /// The `--window` to actually use: the largest that worked, with headroom.
    ///
    /// Deliberately **below** the measured ceiling. A probe measures one moment on a sparse range;
    /// the same endpoint under load, or over a denser range, refuses smaller. Recommending the exact
    /// maximum would hand the operator a value that works until it doesn't - and RFC-0028's adaptive
    /// controller can grow from a conservative start, whereas it can only recover from an aggressive
    /// one by failing first.
    ///
    /// For a **range-only** measurement the halved number is additionally capped at
    /// [`RANGE_ONLY_WINDOW_CAP`], because a range-only probe never meets a result-count cap and
    /// therefore says nothing about what a real nest sustains. The cap is a conservative floor -
    /// any endpoint whose real address-filtered capacity exceeds 320 will need a re-probe with
    /// `--address` to surface it.
    pub fn recommended_window(&self) -> Option<u64> {
        self.max_window.map(|w| {
            let halved = (w / 2).max(1);
            if self.range_only {
                halved.min(RANGE_ONLY_WINDOW_CAP)
            } else {
                halved
            }
        })
    }

    /// The `getLogs` finding on its own, so the re-probe in `run()` can repeat exactly this line
    /// and nothing else - and cannot drift from what [`report`] prints (#1323).
    pub fn window_line(&self) -> String {
        match self.max_window {
            Some(w) => format!(
                "  getLogs window   up to {w} blocks (recommend --window {})\n",
                self.recommended_window().unwrap_or(1)
            ),
            None => "  getLogs window   FAILED - no probe succeeded\n".to_string(),
        }
    }

    /// One line per finding, in the order an operator cares about.
    pub fn report(&self) -> String {
        let mut out = String::new();
        out.push_str(&self.window_line());
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

    /// Machine-readable form of [`report`]. Host only, never the URL: providers put keys in the
    /// path. `max_window` is `null` when getLogs failed; that is what the live-endpoints retry
    /// keys on (#716), not the `getLogs window   up to` sentence.
    pub fn to_json(&self, host: &str) -> Value {
        json!({
            "host": host,
            "max_window": self.max_window,
            "recommended_window": self.recommended_window(),
            "max_batch": self.max_batch,
            "archive": self.archive,
            "archive_unknown": self.archive_unknown,
            "notes": self.notes,
        })
    }
}

/// Pick a busy contract to run a filtered probe against, when the operator gave none (#1323).
///
/// `doctor` without an address measures a range-only `getLogs`, which never meets a result-count cap
/// and is therefore a **floor**, not a forecast. On Arc Testnet that floor recommended `--window 80`
/// where the same endpoint answered 20,000 blocks once filtered. The report already said to re-probe
/// with `--address`, and the smaller number still went into a README, because the first number is
/// the one on the screen. So `doctor` finds an address itself rather than asking.
///
/// The sample is the unfiltered firehose over [`DISCOVERY_SPAN`] blocks, issued through
/// [`RpcClient::get_logs`] rather than a [`crate::source::LogFilter`]: that type's refusal to build
/// a match-everything filter (#432) guards the *ingestion* path and should stay exactly as strict as
/// it is. This one deliberate exception is visible here and nowhere else.
///
/// Returns the address and how many of the sampled logs it emitted. `None` is an ordinary answer -
/// a quiet chain, or an endpoint that refuses the unfiltered request - and is reported as "could not
/// find one", never as a failure.
/// The emitting addresses of one unfiltered `eth_getLogs`, refusing to hold more than
/// [`DISCOVERY_MAX_BYTES`] of the answer.
///
/// **Deliberately its own transport rather than [`RpcClient::get_logs`], and deliberately not a
/// bounded variant added to that client.** Every other RPC call nuthatch makes is *filtered*, so the
/// request itself bounds the answer and no reader needs a cap; adding a bounded-read method to the
/// shared client would put a footgun beside every caller that does not need one. This is the only
/// unfiltered request in the binary, so the bound lives beside it.
///
/// The body is read chunk by chunk and abandoned the moment it passes the cap, so an oversized
/// answer is never assembled - which is the difference between bounding the request and bounding the
/// response. Only `address` is read out; discovery never needs a log's topics, data or hashes, so no
/// `Log` is constructed at all.
async fn sample_addresses(url: &str, from: u64, to: u64) -> Result<Vec<String>> {
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let body = json!({
        "jsonrpc": "2.0", "id": 1, "method": "eth_getLogs",
        "params": [{"fromBlock": format!("0x{from:x}"), "toBlock": format!("0x{to:x}")}],
    });
    let mut resp = http.post(url).json(&body).send().await?;
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await? {
        if buf.len() + chunk.len() > DISCOVERY_MAX_BYTES {
            anyhow::bail!(
                "unfiltered eth_getLogs over blocks {from}-{to} exceeded {DISCOVERY_MAX_BYTES} \
                 bytes; abandoning the sample rather than holding it"
            );
        }
        buf.extend_from_slice(&chunk);
    }
    let v: Value = serde_json::from_slice(&buf)?;
    if let Some(e) = v.get("error") {
        anyhow::bail!("rpc error: {e}");
    }
    let arr = v
        .get("result")
        .and_then(|r| r.as_array())
        .ok_or_else(|| anyhow::anyhow!("eth_getLogs did not return an array"))?;
    Ok(arr
        .iter()
        .filter_map(|l| l.get("address")?.as_str().map(str::to_string))
        .collect())
}

async fn discover_probe_address(url: &str, tip: u64) -> Option<(String, usize)> {
    // The same offset the width probe uses: at the tip an endpoint may refuse for reorg reasons
    // rather than width, and a sample that fails for the wrong reason is worse than no sample.
    let to = tip.saturating_sub(100);
    let mut sample: Vec<String> = Vec::new();
    let mut sampled_span = 0u64;
    for span in DISCOVERY_SPANS {
        if !sample.is_empty() {
            // Enough to rank by: a wider sample cannot change which contract is busiest by enough
            // to matter, so it is not worth the request.
            if sample.len() >= DISCOVERY_ENOUGH {
                break;
            }
            // Widen only on what the previous rung actually measured. `sampled_span` is non-zero
            // whenever `sample` is, so the division is safe.
            let projected = sample.len().saturating_mul(span as usize) / sampled_span as usize;
            if projected > DISCOVERY_BUDGET {
                break;
            }
        }
        let from = to.saturating_sub(span.saturating_sub(1));
        match sample_addresses(url, from, to).await {
            // An empty answer is a quiet stretch, not a refusal: widen and look again.
            Ok(got) => {
                sampled_span = span;
                if !got.is_empty() {
                    sample = got;
                }
            }
            // A refusal ends the ladder either way - and that includes the byte cap, which is
            // deliberately a refusal rather than a truncation: half a sample would rank contracts
            // by whichever happened to be serialised first, which is not a measurement of anything.
            Err(_) => break,
        }
    }
    let mut tally: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for a in &sample {
        *tally.entry(a.to_ascii_lowercase()).or_default() += 1;
    }
    // Ties broken on the address, so two runs over the same window recommend the same thing. A
    // report an operator cannot reproduce is a report they cannot check.
    tally
        .into_iter()
        .max_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)))
}

/// Probe `url`. Never fails on a bad endpoint - a broken endpoint is the *finding*, not an error.
///
/// `addresses` is the full set a real backfill would filter on - not one representative contract.
/// More addresses means more logs per block range, so the endpoint's result-count cap bites at a
/// narrower span than a single-address probe would suggest (#670). An empty slice is the range-only
/// case: no address filter at all, same as before this could take more than one.
pub async fn probe(url: &str, addresses: &[String]) -> Result<Probe> {
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
                range_only: addresses.is_empty(),
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
    let mut max_window = None;
    let mut span = 10u64;
    while span <= 200_000 {
        let to = tip.saturating_sub(100);
        let from = to.saturating_sub(span - 1);
        // Probing the provider's window cap needs a filter that is actually asked: with no address to
        // probe with, an empty-on-both-halves filter would be the every-log-on-the-chain request
        // rather than a width probe (#432), so the probe uses a topic0 that matches nothing instead.
        let probe = crate::source::LogFilter::new(addresses, &[]).unwrap_or_else(|| {
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

    // This probe filters on a topic0 no event can produce (see above), so its response is empty at
    // every span and it can never meet a result-count cap - it only ever measures the provider's
    // RANGE limit. A real nest filtered by address and topic0 additionally meets whatever
    // result-count cap the provider enforces. The 320-block recommendation is therefore a floor,
    // not a ceiling: on endpoints where the result-count cap is above 320 (as measured at 81,920
    // for one archive RPC in docs/launch/port-queue-nest.md §8), the real window is much larger.
    if addresses.is_empty() && max_window.is_some() {
        notes.push(
            "window measured RANGE-ONLY - capped at 320 because a range-only probe cannot see the \
             result-count limit; your real window is very likely much larger. Re-probe with \
             --address <contract> before backfilling to get a number that reflects both limits"
                .to_string(),
        );
    }

    Ok(Probe {
        max_window,
        max_batch,
        archive,
        archive_unknown,
        notes,
        range_only: addresses.is_empty(),
    })
}

fn catalogue_object(check: &crate::seal::CatalogueCheck) -> serde_json::Value {
    json!({
        "manifest_version": check.manifest_version,
        "segments": check.segments,
        "missing": check.missing,
        "hash_mismatch": check.hash_mismatch,
        "ok": check.ok(),
    })
}

fn catalogue_json(check: &crate::seal::CatalogueCheck) -> Result<String> {
    Ok(serde_json::to_string_pretty(&catalogue_object(check))?)
}

/// `nuthatch doctor` - probe each endpoint and print what it can do.
pub async fn run(args: crate::cli::DoctorArgs) -> Result<()> {
    let dir = std::path::Path::new(&args.dir);
    // Catalogue JSON alone is only for `--json` with no `--rpc`. An explicit `--rpc` is still
    // probed; the two results share one object so the live-endpoints gate (JSON array, no
    // `--catalogue`) is unchanged.
    if args.catalogue && args.json && args.rpc.is_empty() {
        let check = crate::seal::check_catalogue(dir)?;
        println!("{}", catalogue_json(&check)?);
        if !check.ok() {
            anyhow::bail!(
                "catalogue disagrees with the files ({} missing, {} hash mismatch)",
                check.missing.len(),
                check.hash_mismatch.len()
            );
        }
        return Ok(());
    }

    let mut catalogue_bad = false;
    let mut catalogue_check: Option<crate::seal::CatalogueCheck> = None;
    if args.catalogue {
        let check = crate::seal::check_catalogue(dir)?;
        if !args.json {
            println!(
                "catalogue  version {}  {} segment(s)",
                check.manifest_version, check.segments
            );
            for f in &check.missing {
                println!("  missing {f}");
            }
            for f in &check.hash_mismatch {
                println!("  hash mismatch {f}");
            }
            if check.ok() {
                println!("  intact");
            }
            println!();
        }
        catalogue_bad = !check.ok();
        catalogue_check = Some(check);
        if args.rpc.is_empty() && !dir.join(crate::config::CONFIG_FILE).exists() {
            if catalogue_bad {
                anyhow::bail!("catalogue disagrees with the files");
            }
            return Ok(());
        }
    }

    let mut publish_bad = false;
    if let Some(target) = &args.publish {
        println!("publish    {target}");
        match crate::publish::verify(dir, target, false, args.publish_etag_md5).await {
            Ok(n) => println!("  {n} object(s) match the local segments"),
            Err(e) => {
                println!("  FAILED {e:#}");
                publish_bad = true;
            }
        }
        println!();
        // A mirror check is a question about the bucket, not the endpoints.
        if args.rpc.is_empty() {
            if catalogue_bad {
                anyhow::bail!("catalogue disagrees with the files");
            }
            if publish_bad {
                anyhow::bail!("the mirror at {target} disagrees with this nest");
            }
            return Ok(());
        }
    }

    // Derived only when `--dir` supplies the endpoints and the operator gave no explicit
    // `--address`: the nest already declares its contracts, so there is no reason to fall back to
    // the range-only probe - which #644 measured as understating the real window by up to 256x -
    // when a real address is sitting right there in `nuthatch.toml`.
    let mut addresses: Vec<String> = args.address.clone().into_iter().collect();
    let urls = if args.rpc.is_empty() {
        // No `--rpc`: probe whatever the nest in `--dir` is configured to use, which is the case
        // where "my backfill is slow" usually starts.
        // `load_for_diagnostics`, not `load`: doctor only wants `[nest].rpc_urls`,
        // and the serving-path refusals (`[[calls]]`, tip-finality webhooks) are
        // not reasons a diagnostic cannot read them. It used to inherit both and
        // exit 1 on a nest that was present and parsed fine (#582).
        let dir = std::path::Path::new(&args.dir);
        let cfg = crate::config::Config::load_for_diagnostics(dir).with_context(|| {
            // Only claim the nest is missing when it actually is. The old message
            // said "no nest at '{dir}'" for every load failure, so an operator
            // looking straight at their nuthatch.toml was told it was not there.
            if dir.join(crate::config::CONFIG_FILE).exists() {
                format!(
                    "no --rpc given, and the nest at '{}' could not be read for its endpoints",
                    args.dir
                )
            } else {
                format!(
                    "no --rpc given and no nest at '{}' to read endpoints from",
                    args.dir
                )
            }
        })?;
        if addresses.is_empty() && !cfg.contracts.is_empty() {
            // Every declared contract, not just the first (#670): a real backfill filters logs on
            // the full set at once, so more contracts means more logs per block range and the
            // endpoint's result-count cap bites at a narrower span than a single-contract probe
            // would suggest. Probing with one contract when the nest has several would recommend a
            // window the real workload cannot sustain.
            if !args.json {
                if cfg.contracts.len() == 1 {
                    let c = &cfg.contracts[0];
                    println!(
                        "no --address given; probing with '{}' ({}), this nest's only declared \
                         contract - pass --address to probe a different one",
                        c.alias, c.address
                    );
                } else {
                    let aliases: Vec<&str> =
                        cfg.contracts.iter().map(|c| c.alias.as_str()).collect();
                    println!(
                        "no --address given; probing with all {} of this nest's declared contracts \
                         ({}) - matches what a real backfill filters on. Pass --address to probe a \
                         single one instead",
                        cfg.contracts.len(),
                        aliases.join(", ")
                    );
                }
                println!();
            }
            addresses = cfg.contracts.iter().map(|c| c.address.clone()).collect();
        }
        cfg.nest.rpc_urls
    } else {
        args.rpc
    };

    let mut worst_window: Option<u64> = None;
    let mut json_rows = Vec::new();
    for url in &urls {
        // Host only: providers put API keys in the path, and this output gets pasted into issues.
        let host = url
            .split("://")
            .nth(1)
            .and_then(|r| r.split('/').next())
            .unwrap_or(url);
        let p = probe(url, &addresses).await?;
        // Nothing given and nothing declared: the number above is the raw block-range limit, which
        // #1323 measured understating a real endpoint by 250x. Find a contract on this chain and
        // measure what a nest would actually see, rather than telling the operator to do it and
        // watching the floor get written down anyway.
        let discovered = if addresses.is_empty() {
            match RpcClient::new(vec![url.to_string()]) {
                Ok(rpc) => match rpc.block_number().await {
                    Ok(tip) => match discover_probe_address(url, tip).await {
                        Some((addr, seen)) => probe(url, std::slice::from_ref(&addr))
                            .await
                            .ok()
                            .map(|fp| (addr, seen, fp)),
                        None => None,
                    },
                    Err(_) => None,
                },
                Err(_) => None,
            }
        } else {
            None
        };

        if args.json {
            let mut row = p.to_json(host);
            if let (Some(obj), Some((addr, seen, fp))) = (row.as_object_mut(), discovered.as_ref())
            {
                // Added alongside the existing keys, never replacing them: the live-endpoints gate
                // reads `max_window` off this object (#716) and must keep seeing what it saw.
                obj.insert("discovered_address".into(), json!(addr));
                obj.insert("discovered_logs_sampled".into(), json!(seen));
                obj.insert("discovered_max_window".into(), json!(fp.max_window));
                obj.insert(
                    "discovered_recommended_window".into(),
                    json!(fp.recommended_window()),
                );
            }
            json_rows.push(row);
        } else {
            println!("{host}");
            print!("{}", p.report());
            if let Some((addr, seen, fp)) = discovered.as_ref() {
                println!();
                // Only the window line is repeated: batch size and archive depth are properties
                // of the endpoint, identical in both probes, and printing them twice buries the
                // one number that changed.
                println!("  That window is a FLOOR - with no --address it sees the block-range");
                println!(
                    "  limit and never a result-count cap. Re-probed against a real contract:"
                );
                println!("    {addr}");
                println!("    (busiest in a short unfiltered sample, {seen} of its logs)");
                println!("  {}", fp.window_line().trim_start());
                match fp.recommended_window() {
                    Some(w) => println!(
                        "  Use --window {w} for a nest of similar density, or --address <your \
                         contract>"
                    ),
                    None => println!(
                        "  Even filtered, no probe succeeded. Use --address <your contract>"
                    ),
                }
                println!("  to measure your own.");
            }
            println!();
        }
        // The filtered figure is the one a nest can act on, so it is the one that feeds the
        // across-endpoints recommendation. Where nothing was discovered, the floor still stands.
        let actionable = discovered
            .as_ref()
            .and_then(|(_, _, fp)| fp.recommended_window())
            .or_else(|| p.recommended_window());
        if let Some(w) = actionable {
            worst_window = Some(worst_window.map_or(w, |c: u64| c.min(w)));
        }
    }

    if args.json {
        // Stdout is JSON only so `jq` can be the gate. Human asides stay off this stream.
        // `--json` without `--catalogue` stays an array of endpoints; the live-endpoints
        // gate keys on that. Combined with `--catalogue`, one object so both results survive.
        if let Some(check) = catalogue_check {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "endpoints": json_rows,
                    "catalogue": catalogue_object(&check),
                }))?
            );
            if !check.ok() {
                anyhow::bail!(
                    "catalogue disagrees with the files ({} missing, {} hash mismatch)",
                    check.missing.len(),
                    check.hash_mismatch.len()
                );
            }
        } else {
            println!("{}", serde_json::to_string_pretty(&json_rows)?);
        }
        return Ok(());
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
    if catalogue_bad {
        anyhow::bail!("catalogue disagrees with the files");
    }
    if publish_bad {
        anyhow::bail!("the mirror disagrees with this nest");
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
            range_only: false,
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

    /// #716: the live-endpoints gate keys on these fields, not on `report()` prose. A failed getLogs
    /// is `max_window: null`; a healthy one is a number. Rewording the human line must not matter.
    #[test]
    fn json_probe_carries_max_window_and_archive_not_the_prose() {
        let ok = probe_of(Some(40), Some(10), true).to_json("eth.example");
        assert_eq!(ok["host"], "eth.example");
        assert_eq!(ok["max_window"], 40);
        assert_eq!(ok["archive"], true);
        assert!(ok.get("getLogs window").is_none());
        let bad = probe_of(None, None, false).to_json("dead.example");
        assert!(bad["max_window"].is_null(), "{bad}");
        assert_eq!(bad["archive"], false);
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
            range_only: false,
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

    /// A **range-only** measurement (no `--address`) never triggers a result-count cap, so a huge
    /// `max_window` only means no RANGE cap was found - not that no cap exists at all. Halving it is
    /// still an unfounded recommendation, so it is capped at `RANGE_ONLY_WINDOW_CAP` instead. Below
    /// the cap, a range-only probe behaves exactly like a filtered one (plain halving), and the same
    /// `max_window` filtered (an address was given) is never capped, because it already carries a
    /// real result-count answer.
    #[test]
    fn a_range_only_measurement_is_capped_not_just_halved() {
        let unbounded = Probe {
            range_only: true,
            ..probe_of(Some(163_840), None, false)
        };
        assert_eq!(unbounded.recommended_window(), Some(RANGE_ONLY_WINDOW_CAP));

        let small = Probe {
            range_only: true,
            ..probe_of(Some(400), None, false)
        };
        assert_eq!(small.recommended_window(), Some(200));

        let filtered = Probe {
            range_only: false,
            ..probe_of(Some(163_840), None, false)
        };
        assert_eq!(filtered.recommended_window(), Some(81_920));
    }

    /// A one-endpoint fake JSON-RPC server that captures every `eth_getLogs` filter it is sent and
    /// answers everything else just well enough for `probe()` to run to completion. `cap`, if set,
    /// refuses `eth_getLogs` on result count once the requested span exceeds it - simulating a
    /// provider whose range limit is known, rather than one that never refuses.
    async fn filter_capturing_rpc(
        seen: std::sync::Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
        cap: Option<u64>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::{extract::State, routing::post, Json, Router};
        use serde_json::Value;

        #[derive(Clone)]
        struct St {
            seen: std::sync::Arc<std::sync::Mutex<Vec<Value>>>,
            cap: Option<u64>,
        }

        fn hex_u64(v: Option<&Value>) -> Option<u64> {
            u64::from_str_radix(v?.as_str()?.trim_start_matches("0x"), 16).ok()
        }

        async fn handler(State(st): State<St>, Json(req): Json<Value>) -> Json<Value> {
            if let Some(batch) = req.as_array() {
                let out: Vec<Value> = batch
                    .iter()
                    .map(|item| {
                        json!({"jsonrpc":"2.0","id": item.get("id").cloned().unwrap_or(json!(0)), "result":"0x1"})
                    })
                    .collect();
                return Json(Value::Array(out));
            }
            let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
            match method {
                "eth_blockNumber" => Json(json!({"jsonrpc":"2.0","id":1,"result":"0x100000"})),
                "eth_getLogs" => {
                    let filter = req
                        .get("params")
                        .and_then(|p| p.as_array())
                        .and_then(|a| a.first());
                    if let Some(f) = filter {
                        st.seen.lock().unwrap().push(f.clone());
                    }
                    let span = filter.and_then(|f| {
                        Some(
                            hex_u64(f.get("toBlock"))?.saturating_sub(hex_u64(f.get("fromBlock"))?)
                                + 1,
                        )
                    });
                    if let (Some(span), Some(cap)) = (span, st.cap) {
                        if span > cap {
                            return Json(json!({
                                "jsonrpc":"2.0","id":1,
                                "error":{"code":-32000,"message":"query returned more than 10000 results"}
                            }));
                        }
                    }
                    Json(json!({"jsonrpc":"2.0","id":1,"result": []}))
                }
                _ => Json(json!({"jsonrpc":"2.0","id":1,"result":"0x0"})),
            }
        }

        let app = Router::new()
            .route("/", post(handler))
            .route("/{*rest}", post(handler))
            .with_state(St { seen, cap });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/"), handle)
    }

    // ---------------------------------------------------------------------------------------
    // #1323: finding a probe address rather than telling the operator to.
    // ---------------------------------------------------------------------------------------

    /// A provider that answers the unfiltered sample with `logs` - each entry an emitting address,
    /// repeated as many times as it emitted. Separate from [`filter_capturing_rpc`], which answers
    /// every `eth_getLogs` with `[]` and so can say nothing about which address is busiest.
    async fn log_serving_rpc(logs: &[&str]) -> (String, tokio::task::JoinHandle<()>) {
        log_serving_rpc_capped(logs, u64::MAX).await
    }

    /// As above, but refusing any unfiltered sample wider than `max_span` - which is Base, measured
    /// on 2026-09-14: it answers 5 blocks with 5,116 logs and refuses 10 as "backend response too
    /// large".
    async fn log_serving_rpc_capped(
        logs: &[&str],
        max_span: u64,
    ) -> (String, tokio::task::JoinHandle<()>) {
        log_serving_rpc_recording(logs, max_span, Default::default()).await
    }

    /// As above, and recording every **unfiltered** span asked for, in order. The order is the
    /// safety property #1385 turned on, so it has to be assertable.
    async fn log_serving_rpc_recording(
        logs: &[&str],
        max_span: u64,
        spans: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::{extract::State, routing::post, Json, Router};
        use serde_json::Value;

        let rows: Vec<Value> = logs
            .iter()
            .enumerate()
            .map(|(i, a)| {
                json!({
                    "address": a,
                    "topics": ["0x00"],
                    "data": "0x",
                    "blockNumber": format!("0x{:x}", 0x100000 - 100 - (i as u64 % 10)),
                    "blockHash": format!("0x{:064x}", 1),
                    "transactionHash": format!("0x{:064x}", i + 1),
                    "logIndex": format!("0x{i:x}"),
                })
            })
            .collect();

        #[derive(Clone)]
        struct St {
            rows: std::sync::Arc<Vec<Value>>,
            max_span: u64,
            spans: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
        }

        fn hex_u64(v: Option<&Value>) -> Option<u64> {
            u64::from_str_radix(v?.as_str()?.trim_start_matches("0x"), 16).ok()
        }

        async fn handler(State(st): State<St>, Json(req): Json<Value>) -> Json<Value> {
            if req.as_array().is_some() {
                return Json(json!([]));
            }
            match req.get("method").and_then(|m| m.as_str()).unwrap_or("") {
                "eth_blockNumber" => Json(json!({"jsonrpc":"2.0","id":1,"result":"0x100000"})),
                // Only the *unfiltered* request gets rows: an address-filtered probe is a different
                // question, and answering it from this pile would make the test agree with itself.
                "eth_getLogs" => {
                    let f = req
                        .get("params")
                        .and_then(|p| p.as_array())
                        .and_then(|a| a.first());
                    let filtered = f
                        .map(|f| f.get("address").is_some() || f.get("topics").is_some())
                        .unwrap_or(false);
                    let span = f
                        .and_then(|f| {
                            Some(
                                hex_u64(f.get("toBlock"))?
                                    .saturating_sub(hex_u64(f.get("fromBlock"))?)
                                    + 1,
                            )
                        })
                        .unwrap_or(1);
                    if !filtered {
                        st.spans.lock().unwrap().push(span);
                    }
                    // The dense-chain refusal a provider may or may not give; the ladder must be
                    // safe whether or not it does.
                    if !filtered && span > st.max_span {
                        return Json(json!({
                            "jsonrpc":"2.0","id":1,
                            "error":{"code":-32000,"message":"backend response too large"}
                        }));
                    }
                    let result = if filtered { json!([]) } else { json!(*st.rows) };
                    Json(json!({"jsonrpc":"2.0","id":1,"result": result}))
                }
                _ => Json(json!({"jsonrpc":"2.0","id":1,"result":"0x0"})),
            }
        }

        let app = Router::new()
            .route("/", post(handler))
            .route("/{*rest}", post(handler))
            .with_state(St {
                rows: std::sync::Arc::new(rows),
                max_span,
                spans,
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}/"), handle)
    }

    /// **The first unfiltered request is the narrowest one**, which is the whole of #1385's fix.
    ///
    /// This is the only unfiltered `eth_getLogs` in the binary and nothing bounds how large an
    /// answer to it may be, so the safety cannot rest on the provider refusing: a provider that
    /// cheerfully answers a wide firehose is precisely the case that hurts. The first cut asked
    /// widest-first and narrowed on refusal, which is bounded by hope. This asserts the order.
    #[tokio::test]
    async fn the_first_unfiltered_sample_is_the_narrowest() {
        let spans: std::sync::Arc<std::sync::Mutex<Vec<u64>>> = Default::default();
        let (url, handle) = log_serving_rpc_recording(
            &["0xdddddddddddddddddddddddddddddddddddddddd"; 250],
            u64::MAX,
            spans.clone(),
        )
        .await;
        let found = discover_probe_address(&url, 0x100000).await;
        handle.abort();

        let asked = spans.lock().unwrap().clone();
        assert_eq!(
            asked.first().copied(),
            Some(1),
            "the first unfiltered request asked for {asked:?} blocks - a wide one goes out before \
             anything is known about the chain's density"
        );
        // 250 logs clears DISCOVERY_ENOUGH, so a busy chain costs exactly one request and the
        // widest rung is never asked for at all.
        assert_eq!(
            asked.len(),
            1,
            "a sample that already answers the question asked again: {asked:?}"
        );
        assert_eq!(found.map(|(_, n)| n), Some(250));
    }

    /// A chain quiet enough to need a wider sample gets one - the ladder must widen, or it only
    /// ever works where one block happens to be enough.
    #[tokio::test]
    async fn a_quiet_chain_widens_until_the_sample_says_something() {
        let spans: std::sync::Arc<std::sync::Mutex<Vec<u64>>> = Default::default();
        let (url, handle) = log_serving_rpc_recording(
            &[
                "0xdddddddddddddddddddddddddddddddddddddddd",
                "0xdddddddddddddddddddddddddddddddddddddddd",
                "0xeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
            ],
            u64::MAX,
            spans.clone(),
        )
        .await;
        let found = discover_probe_address(&url, 0x100000).await;
        handle.abort();

        let asked = spans.lock().unwrap().clone();
        assert_eq!(
            asked,
            vec![1, 5, 25],
            "three logs is far under DISCOVERY_ENOUGH, so every rung should have been tried"
        );
        assert_eq!(
            found,
            Some(("0xdddddddddddddddddddddddddddddddddddddddd".to_string(), 2)),
            "widening found nothing, so a quiet chain still gets only the range-only floor"
        );
    }

    /// A provider that refuses the unfiltered request outright ends the ladder at the first rung,
    /// rather than trying four more times against an endpoint that has already said no. Measured:
    /// `ethereum-rpc.publicnode.com` answers "Please specify an address in your request".
    #[tokio::test]
    async fn a_provider_that_refuses_unfiltered_logs_is_asked_once() {
        let spans: std::sync::Arc<std::sync::Mutex<Vec<u64>>> = Default::default();
        let (url, handle) = log_serving_rpc_recording(
            &["0xdddddddddddddddddddddddddddddddddddddddd"],
            0, // refuses every span, including one block
            spans.clone(),
        )
        .await;
        let found = discover_probe_address(&url, 0x100000).await;
        handle.abort();

        assert_eq!(found, None, "a refusal is not a discovery");
        assert_eq!(
            spans.lock().unwrap().len(),
            1,
            "the ladder kept asking an endpoint that had already refused"
        );
    }

    /// The busiest contract in the sample is the one probed with - not the first seen, which is what
    /// a naive tally returns and what would make the recommendation depend on log ordering.
    #[tokio::test]
    async fn the_busiest_contract_in_the_sample_is_the_one_chosen() {
        let (url, handle) = log_serving_rpc(&[
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ])
        .await;
        let found = discover_probe_address(&url, 0x100000).await;
        handle.abort();
        assert_eq!(
            found,
            Some(("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(), 3)),
            "the first address seen was chosen over the busiest one"
        );
    }

    /// One contract written two ways is one contract. Providers are inconsistent about EIP-55
    /// checksumming, and a case-sensitive tally would split a busy address in half and hand back a
    /// quieter one - with a count that understates it, which is the figure the report prints.
    #[tokio::test]
    async fn the_same_address_in_two_casings_is_tallied_once() {
        let (url, handle) = log_serving_rpc(&[
            "0xAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        ])
        .await;
        let found = discover_probe_address(&url, 0x100000).await;
        handle.abort();
        assert_eq!(
            found,
            Some(("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(), 2)),
            "a checksummed and a lowercase spelling of one address were counted as two"
        );
    }

    /// A tie must break the same way every run. A report an operator cannot reproduce is a report
    /// they cannot check, and `HashMap` iteration order would otherwise decide it.
    #[tokio::test]
    async fn a_tie_is_broken_deterministically() {
        let mut chosen = std::collections::BTreeSet::new();
        for _ in 0..8 {
            let (url, handle) = log_serving_rpc(&[
                "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "0xcccccccccccccccccccccccccccccccccccccccc",
            ])
            .await;
            chosen.insert(discover_probe_address(&url, 0x100000).await);
            handle.abort();
        }
        assert_eq!(
            chosen.len(),
            1,
            "eight runs over an identical window chose {chosen:?}"
        );
    }

    /// **An oversized answer is refused before it is assembled**, which is the bound the ladder is
    /// not (Jules on #1385). The ladder makes a huge response unlikely by asking for one block
    /// first; only this makes it impossible, and a provider is free to answer one block with
    /// anything it likes.
    ///
    /// Refused rather than truncated, deliberately: half a sample would rank contracts by whichever
    /// happened to be serialised first, which is not a measurement of anything.
    #[tokio::test]
    async fn an_oversized_unfiltered_answer_is_refused_not_held() {
        use axum::{routing::post, Json, Router};

        // One block, and a body comfortably over the cap - the shape a ladder cannot protect
        // against, because it is already asking for the smallest window there is.
        async fn flood() -> Json<serde_json::Value> {
            let rows: Vec<serde_json::Value> = (0..60_000)
                .map(|i| {
                    json!({
                        "address": format!("0x{i:040x}"),
                        "topics": [format!("0x{:064x}", i)],
                        "data": format!("0x{}", "ab".repeat(64)),
                        "blockNumber": "0x1",
                        "blockHash": format!("0x{:064x}", 1),
                        "transactionHash": format!("0x{:064x}", i),
                        "logIndex": format!("0x{i:x}"),
                    })
                })
                .collect();
            Json(json!({"jsonrpc":"2.0","id":1,"result": rows}))
        }

        let app = Router::new()
            .route("/", post(flood))
            .route("/{*rest}", post(flood));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let url = format!("http://{addr}/");

        let err = sample_addresses(&url, 1, 1)
            .await
            .expect_err("an answer over the cap must be refused");
        handle.abort();
        assert!(
            err.to_string().contains("exceeded"),
            "refused for the wrong reason: {err:#}"
        );

        // And nothing partial survives it: the caller gets an error, not a short sample.
        assert!(
            !err.to_string().contains("truncat"),
            "the cap must refuse, not truncate: {err:#}"
        );
    }

    /// A quiet chain is an ordinary answer, not a failure: `doctor` falls back to reporting the
    /// range-only floor rather than erroring, so the command still works where there is nothing to
    /// find.
    #[tokio::test]
    async fn an_empty_window_finds_nothing_rather_than_failing() {
        let (url, handle) = log_serving_rpc(&[]).await;
        let found = discover_probe_address(&url, 0x100000).await;
        handle.abort();
        assert_eq!(found, None);
    }

    /// #432: an empty address list AND an empty topic0 list is not a width probe - it is a request
    /// for every log on the chain, which endpoints refuse on RESULT COUNT, and `doctor` misreported
    /// that refusal as WIDTH. #446 fixed it with a topic0 no event can produce. This drives the real
    /// probe loop (not a hand-built `Probe`) against a stub RPC and inspects the filter actually put
    /// on the wire, so a regression that quietly drops the no-match topic0 fails here. Mutation check:
    /// delete the `NO_MATCH_TOPIC0` fallback in `probe()` and this goes red.
    #[tokio::test]
    async fn the_no_address_probe_never_sends_the_match_everything_filter() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (url, handle) = filter_capturing_rpc(seen.clone(), None).await;

        probe(&url, &[]).await.unwrap();
        handle.abort();

        let filters = seen.lock().unwrap();
        assert!(!filters.is_empty(), "no eth_getLogs call was captured");
        assert!(
            filters
                .iter()
                .all(|f| f.get("address").is_some() || f.get("topics").is_some()),
            "a getLogs filter with neither address nor topics matches every log on the chain (#432): \
             {filters:?}"
        );
    }

    /// Drives `probe()` end to end against a fake provider whose range limit is controlled (50,000
    /// blocks - large enough that the probe's own doubling climbs well past `RANGE_ONLY_WINDOW_CAP`
    /// before the mock refuses), so a wrong headroom is visible as a wrong *recommendation*, not just
    /// a wrong constant. Mutation check: change `recommended_window()` back to plain `(w / 2).max(1)`
    /// (src/doctor.rs, `recommended_window`) and the final assertion goes red - it asserts 320, the
    /// mutated code returns half of whatever range-only ceiling the mock's cap produced instead.
    #[tokio::test]
    async fn a_range_only_recommendation_against_a_controlled_provider_is_capped() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (url, handle) = filter_capturing_rpc(seen, Some(50_000)).await;

        let p = probe(&url, &[]).await.unwrap();
        handle.abort();

        assert!(
            p.max_window.unwrap_or(0) > RANGE_ONLY_WINDOW_CAP * 2,
            "test is meaningless unless the measured ceiling clears the cap: {:?}",
            p.max_window
        );
        assert_eq!(p.recommended_window(), Some(RANGE_ONLY_WINDOW_CAP));
    }

    /// #644: the operator-facing note used to claim a range-only measurement OVERSTATES real
    /// capacity. Every measurement on record says the opposite - address-filtered ceilings came
    /// back wider, by up to 256x, and two endpoints answered only once filtered. Mutation check:
    /// revert the note text in `probe()` to the pre-#644 wording ("will sustain a narrower
    /// window") and this goes red.
    #[tokio::test]
    async fn the_range_only_note_says_the_real_window_is_likely_larger_not_smaller() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (url, handle) = filter_capturing_rpc(seen, None).await;

        let p = probe(&url, &[]).await.unwrap();
        handle.abort();

        let joined = p.notes.join(" ");
        assert!(
            joined.contains("much larger"),
            "the note must say the real window is likely LARGER, not narrower: {joined}"
        );
        assert!(
            !joined.contains("narrower window"),
            "the pre-#644 backwards claim must be gone: {joined}"
        );
    }

    /// #644 item 2 (probe with an address by default where one is derivable): `run()` with `--dir`
    /// and no `--address` must probe using the nest's own first declared contract rather than
    /// falling back to the range-only probe, which every #644 measurement showed understates the
    /// real window - sometimes by 256x, sometimes (`bsc-rpc.publicnode.com`,
    /// `polygon-bor-rpc.publicnode.com`) all the way to an outright refusal. Mutation check: delete
    /// the address-derivation block in `run()` and the captured filter carries no `address`, only
    /// the no-match `topics`.
    #[tokio::test]
    async fn run_with_dir_and_no_address_derives_one_from_the_nest() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (url, handle) = filter_capturing_rpc(seen.clone(), None).await;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(crate::config::CONFIG_FILE),
            format!(
                r#"
[nest]
name = "n"
chain = "mainnet"
chain_id = 1
rpc_urls = ["{url}"]
schema_version = 1

[[contracts]]
alias = "busiest"
address = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
abi = "abis/busiest.json"
"#
            ),
        )
        .unwrap();

        run(crate::cli::DoctorArgs {
            rpc: Vec::new(),
            dir: dir.path().to_string_lossy().into_owned(),
            address: None,
            json: false,
            catalogue: false,
            publish: None,
            publish_etag_md5: false,
        })
        .await
        .unwrap();
        handle.abort();

        let filters = seen.lock().unwrap();
        assert!(!filters.is_empty(), "no eth_getLogs call was captured");
        assert!(
            filters.iter().any(|f| {
                f.get("address")
                    .and_then(|a| a.as_array())
                    .map(|a| {
                        a.iter().any(|v| {
                            v.as_str() == Some("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
                        })
                    })
                    .unwrap_or(false)
            }),
            "no filter carried the nest's own declared contract address: {filters:?}"
        );
    }

    /// #670: a real backfill filters on **every** declared contract at once, not just the first,
    /// because more addresses means more logs per block range and the endpoint's result-count cap
    /// bites at a narrower span than a single-contract probe suggests. `run()` with `--dir` and no
    /// `--address` on a two-contract nest must therefore put *both* addresses in the probe filter.
    /// Mutation check: revert the derivation in `run()` to `cfg.contracts.first()` and the second
    /// assertion goes red - only the first contract's address is ever seen.
    #[tokio::test]
    async fn run_with_dir_and_no_address_derives_the_full_contract_set_not_just_the_first() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (url, handle) = filter_capturing_rpc(seen.clone(), None).await;

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(crate::config::CONFIG_FILE),
            format!(
                r#"
[nest]
name = "n"
chain = "mainnet"
chain_id = 1
rpc_urls = ["{url}"]
schema_version = 1

[[contracts]]
alias = "first"
address = "0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
abi = "abis/first.json"

[[contracts]]
alias = "second"
address = "0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
abi = "abis/second.json"
"#
            ),
        )
        .unwrap();

        run(crate::cli::DoctorArgs {
            rpc: Vec::new(),
            dir: dir.path().to_string_lossy().into_owned(),
            address: None,
            json: false,
            catalogue: false,
            publish: None,
            publish_etag_md5: false,
        })
        .await
        .unwrap();
        handle.abort();

        let filters = seen.lock().unwrap();
        assert!(!filters.is_empty(), "no eth_getLogs call was captured");
        let carries = |addr: &str| {
            filters.iter().any(|f| {
                f.get("address")
                    .and_then(|a| a.as_array())
                    .map(|a| a.iter().any(|v| v.as_str() == Some(addr)))
                    .unwrap_or(false)
            })
        };
        assert!(
            carries("0xaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            "the first declared contract must still be probed: {filters:?}"
        );
        assert!(
            carries("0xbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
            "the second declared contract was dropped - doctor probed only the first, which is \
             exactly #670: {filters:?}"
        );
    }

    /// `--catalogue --json --rpc` must still probe the endpoint. The catalogue-only JSON
    /// shortcut is `--json` with no `--rpc`; an explicit URL is not dropped.
    #[tokio::test]
    async fn catalogue_json_with_an_explicit_rpc_still_probes() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (url, handle) = filter_capturing_rpc(seen.clone(), None).await;
        let dir = tempfile::tempdir().unwrap();
        run(crate::cli::DoctorArgs {
            rpc: vec![url],
            dir: dir.path().to_string_lossy().into_owned(),
            address: None,
            json: true,
            catalogue: true,
            publish: None,
            publish_etag_md5: false,
        })
        .await
        .unwrap();
        handle.abort();
        assert!(
            !seen.lock().unwrap().is_empty(),
            "an explicit --rpc must be probed even when --catalogue --json is set"
        );
    }
}
