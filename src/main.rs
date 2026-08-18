//! nuthatch - be your own indexer.
//!
//! Turn any contract into a local SQL database:
//!   `nuthatch init 0xADDR`                  -> detect the chain, resolve ABI (Sourcify -> Etherscan), scaffold a nest
//!   `nuthatch dev`                          -> backfill + follow the tip -> decode -> serve an API
//!   `nuthatch sql "SELECT …"`               -> query the live tip + sealed history, as a table
//!
//! Generalised event decode over many contracts, content-addressed Parquet sealing past finality with
//! DuckDB analytics (hot ∪ cold SQL), DBSP incremental views, factories, a compliance pack, webhooks,
//! a built-in admin UI, an MCP server, and multi-nest roosts - all from one static binary. This file is
//! just the CLI front door; the engine lives in the library crate.

use nuthatch::{
    analytics, audit, bench, blob, check, cli, config, distribution, doctor, indexer, labels,
    lists, mcp, pack, project, runtime, screen, store, transform,
};

use anyhow::{Context, Result};
use clap::Parser;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = cli::Cli::parse();

    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "nuthatch=info".into())
    };
    match cli.log_format {
        // Text stays exactly as it was: no target (the crate is the only binary running), env-filter
        // default `nuthatch=info`.
        cli::LogFormat::Text => tracing_subscriber::fmt()
            .with_env_filter(filter())
            .with_target(false)
            .init(),
        // One JSON object per line for a log aggregator. Every `tracing::info!(block = .., tip = ..,
        // "…")` field (the at-tip heartbeat, RFC #302) lands as its own JSON key instead of being
        // interpolated into a message string, so it's queryable without a text parser.
        cli::LogFormat::Json => tracing_subscriber::fmt()
            .json()
            .with_env_filter(filter())
            .init(),
    }

    match cli.command {
        cli::Command::Init(args) => project::init(args).await,
        cli::Command::Add(args) => project::add(args).await,
        // **One command for 1..N nests** (RFC-0032). The runtime hosts a single nest or many; which
        // one you get is a property of the directory, not a decision an operator has to make before
        // they know which they want. A `mounts.toml` means a multi-nest runtime, a `nuthatch.toml`
        // means one nest, and the pre-2.0 `mounts dev` split is gone.
        cli::Command::Dev(args) => {
            let dir = std::path::PathBuf::from(&args.dir);
            if dir.join(nuthatch::runtime::MOUNTS_FILE).exists()
                || dir.join(nuthatch::runtime::LEGACY_ROOST_FILE).exists()
            {
                runtime::dev(
                    dir,
                    args.listen,
                    args.rpc,
                    args.backfill,
                    args.seal_direct,
                    args.concurrency,
                    args.window,
                    args.no_admin,
                    args.fail_fast,
                )
                .await
            } else {
                indexer::dev(args).await
            }
        }
        cli::Command::Serve(args) => indexer::serve_role(args).await,
        #[cfg(feature = "postgres-store")]
        cli::Command::Worker(args) => {
            let id = match args.id {
                Some(id) => id,
                None => hostname_or_bail()?,
            };
            let cp = nuthatch::controlplane::ControlPlane::connect(&args.control_db)?;
            let hosts = nuthatch::worker::Hosts::from_chains(&args.hot_store, &args.chains)?;
            nuthatch::worker::run(
                cp,
                hosts,
                &id,
                args.budget_mb,
                !args.no_secrets,
                nuthatch::worker::NestPaths {
                    root: std::path::PathBuf::from(&args.nest_root),
                    cache: std::path::PathBuf::from(&args.nest_cache),
                    registry: args.registry.clone(),
                },
            )
            .await
        }
        #[cfg(not(feature = "postgres-store"))]
        cli::Command::Worker(_) => anyhow::bail!(
            "the writer-worker role needs a build with `--features postgres-store`. The default \
             binary is the embedded one and carries no database driver (CLAUDE.md non-negotiable 1)."
        ),
        #[cfg(feature = "postgres-store")]
        cli::Command::Control(args) => nuthatch::control_api::run(&args.listen, &args.db).await,
        #[cfg(not(feature = "postgres-store"))]
        cli::Command::Control(_) => anyhow::bail!(
            "the control plane needs a build with `--features postgres-store`. The default binary \
             is the embedded one and carries no database driver (CLAUDE.md non-negotiable 1)."
        ),
        cli::Command::Sql(args) => run_sql(args).await,
        cli::Command::Transform(args) => run_transform(args),
        cli::Command::Mcp(args) => {
            if args.print_config {
                mcp::print_client_config(&args.url);
                Ok(())
            } else {
                mcp::serve(args.url).await
            }
        }
        cli::Command::Check(args) => check::check(args),
        cli::Command::Schema(args) => project::regen(args),
        cli::Command::Bench(args) => match args.what {
            cli::BenchWhat::Backfill(a) => bench::backfill(a).await,
            cli::BenchWhat::Query(a) => bench::query(a),
        },
        cli::Command::Doctor(args) => doctor::run(args).await,
        cli::Command::Labels(args) => run_labels(args),
        cli::Command::Lists(args) => run_lists(args).await,
        cli::Command::Screen(args) => screen::backfill(args),
        cli::Command::Pack(args) => pack::run(args, &now_stamp()),
        cli::Command::Audit(args) => audit::run(args),
        cli::Command::Nest(args) => match args.what {
            cli::NestWhat::Bundle(a) => blob::bundle(
                std::path::Path::new(&a.dir),
                a.out.as_deref().map(std::path::Path::new),
                a.as_dir,
            ),
            cli::NestWhat::Load(a) => match a.registry.as_deref() {
                Some(registry) => {
                    distribution::load_from_registry(
                        registry,
                        &a.bundle,
                        a.dir.as_deref().map(std::path::Path::new),
                    )
                    .await
                }
                None => {
                    blob::load(
                        &a.bundle,
                        a.dir.as_deref().map(std::path::Path::new),
                        a.expect.as_deref(),
                    )
                    .await
                }
            },
            cli::NestWhat::Publish(a) => {
                distribution::publish_cli(
                    &a.registry,
                    std::path::Path::new(&a.bundle),
                    a.as_ref.as_deref(),
                )
                .await
            }
        },
        cli::Command::SkillRefs => {
            nuthatch::skill::write_refs(std::path::Path::new("."))?;
            println!(
                "✓ regenerated {}/cli-reference.md",
                nuthatch::skill::SKILL_DIR
            );
            Ok(())
        }
        cli::Command::Migrate(a) => nuthatch::migrate::run(std::path::Path::new(&a.dir), a.dry_run, a.allow_breaking),
        cli::Command::Prune(a) => {
            nuthatch::prune::run(std::path::Path::new(&a.dir), a.yes)
        }
        cli::Command::Recipe(args) => match args.what {
            cli::RecipeWhat::List => {
                nuthatch::recipes::list_cli();
                Ok(())
            }
            cli::RecipeWhat::Add(a) => nuthatch::recipes::add_cli(
                std::path::Path::new(&a.dir),
                &a.name,
                a.alias.as_deref(),
            ),
        },
        cli::Command::Metadata(args) => match args.what {
            cli::MetadataWhat::Fetch(a) => {
                nuthatch::metadata::fetch_cli(std::path::Path::new(&a.dir), a.rpc).await
            }
        },
    }
}

/// A coarse wall-clock stamp for the `created` field of a manifest (provenance, not a correctness path).
fn now_stamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("unix:{secs}")
}

/// `nuthatch lists …` - manage sanctions/watch lists as content-addressed snapshots (RFC-0008 C2).
async fn run_lists(args: cli::ListsArgs) -> Result<()> {
    use std::path::{Path, PathBuf};
    match args.what {
        cli::ListsWhat::Fetch(a) => {
            let dir = PathBuf::from(&a.dir);
            let (hash, count) = lists::fetch(
                &dir,
                &a.list,
                a.url.as_deref(),
                a.file.as_deref().map(Path::new),
            )
            .await?;
            println!(
                "✓ fetched {count} sanctioned address(es) → lists/{}.json",
                &hash[..16]
            );
            println!(
                "  screen a range with:  nuthatch screen --list {hash} --from <block> --to <block>"
            );
            Ok(())
        }
        cli::ListsWhat::List(a) => {
            let dir = PathBuf::from(&a.dir);
            for (hash, count) in lists::snapshots(&dir) {
                println!("{hash}  {count} address(es)");
            }
            Ok(())
        }
    }
}

/// `nuthatch labels …` - manage the compliance annotation substrate (RFC-0008 C1).
fn run_labels(args: cli::LabelsArgs) -> Result<()> {
    use std::path::{Path, PathBuf};
    match args.what {
        cli::LabelsWhat::Import(a) => {
            let dir = PathBuf::from(&a.dir);
            let (hash, count) = labels::import(&dir, Path::new(&a.file))?;
            println!(
                "✓ imported {count} labeled address(es) → labels/{}.json",
                &hash[..16]
            );
            println!("  (content-addressed: re-importing the same set is idempotent)");
            Ok(())
        }
        cli::LabelsWhat::List(a) => {
            let dir = PathBuf::from(&a.dir);
            let set = labels::load(&dir);
            println!(
                "{} labeled address(es) loaded from {}/labels/",
                set.len(),
                a.dir
            );
            Ok(())
        }
    }
}

/// `nuthatch sql [query]` - read-only SQL over the nest's data (live tip ∪ sealed history). With a
/// query, one-shot to a table (`--json` to pipe). Without, an interactive REPL. The terminal-native
/// front door to querying, so a user never needs curl to poke at their own data (RFC-0015).
async fn run_sql(args: cli::SqlArgs) -> Result<()> {
    let backend = SqlBackend::open(&args.dir, &args.url).await?;
    match args.query.clone() {
        Some(query) => {
            let out = backend.query(&query).await?;
            if args.json {
                for row in &out.rows {
                    println!("{row}");
                }
            } else {
                print_table(&out.rows);
            }
            report_caveats(&out);
            Ok(())
        }
        None => repl(backend).await,
    }
}

/// Everything about a result that the rows themselves do not say. To **stderr**, so `--json` stays a
/// clean pipe and the caveat still reaches a human watching the terminal.
///
/// The degraded line is the one that matters (#435). `nuthatch sql`'s default rendering is a table of
/// rows, and a `degraded` field the CLI declined to print would be exactly the invisible signal the
/// issue is about - the whole point is that a caller who ignores the flag still gets told. A reduced
/// table looks identical to a small one from here, so absent the line the operator sums a column and
/// gets a confident wrong number off their own machine.
fn report_caveats(out: &analytics::QueryOutput) {
    for line in caveats(out) {
        eprintln!("{line}");
    }
}

/// The caveat lines themselves, split out from the printing so they can be asserted. A test that had
/// to capture stderr would be asserting the plumbing; this asserts the decision.
fn caveats(out: &analytics::QueryOutput) -> Vec<String> {
    let mut lines = Vec::new();
    if out.truncated {
        lines.push("(result truncated at 50000 rows)".to_string());
    }
    if out.tip_unavailable {
        // Whole-answer, not per-table (#472): the live tip could not be scanned at all, so this result
        // is sealed history only and misses everything since the last seal, on every table.
        lines.push(
            "warning: the hot tip could not be scanned. This result is sealed history only and \
             misses everything indexed since the last seal. Check the node's logs for the cause."
                .to_string(),
        );
    }
    if out.degraded() {
        // Phrased about the *nest*, not about this result, and it has to be. `define_views` builds
        // its table set from schema ∪ manifest ∪ hot and never sees the SQL, so `degraded_tables` is
        // a property of the nest - on a two-table nest with one bad segment, a query over the healthy
        // table is complete and correct, and "these rows are INCOMPLETE" would be a false statement
        // about a true flag. `SELECT 1` and `.tables` make it plainer: neither has rows drawn from any
        // of these tables, and neither has a total to understate.
        //
        // Cause-neutral for the same reason. The degraded set also carries the view whose whole-table
        // DDL failed with every segment binding fine (issue #434's shape), so "could not be read"
        // sends the operator hunting a corrupt file that provably is not there.
        lines.push(format!(
            "warning: this nest could not serve complete cold data for {}. Any result drawing on {} \
             is INCOMPLETE and totals over {} are understated. Check the node's logs for the cause.",
            out.degraded_tables
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
            if out.degraded_tables.len() == 1 {
                "it"
            } else {
                "them"
            },
            if out.degraded_tables.len() == 1 {
                "it"
            } else {
                "them"
            }
        ));
    }
    lines
}

/// Where `nuthatch sql` queries run: the local store (when `dev` is stopped) or the running instance's
/// HTTP API (when `dev` holds the single-writer redb). Opened once, so a REPL reuses one connection.
enum SqlBackend {
    Local {
        dir: std::path::PathBuf,
        store: store::Store,
    },
    Http {
        url: String,
        /// The route prefix a `mounts.toml` runtime serves this nest under, e.g. `/lbtc` - empty for a
        /// solo (`nuthatch.toml`) runtime, which serves `/sql` at the root (#509). Resolved once in
        /// `open`, from the live instance's own roster rather than a local `mounts.toml` read, so it is
        /// right even when `--dir` is a bare `data/<nid>` the CLI has no other context for.
        prefix: String,
        client: reqwest::Client,
        /// Why we are not local. `None` when a store is there but held by `dev` - the ordinary case,
        /// and not worth mentioning. `Some(path)` when there is no store at all, which is the case
        /// that used to be silently invented (#413) and the one a failed connection needs to name:
        /// "no instance running" is only half the truth when the directory has no nest in it either.
        absent_store: Option<std::path::PathBuf>,
    },
}

/// Best-effort: map `--dir data/<nid>` to the alias a `mounts.toml` runtime serves it under, so the
/// HTTP fallback asks for the route that actually exists (#509) rather than the bare `/sql` a
/// multi-nest runtime never mounts (RFC-0032 §7 nests each mount under `/<alias>`).
///
/// Resolved against the *live instance's* own roster (`GET /nests`), not a local `mounts.toml` read -
/// `--dir` is a `data/<nid>` the CLI has no runtime root for, and the roster is the one place that
/// already knows the nid → route mapping (`runtime.rs`'s `route_key`), tenant segment included.
///
/// Anything short of an unambiguous match leaves the request unprefixed, exactly as before #509: no
/// `/nests` route at all (a solo `nuthatch.toml` runtime), no entry for this nid, more than one (two
/// mounts sharing a dataset - `--url .../<alias>` disambiguates by hand), or a `--url` that already
/// carries a path of its own. The real connectivity error, if any, surfaces from the query itself, not
/// from this probe.
async fn resolve_mount_prefix(
    client: &reqwest::Client,
    url: &str,
    dir: &std::path::Path,
) -> String {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return String::new();
    };
    if !matches!(parsed.path(), "" | "/") {
        return String::new();
    }
    let Some(nid) = dir.file_name().and_then(|n| n.to_str()) else {
        return String::new();
    };
    let Ok(resp) = client.get(format!("{url}/nests")).send().await else {
        return String::new();
    };
    if !resp.status().is_success() {
        return String::new();
    }
    let Ok(body) = resp.json::<serde_json::Value>().await else {
        return String::new();
    };
    let Some(nests) = body.get("nests").and_then(|n| n.as_array()) else {
        return String::new();
    };
    let mut matches = nests
        .iter()
        .filter(|n| n.get("nid").and_then(|v| v.as_str()) == Some(nid))
        .filter_map(|n| n.get("base_path").and_then(|v| v.as_str()));
    match (matches.next(), matches.next()) {
        (Some(only), None) => only.to_string(),
        _ => String::new(),
    }
}

impl SqlBackend {
    async fn open(dir: &str, url: &str) -> Result<Self> {
        let dir = std::path::PathBuf::from(dir);
        let db = dir.join(config::DB_FILE);
        // Prefer local files; redb is single-writer, so if `dev` holds the store the open fails and we
        // fall back to the running instance's API - the same command works either way.
        //
        // The open is **non-creating** (#413). `Store::open` is `Database::create`, so this probe used
        // to answer its own question: in any directory without a nest it created an empty store,
        // reported `local nest at .`, returned no rows from the store it had just made, and never
        // reached the running instance that had the data. `--dir` defaults to `.`, so running one
        // directory up from the nest was enough. Non-creating, the three cases separate: absent →
        // HTTP, locked by `dev` → HTTP, present and free → local.
        match store::Store::open_existing(&db) {
            Ok(store) => Ok(SqlBackend::Local { dir, store }),
            Err(_) => {
                let url = url.trim_end_matches('/').to_string();
                let client = reqwest::Client::new();
                let prefix = resolve_mount_prefix(&client, &url, &dir).await;
                Ok(SqlBackend::Http {
                    url,
                    prefix,
                    client,
                    absent_store: (!db.exists()).then_some(db),
                })
            }
        }
    }

    fn describe(&self) -> String {
        match self {
            SqlBackend::Local { dir, .. } => format!("local nest at {}", dir.display()),
            SqlBackend::Http { url, .. } => format!("running nuthatch at {url}"),
        }
    }

    /// Both backends answer in the same shape, so the caveats a result carries (`truncated`,
    /// `degraded_tables`) survive the local/HTTP split instead of being flattened away at the boundary
    /// - the HTTP branch reconstructs them from the JSON the node already sends.
    async fn query(&self, sql: &str) -> Result<analytics::QueryOutput> {
        match self {
            SqlBackend::Local { dir, store } => {
                // Live tip ∪ sealed history, disjoint by the sealed watermark (COR-1).
                // Same silent-degradation shape as `serve.rs::run_sql_query` (#472): a hot-scan error
                // here used to fall back to cold-only with nothing said, indistinguishable from a
                // complete answer.
                let (hot, tip_unavailable) = match store.hot_rows_by_table() {
                    Ok(hot) => (hot, false),
                    Err(e) => {
                        tracing::error!(
                            "hot-tip scan failed - serving cold-only for this query: {e:#}"
                        );
                        (Default::default(), true)
                    }
                };
                let sealed_through = store.sealed_through();
                match analytics::query_hot_cold(
                    dir,
                    sql,
                    analytics::QueryGuard {
                        timeout: std::time::Duration::from_secs(30),
                        max_rows: 50_000,
                    },
                    &hot,
                    sealed_through,
                ) {
                    Ok(mut out) => {
                        out.tip_unavailable = tip_unavailable;
                        Ok(out)
                    }
                    Err(e) => {
                        // Errors as prompts (RFC-0016 §3), same as the HTTP path: classify against the
                        // nest's schema and append a fix hint. Schema is loaded only on the error path.
                        let raw = format!("{e:#}");
                        let hint = config::Config::load(dir)
                            .ok()
                            .and_then(|cfg| {
                                nuthatch::registry::from_nest(dir, &cfg).ok()
                            })
                            .and_then(|reg| {
                                nuthatch::analytics::enrich_query_error(
                                    dir,
                                    &raw,
                                    sql,
                                    &reg.schema(),
                                )
                            });
                        match hint {
                            Some(h) => anyhow::bail!("{raw}\n\nhint: {h}"),
                            None => anyhow::bail!("{raw}"),
                        }
                    }
                }
            }
            SqlBackend::Http {
                url,
                prefix,
                client,
                absent_store,
            } => {
                let resp = client
                    .get(format!("{url}{prefix}/sql"))
                    .query(&[("q", sql)])
                    .send()
                    .await
                    .with_context(|| match absent_store {
                        // Both halves are missing, so name both. Previously this said only "is dev
                        // running?" for a user whose real mistake was the directory (#413).
                        Some(db) => format!(
                            "querying {url} - no store at {} and nothing answering there. \
                             Is `nuthatch dev` running, and is --dir the nest directory?",
                            db.display()
                        ),
                        None => format!("querying {url} - is `nuthatch dev` running?"),
                    })?;
                let status = resp.status();
                let body: serde_json::Value =
                    resp.json().await.context("reading the API response")?;
                if !status.is_success() {
                    anyhow::bail!(
                        "{}",
                        body.get("error")
                            .and_then(|e| e.as_str())
                            .unwrap_or("query failed")
                    );
                }
                let rows = body
                    .get("rows")
                    .and_then(|r| r.as_array())
                    .cloned()
                    .unwrap_or_default();
                let truncated = body
                    .get("truncated")
                    .and_then(|t| t.as_bool())
                    .unwrap_or(false);
                // Absent (an older node) reads as healthy, which is the only safe default here: this
                // branch cannot distinguish "no degradation" from "does not report it", and inventing
                // a warning on every query against an older node would train the operator to ignore
                // the one that matters.
                let degraded_tables = body
                    .get("degraded_tables")
                    .and_then(|t| t.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                // Same absent-reads-healthy default as `degraded_tables` above, for the same reason: an
                // older node that predates #472 simply never sends the field.
                let tip_unavailable = body
                    .get("tip_unavailable")
                    .and_then(|t| t.as_bool())
                    .unwrap_or(false);
                Ok(analytics::QueryOutput {
                    rows,
                    truncated,
                    degraded_tables,
                    tip_unavailable,
                })
            }
        }
    }
}

/// The interactive `nuthatch sql` REPL: readline with history, dot-commands, and a table per query.
async fn repl(backend: SqlBackend) -> Result<()> {
    use rustyline::error::ReadlineError;
    println!("nuthatch sql - querying {}.", backend.describe());
    println!("Type SQL, or .help for commands. .exit (or Ctrl-D) to quit.");
    let mut rl = rustyline::DefaultEditor::new().context("starting the REPL")?;
    loop {
        match rl.readline("nuthatch> ") {
            Ok(line) => {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                let _ = rl.add_history_entry(line);
                if line.starts_with('.') {
                    if repl_meta(line, &backend).await {
                        break; // .exit / .quit
                    }
                    continue;
                }
                // A query error is printed, never fatal - the session stays open.
                match backend.query(line).await {
                    Ok(out) => {
                        print_table(&out.rows);
                        report_caveats(&out);
                    }
                    Err(e) => eprintln!("error: {e:#}"),
                }
            }
            Err(ReadlineError::Interrupted) => continue, // Ctrl-C clears the line
            Err(ReadlineError::Eof) => break,            // Ctrl-D exits
            Err(e) => {
                eprintln!("{e}");
                break;
            }
        }
    }
    Ok(())
}

/// Handle a REPL dot-command. Returns `true` when the session should exit.
async fn repl_meta(line: &str, backend: &SqlBackend) -> bool {
    let mut parts = line.split_whitespace();
    match parts.next() {
        Some(".exit") | Some(".quit") | Some(".q") => return true,
        Some(".help") => {
            println!(".tables            list the queryable tables");
            println!(".schema <table>    show a table's columns");
            println!(".exit / .quit      leave the REPL (or Ctrl-D)");
            println!("anything else is run as SQL (SELECT/WITH only).");
        }
        Some(".tables") => {
            run_meta_query(
                backend,
                "SELECT table_name FROM information_schema.tables \
                 WHERE NOT starts_with(table_name, '__hot_') ORDER BY table_name",
            )
            .await;
        }
        Some(".schema") => match parts.next() {
            Some(t) => {
                let q = format!(
                    "SELECT column_name, data_type FROM information_schema.columns \
                     WHERE table_name = '{}' ORDER BY ordinal_position",
                    t.replace('\'', "''")
                );
                run_meta_query(backend, &q).await;
            }
            None => eprintln!("usage: .schema <table>"),
        },
        _ => eprintln!("unknown command {line:?} - try .help"),
    }
    false
}

async fn run_meta_query(backend: &SqlBackend, sql: &str) {
    match backend.query(sql).await {
        Ok(out) => {
            print_table(&out.rows);
            // The dot-commands get the caveats too. `.tables` is the sharpest case: a table whose
            // view could not be defined is simply *absent* from the catalogue listing, which is the
            // naming-fault misread of #419 in its purest form - the warning names it.
            report_caveats(&out);
        }
        Err(e) => eprintln!("error: {e:#}"),
    }
}

/// Render query rows as a simple aligned ASCII table.
fn print_table(rows: &[serde_json::Value]) {
    use serde_json::Value;
    if rows.is_empty() {
        println!("(0 rows)");
        return;
    }
    // Column order: first-seen across rows (a query result's columns are consistent row to row).
    let mut cols: Vec<String> = Vec::new();
    for r in rows {
        if let Some(o) = r.as_object() {
            for k in o.keys() {
                if !cols.iter().any(|c| c == k) {
                    cols.push(k.clone());
                }
            }
        }
    }
    let cell = |v: Option<&Value>| -> String {
        match v {
            Some(Value::String(s)) => s.clone(),
            None | Some(Value::Null) => String::new(),
            Some(other) => other.to_string(),
        }
    };
    let table: Vec<Vec<String>> = rows
        .iter()
        .map(|r| cols.iter().map(|c| cell(r.get(c))).collect())
        .collect();
    let mut widths: Vec<usize> = cols.iter().map(|c| c.chars().count()).collect();
    for row in &table {
        for (i, s) in row.iter().enumerate() {
            widths[i] = widths[i].max(s.chars().count());
        }
    }
    let line = |cells: &[String]| -> String {
        cells
            .iter()
            .enumerate()
            .map(|(i, s)| format!(" {:<w$} ", s, w = widths[i]))
            .collect::<Vec<_>>()
            .join("|")
    };
    println!("{}", line(&cols));
    println!(
        "{}",
        widths
            .iter()
            .map(|w| "-".repeat(w + 2))
            .collect::<Vec<_>>()
            .join("+")
    );
    for row in &table {
        println!("{}", line(row));
    }
    let n = rows.len();
    println!("({n} row{})", if n == 1 { "" } else { "s" });
}

/// `nuthatch transform` - run a WASM transform component over a project's stored transfers.
fn run_transform(args: cli::TransformArgs) -> Result<()> {
    use std::path::{Path, PathBuf};
    let dir = PathBuf::from(&args.dir);
    // Non-creating, for the reason `nuthatch sql` is (#413): this reads a nest's stored transfers, so
    // a directory with no store has none to run over. Creating one got the same three things wrong -
    // it reported `0 transfers` and `✓ 0 facts out` for what is really "there is no nest here", and it
    // left an empty `nuthatch.redb` behind for a later `holds_data` to misread.
    let store = store::Store::open_existing(&dir.join(config::DB_FILE))
        .with_context(|| format!("no nest to transform at {}", dir.display()))?;
    let entities = store.recent(args.limit)?;
    println!(
        "→ running {} over {} transfers…",
        args.component,
        entities.len()
    );

    let input = transform::transfers_to_ipc(&entities)?;
    let runtime = transform::TransformRuntime::load(Path::new(&args.component))?;
    let output = runtime.run(&input)?;
    let facts = transform::ipc_to_json(&output)?;

    println!(
        "✓ {} facts out (pure, deterministic, sandboxed)",
        facts.len()
    );
    for f in facts.iter().take(5) {
        println!("    {f}");
    }
    Ok(())
}

/// A worker's identity, from the hostname.
///
/// Bails rather than inventing one: two workers sharing an id look like a single worker to the
/// registry and could each hold what it believes is its own lease - which is the one failure the whole
/// lease mechanism exists to prevent. Better to refuse to start than to guess.
#[cfg(feature = "postgres-store")]
fn hostname_or_bail() -> anyhow::Result<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "cannot determine this worker's identity - pass --id explicitly. Two workers sharing \
                 an id would each believe they hold their own lease."
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(truncated: bool, degraded: &[&str]) -> analytics::QueryOutput {
        analytics::QueryOutput {
            rows: vec![],
            truncated,
            degraded_tables: degraded.iter().map(|s| s.to_string()).collect(),
            tip_unavailable: false,
        }
    }

    /// **Issue #435, on the terminal.** `nuthatch sql`'s default rendering is a table of rows, and a
    /// `degraded` field the CLI declined to print is exactly the invisible signal the issue is about -
    /// a reduced table looks identical to a small one from here, so the operator sums a column on
    /// their own machine and gets a confident wrong number.
    #[test]
    fn a_reduced_result_warns_on_the_terminal_and_names_the_table() {
        let lines = caveats(&out(false, &["usdc__transfer"]));
        assert_eq!(lines.len(), 1, "one caveat, the degraded one: {lines:?}");
        assert!(
            lines[0].contains("INCOMPLETE") && lines[0].contains("usdc__transfer"),
            "the warning must name what is short: {lines:?}"
        );
    }

    /// The control. A CLI that warns on every query is a CLI whose warnings are ignored, and the
    /// assertion above would pass just as well against an unconditional `eprintln!`.
    #[test]
    fn a_healthy_result_prints_no_caveats() {
        assert!(
            caveats(&out(false, &[])).is_empty(),
            "an intact nest must print nothing"
        );
    }

    /// Truncation and degradation are independent, and a result can be both: the caller capped the
    /// rows *and* the cold data behind them was short. Reporting only the first would hide the one
    /// they cannot fix by re-querying.
    #[test]
    fn truncation_and_degradation_are_reported_independently() {
        assert_eq!(caveats(&out(true, &[])).len(), 1);
        assert_eq!(caveats(&out(true, &["a", "b"])).len(), 2);
        assert!(caveats(&out(true, &["a", "b"]))[1].contains("a, b"));
    }

    /// **Issue #472.** A tip failure is not per-table the way `degraded_tables` is, so it gets its own
    /// caveat rather than being folded in - and, same as #435, a caller who ignores the field must
    /// still be told on the terminal.
    #[test]
    fn a_lost_tip_warns_on_the_terminal_and_is_not_a_degraded_table() {
        let mut result = out(false, &[]);
        result.tip_unavailable = true;
        let lines = caveats(&result);
        assert_eq!(lines.len(), 1, "one caveat, the tip one: {lines:?}");
        assert!(
            lines[0].contains("hot tip"),
            "the warning must say what went missing: {lines:?}"
        );
        assert!(
            !lines[0].contains("INCOMPLETE"),
            "must not read as the degraded_tables wording - different cause, different remedy: {lines:?}"
        );
    }
}
