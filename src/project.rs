//! `nuthatch init` - resolve each contract's ABI and scaffold a nest (RFC-0001: N contracts).
//! `nuthatch add` - resolve one more contract's ABI and grow an existing nest, no re-init.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

use crate::abi;
use crate::chains;
use crate::cli::{AddArgs, InitArgs};
use crate::config::{Config, Contract, Extract, Nest};
use crate::rpc::RpcClient;

pub async fn init(args: InitArgs) -> Result<()> {
    // Three ways to start a nest: clone/copy a published one (`--from`), port a subgraph
    // (`--from-subgraph`), or resolve from addresses.
    if let Some(source) = args.from.clone() {
        return init_from(&source, &args.dir);
    }
    if let Some(source) = args.from_subgraph.clone() {
        return init_from_subgraph(&source, &args).await;
    }
    if args.addresses.is_empty() {
        bail!("provide one or more contract addresses, --from <git-url|dir>, or --from-subgraph <cid>");
    }
    // Chain identity: honour an explicit `--chain`, otherwise detect it. The first-run friction we
    // most want to delete is making the user know (and correctly spell) which chain their contract
    // is on - so when they don't say, we go and find out.
    let chain = match &args.chain {
        Some(name) => chains::resolve(name, &args.rpc).await?,
        None if args.rpc.is_empty() => detect_chain(&args.addresses).await?.into(),
        None => detect_chain_on_rpc(&args.addresses, &args.rpc).await?,
    };
    let dir = PathBuf::from(&args.dir);
    std::fs::create_dir_all(dir.join("abis"))
        .with_context(|| format!("cannot create {}", dir.display()))?;

    let addresses: Vec<String> = args
        .addresses
        .iter()
        .map(|a| normalise_address(a))
        .collect::<Result<_>>()?;
    let aliases = resolve_aliases(&args.alias, addresses.len())?;

    // An explicit `--rpc` is the whole endpoint pool, both for first-run resolution and in the
    // persisted config. Otherwise use the chain defaults.
    let rpc_urls = crate::rpc::select_rpcs(&args.rpc, chain.rpc_urls.iter().map(|s| s.to_string()));

    // One RPC client for best-effort deployment-block detection.
    let rpc = RpcClient::new(rpc_urls.clone())?;
    let tip = rpc.block_number().await.ok();

    let overrides = resolve_abi_overrides(&args.abi, addresses.len())?;

    let mut contracts = Vec::with_capacity(addresses.len());
    for (i, (address, alias)) in addresses.iter().zip(&aliases).enumerate() {
        let (abi_json, implementation) = match &overrides[i] {
            Some(path) => {
                println!("→ using local ABI {path} for {alias} ({address})");
                (read_local_abi(path)?, None)
            }
            None => {
                println!("→ resolving ABI for {alias} ({address}) on {}…", chain.name);
                let resolved = resolve_abi(&rpc, chain.chain_id, address).await?;
                (resolved.abi, resolved.implementation)
            }
        };
        let abi_path = format!("abis/{alias}.json");
        std::fs::write(
            dir.join(&abi_path),
            serde_json::to_string_pretty(&abi_json).context("failed to serialise ABI")?,
        )
        .with_context(|| format!("failed to write {abi_path}"))?;

        let start_block = match tip {
            Some(tip) => match detect_deploy_block(&rpc, address, tip).await {
                Ok(b) => {
                    println!("  ✓ deployed at block {b}");
                    Some(b)
                }
                Err(e) => {
                    println!("  · deployment block undetected ({e:#}); backfill starts from a tip offset");
                    None
                }
            },
            None => None,
        };

        report_proxy_history_gap(&rpc, implementation.as_deref(), start_block, tip, alias).await;

        // Does the ABI we just vendored actually decode what this address emits? Best-effort and
        // never fatal - but loud when the answer is no, because the alternative is a nest that
        // indexes nothing and says nothing about it.
        report_abi_fit(
            check_abi_fits(&rpc, address, &abi_json, tip, start_block, chain.log_window).await,
            alias,
            address,
        );

        contracts.push(Contract {
            alias: alias.clone(),
            address: address.clone(),
            start_block,
            abi: abi_path,
            events: Vec::new(),
        });
    }

    let config = Config {
        state_rpc_urls: Vec::new(),
        ipfs_gateways: Vec::new(),
        ipfs: Vec::new(),
        nest: Nest {
            name: nest_name(&dir),
            chain: chain.name.clone(),
            chain_id: chain.chain_id,
            rpc_urls,
            // Not `CURRENT_SCHEMA_VERSION`: a timestamped nest is a v1 file and stays readable by
            // 0.8.x. See `config::required_schema_version`.
            schema_version: crate::config::required_schema_version(!args.no_timestamps),
            // Serialised unconditionally (no `skip_serializing_if`), so every scaffolded nest states
            // its schema shape in the file. Someone reading `nuthatch.toml` to work out why a table
            // has no `block_timestamp` finds the answer there rather than in our serde defaults.
            block_timestamps: !args.no_timestamps,
        },
        contracts,
        screening: crate::config::Screening::default(),
        flags: crate::config::Flags::default(),
        alerts: Vec::new(),
        templates: Vec::new(),
        factories: Vec::new(),
        webhooks: Vec::new(),
        extract: Extract::default(),
        calls: Vec::new(),
    };
    config.save(&dir)?;

    // Build the registry from the vendored ABIs to generate the schema artifact + AI surface (one
    // source of truth: schema.json, llms.txt, the skill, and `/tables` all come from here).
    let table_count = write_nest_artifacts(&dir, &chain.name, &config)?;

    println!(
        "✓ scaffolded nest '{}' ({} contract(s), {} table(s)) in {}",
        config.nest.name,
        config.contracts.len(),
        table_count,
        dir.display()
    );
    println!("    nuthatch.toml              config");
    println!("    abis/                      resolved ABIs");
    println!("    schema.json                decoded tables + columns");
    println!("    semantic.toml              what the data means (edit freely)");
    println!("    views/                     authored SQL derivations (a commented starter to uncomment)");
    println!("    llms.txt                   how an AI agent queries this index");
    println!("    .claude/skills/nuthatch/   Claude Code skill (offline, no phone-home)");
    println!();
    println!("next:  nuthatch dev{}", dir_hint(&args.dir));
    println!("       nuthatch mcp   (expose this index to a coding agent over MCP)");
    Ok(())
}

/// `nuthatch add 0xAnother` - grow an existing nest with more contracts without re-`init`. This is
/// the natural "one or many contracts" flow (RFC-0001): the chain, RPC endpoints, and screening
/// config are already settled by `init`, so `add` only resolves each new contract's ABI, vendors it,
/// appends it to `nuthatch.toml`, and regenerates the derived artifacts (schema.json + the AI
/// surface). The next `dev` backfills the new contract from its own deployment block - the existing
/// contracts resume from their stored cursor, untouched.
pub async fn add(args: AddArgs) -> Result<()> {
    let dir = PathBuf::from(&args.dir);
    let mut config = Config::load(&dir).with_context(|| {
        format!(
            "no nest at '{}' (run `nuthatch init` first, or pass --dir)",
            dir.display()
        )
    })?;
    // The chain is the nest's, already chosen at init - never re-detected. Adding a contract that
    // lives on a different chain is a different nest (one cursor, one chain - non-negotiable).
    let chain = chains::from_config(&config.nest.chain, config.nest.chain_id);

    let new_addresses: Vec<String> = args
        .addresses
        .iter()
        .map(|a| normalise_address(a))
        .collect::<Result<_>>()?;
    // Refuse duplicates: a contract already in the nest must not be added twice (it would collide on
    // the alias/ABI and double-register decoders).
    for addr in &new_addresses {
        if config
            .contracts
            .iter()
            .any(|c| c.address.eq_ignore_ascii_case(addr))
        {
            bail!("{addr} is already in this nest");
        }
    }
    let aliases = add_aliases(&config.contracts, &args.alias, new_addresses.len())?;

    let rpc_urls = crate::rpc::select_rpcs(&args.rpc, config.nest.rpc_urls.iter().cloned());
    let rpc = RpcClient::new(rpc_urls)?;
    let tip = rpc.block_number().await.ok();

    std::fs::create_dir_all(dir.join("abis"))
        .with_context(|| format!("cannot create {}", dir.join("abis").display()))?;

    let overrides = resolve_abi_overrides(&args.abi, new_addresses.len())?;

    for (i, (address, alias)) in new_addresses.iter().zip(&aliases).enumerate() {
        let (abi_json, implementation) = match &overrides[i] {
            Some(path) => {
                println!("→ using local ABI {path} for {alias} ({address})");
                (read_local_abi(path)?, None)
            }
            None => {
                println!("→ resolving ABI for {alias} ({address}) on {}…", chain.name);
                let resolved = resolve_abi(&rpc, chain.chain_id, address).await?;
                (resolved.abi, resolved.implementation)
            }
        };
        let abi_path = format!("abis/{alias}.json");
        std::fs::write(
            dir.join(&abi_path),
            serde_json::to_string_pretty(&abi_json).context("failed to serialise ABI")?,
        )
        .with_context(|| format!("failed to write {abi_path}"))?;

        let start_block = match tip {
            Some(tip) => match detect_deploy_block(&rpc, address, tip).await {
                Ok(b) => {
                    println!("  ✓ deployed at block {b}");
                    Some(b)
                }
                Err(e) => {
                    println!("  · deployment block undetected ({e:#}); backfill starts from a tip offset");
                    None
                }
            },
            None => None,
        };

        report_proxy_history_gap(&rpc, implementation.as_deref(), start_block, tip, alias).await;

        report_abi_fit(
            check_abi_fits(&rpc, address, &abi_json, tip, start_block, chain.log_window).await,
            alias,
            address,
        );

        config.contracts.push(Contract {
            alias: alias.clone(),
            address: address.clone(),
            start_block,
            abi: abi_path,
            events: Vec::new(),
        });
    }

    config.save(&dir)?;
    let table_count = write_nest_artifacts(&dir, &chain.name, &config)?;

    println!(
        "✓ added {} contract(s); nest '{}' now has {} contract(s), {} table(s)",
        new_addresses.len(),
        config.nest.name,
        config.contracts.len(),
        table_count,
    );
    println!(
        "next:  nuthatch dev{}   (backfills the new contract(s) from deployment)",
        dir_hint(&args.dir)
    );
    Ok(())
}

/// `nuthatch schema` - regenerate the derived artifacts (`schema.json`, `llms.txt`, `semantic.toml`
/// footguns) from the current `nuthatch.toml`. The manual counterpart to what `init`/`add` do
/// automatically: run it after hand-editing the config - notably adding a factory `[[templates]]` /
/// `[[factories]]`, which introduces the `{template}__{event}` tables and their `*_dec` columns that
/// the auto path never saw. Idempotent: authored views and semantic descriptions are preserved.
pub fn regen(args: crate::cli::SchemaArgs) -> Result<()> {
    let dir = PathBuf::from(&args.dir);
    let config = Config::load(&dir)
        .with_context(|| format!("no nest at '{}' (need a nuthatch.toml)", dir.display()))?;
    let n = write_nest_artifacts(&dir, &config.nest.chain, &config)?;
    println!("✓ regenerated schema.json + AI surface from nuthatch.toml - {n} table(s)");
    Ok(())
}

/// Regenerate the derived artifacts **if they are missing or older than `nuthatch.toml`**, returning
/// what it did so the caller can say so.
///
/// A hand-written `nuthatch.toml` has no `schema.json`, and the consequence is worse than a missing
/// file: `schema.json` is what creates the derived `{col}_dec` columns, while the *advice* to use them
/// comes from the live registry. So the schema tool confidently recommended `delta → delta_dec` on 196
/// lines while **zero** `_dec` columns existed, and an agent following that advice got
/// `Binder Error: Referenced column "delta_dec" not found` (issue #241 item 2).
///
/// The AI surface being confidently wrong is the most expensive place to be wrong, so this does not
/// warn - it fixes. Regenerating is cheap, idempotent, and preserves authored views and semantic
/// descriptions; `dev` already rebuilds the child registry on startup for the same reason.
pub fn refresh_stale_artifacts(dir: &Path, config: &Config) -> Result<Option<String>> {
    let schema = dir.join("schema.json");
    let toml = dir.join(crate::config::CONFIG_FILE);
    let why = if !schema.exists() {
        "schema.json was missing"
    } else {
        // Compare mtimes. An unreadable timestamp on either side means "cannot prove it is fresh",
        // and regenerating costs milliseconds - so the doubt resolves toward correctness.
        let stale = match (
            std::fs::metadata(&schema).and_then(|m| m.modified()),
            std::fs::metadata(&toml).and_then(|m| m.modified()),
        ) {
            (Ok(s), Ok(t)) => s < t,
            _ => true,
        };
        if !stale {
            return Ok(None);
        }
        "schema.json was older than nuthatch.toml"
    };
    let n = write_nest_artifacts(dir, &config.nest.chain, config)?;
    Ok(Some(format!(
        "{why} - regenerated schema.json + AI surface ({n} table(s)). \
         The `{{col}}_dec` companions the schema tool recommends come from this file."
    )))
}

/// Build the registry from the vendored ABIs and (re)write the derived artifacts - `schema.json` and
/// the AI surface (`llms.txt` + the scaffolded skill). One source of truth: `init` and `add` both
/// call this so the artifacts never drift from `nuthatch.toml`. Returns the table count.
/// `nuthatch init --from-subgraph <cid|url>` (#241 item 5).
///
/// Ports a subgraph manifest into a nest: `dataSources` become `[[contracts]]`,
/// `templates` become `[[templates]]`, ABIs are vendored from the CIDs the
/// manifest pins, and `startBlock` carries across so the backfill starts where
/// the subgraph did rather than at genesis.
///
/// The report at the end is the point as much as the config is. A manifest
/// cannot say which template a factory creates — that lives in the mapping
/// WASM — so this states plainly what it mapped, what it skipped, and which
/// templates still need a creating event, rather than emitting a config that
/// looks complete and indexes nothing.
async fn init_from_subgraph(source: &str, args: &InitArgs) -> Result<()> {
    use crate::subgraph_import as sg;
    use std::collections::BTreeMap;

    let gateways: Vec<String> = if args.ipfs.is_empty() {
        sg::DEFAULT_IPFS_GATEWAYS
            .iter()
            .map(|s| s.to_string())
            .collect()
    } else {
        args.ipfs.clone()
    };

    println!("→ fetching subgraph manifest {source}…");
    // The operator typed this one, so it may be a URL.
    let raw = sg::fetch_ipfs(source, &gateways, sg::Origin::Operator).await?;
    let manifest = sg::parse_manifest(&raw)?;

    let network = manifest.network()?;
    // `--chain` wins if given: a manifest's network name and ours can disagree
    // (and a user porting to a fork needs the override).
    let chain = match &args.chain {
        Some(name) => chains::resolve(name, &args.rpc).await?,
        None => chains::lookup(&network)
            .with_context(|| {
                format!(
                    "the manifest indexes '{network}', which nuthatch has no built-in chain for - \
                     re-run with --chain <name> --rpc <url> to point at it yourself"
                )
            })?
            .into(),
    };

    let dir = PathBuf::from(&args.dir);
    std::fs::create_dir_all(dir.join("abis"))
        .with_context(|| format!("cannot create {}", dir.display()))?;

    let mut notes: Vec<String> = Vec::new();
    let mut taken: BTreeMap<String, u32> = BTreeMap::new();
    // CID -> ABI body, so a shared ABI is downloaded once however many sources pin it.
    let mut fetched: BTreeMap<String, serde_json::Value> = BTreeMap::new();
    let mut contracts: Vec<Contract> = Vec::new();
    let mut address_params: Vec<sg::AddressParam> = Vec::new();

    // ── dataSources → [[contracts]] ──────────────────────────────────────
    for ds in &manifest.data_sources {
        if !ds.is_evm() {
            notes.push(format!(
                "skipped `{}` (kind `{}`) - nuthatch indexes EVM logs, and stores a content \
                 hash as a column value rather than fetching what it points at",
                ds.name, ds.kind
            ));
            continue;
        }
        let Some(address) = ds.address.clone() else {
            notes.push(format!(
                "skipped `{}` - an EVM dataSource with no `source.address`",
                ds.name
            ));
            continue;
        };
        // A subgraph may legitimately declare a placeholder it never indexes.
        if address
            .trim_start_matches("0x")
            .trim_matches('0')
            .is_empty()
        {
            notes.push(format!("skipped `{}` - address is {address}", ds.name));
            continue;
        }
        // Normalise before anything is fetched or written. This is the one check that used to
        // abort the whole import, and it fired after the ABI was already on disk and the alias
        // already consumed - a malformed address is the manifest's mistake, and every other
        // malformed field here is a note and a `continue`.
        let address = match normalise_address(&address) {
            Ok(a) => a,
            Err(e) => {
                notes.push(format!("skipped `{}` - {e}", ds.name));
                continue;
            }
        };
        let Some(abi_ref) = ds.own_abi().cloned() else {
            notes.push(format!(
                "skipped `{}` - the manifest pins no ABI for it, so there is nothing to decode",
                ds.name
            ));
            continue;
        };
        if ds.abi_is_fallback() {
            notes.push(format!(
                "`{}` asks for ABI `{}`, which `mapping.abis` does not contain - vendored `{}` \
                 instead; check it decodes what you expect",
                ds.name,
                ds.abi_name.as_deref().unwrap_or("?"),
                abi_ref.name
            ));
        }

        let alias = sg::dedupe_alias(&sg::to_alias(&ds.name), &mut taken);
        let abi_json =
            fetch_and_vendor_abi(&dir, &alias, &abi_ref, &gateways, &mut notes, &mut fetched)
                .await?;
        address_params.extend(sg::address_params(&alias, &abi_json));

        // Carry the manifest's event allowlist: a subgraph that handles only
        // `Transfer` should not silently grow every other event's table.
        let events: Vec<String> = ds
            .events
            .iter()
            .map(|sig| sg::event_name(sig).to_string())
            .collect();
        // An empty allowlist means "every event in the ABI" (see `config::Contract::events`),
        // so a source whose handlers are all block/call handlers lands in exactly the state the
        // comment above says it should not - silently, unless we say so.
        if events.is_empty() && ds.has_non_event_handlers {
            notes.push(format!(
                "`{}` declares only block/call handlers, which nuthatch has no equivalent for - \
                 it indexes logs. This contract will index **every** event its ABI defines; \
                 narrow it with `events = [...]` in nuthatch.toml if that is not what you want",
                ds.name
            ));
        }
        if let Some(end) = ds.end_block {
            notes.push(format!(
                "`{}` stops at block {end} in the subgraph; a nest has no end block, so it will \
                 keep indexing past it",
                ds.name
            ));
        }

        println!(
            "  ✓ {alias:<28} {address}  start={}  {} event(s)",
            ds.start_block
                .map(|b| b.to_string())
                .unwrap_or_else(|| "-".into()),
            events.len()
        );
        contracts.push(Contract {
            alias,
            address,
            start_block: ds.start_block,
            abi: String::new(), // filled below, once the alias is final
            events,
        });
        // `abi` path mirrors the alias; set it now that the alias is settled.
        let last = contracts.last_mut().expect("just pushed");
        last.abi = format!("abis/{}.json", last.alias);
    }

    if contracts.is_empty() {
        bail!(
            "no indexable dataSources in this manifest - {} source(s) were all skipped:\n  {}",
            manifest.data_sources.len(),
            notes.join("\n  ")
        );
    }

    // ── templates → [[templates]] ────────────────────────────────────────
    let mut templates: Vec<crate::config::Template> = Vec::new();
    // `file/ipfs` templates, reported separately because their remedy is `[[ipfs]]`, not `[[factories]]`.
    let mut ipfs_templates: Vec<String> = Vec::new();
    // Manifest name → the alias it actually settled on, so factory rules can be emitted
    // against the same name the `[[templates]]` entry carries.
    let mut template_alias: BTreeMap<String, String> = BTreeMap::new();
    for t in &manifest.templates {
        if !t.is_evm() {
            // A `file/ipfs` template is not an unportable thing any more (RFC-0037): it is a
            // resolution nuthatch can express, once somebody says which column carries the CID.
            // Which column that is lives in the mapping WASM - same reason a factory's rule does -
            // so the manifest cannot tell us, but "we cannot do this" is now the wrong answer.
            if t.kind.starts_with("file/ipfs") {
                ipfs_templates.push(t.name.clone());
            } else {
                notes.push(format!("skipped template `{}` (kind `{}`)", t.name, t.kind));
            }
            continue;
        }
        let Some(abi_ref) = t.own_abi().cloned() else {
            notes.push(format!("skipped template `{}` - no ABI pinned", t.name));
            continue;
        };
        let alias = sg::dedupe_alias(&sg::to_alias(&t.name), &mut taken);
        let abi_json =
            fetch_and_vendor_abi(&dir, &alias, &abi_ref, &gateways, &mut notes, &mut fetched)
                .await?;
        // Both the entry and its ABI path must use the *settled* alias. Recomputing
        // `to_alias(&t.name)` here would name the file the template was never written to:
        // a dataSource `Vault` plus a template `Vault` — the canonical factory shape — makes
        // the template point at the dataSource's ABI. Nothing errors, because that file
        // exists; every discovered child is just decoded against the wrong contract.
        template_alias.insert(t.name.clone(), alias.clone());
        // Carry the manifest's own allowlist, exactly as the `[[contracts]]` loop does. A subgraph
        // declares `eventHandlers` per template, so the manifest already says which of the ABI's
        // events the author cared about - dropping it here made an imported nest decode a superset
        // of what the subgraph decoded, which is the one thing this import must not do.
        let events: Vec<String> = t
            .events
            .iter()
            .map(|sig| sg::event_name(sig).to_string())
            .collect();
        // The gap is worth saying out loud at init: it is the difference between the workload the
        // subgraph ran and the one this nest will run, and it is invisible until a row count
        // surprises someone.
        if !events.is_empty() {
            let defined = abi_json
                .as_array()
                .map(|a| a.iter().filter(|e| e["type"] == "event").count())
                .unwrap_or(0);
            if defined > events.len() {
                notes.push(format!(
                    "template `{alias}` handles {} of the {defined} events its ABI defines - \
                     the rest are not indexed",
                    events.len()
                ));
            }
        }
        templates.push(crate::config::Template {
            name: alias.clone(),
            abi: format!("abis/{alias}.json"),
            filter: None,
            events,
        });
    }

    // ── factory inference ────────────────────────────────────────────────
    // Inference runs on the manifest's own names, so the rules it returns have to be mapped
    // back through the same settled aliases before they are written out.
    let template_names: Vec<String> = template_alias.keys().cloned().collect();
    let (inferred, unresolved) = sg::infer_factories(&template_names, &address_params);

    let factories: Vec<crate::config::Factory> = inferred
        .iter()
        .map(|f| crate::config::Factory {
            watch: f.watch.clone(),
            event: f.event.clone(),
            child_param: f.child_param.clone(),
            // The settled alias, not a fresh `to_alias` of the manifest name — see the
            // template loop above. A recomputed alias names a template that may not exist.
            template: template_alias
                .get(&f.template)
                .cloned()
                .unwrap_or_else(|| sg::to_alias(&f.template)),
            start: None,
        })
        .collect();

    let rpc_urls = crate::rpc::select_rpcs(&args.rpc, chain.rpc_urls.iter().map(|s| s.to_string()));
    let config = Config {
        state_rpc_urls: Vec::new(),
        ipfs_gateways: Vec::new(),
        ipfs: Vec::new(),
        nest: Nest {
            name: nest_name(&dir),
            chain: chain.name.clone(),
            chain_id: chain.chain_id,
            rpc_urls,
            schema_version: crate::config::required_schema_version(!args.no_timestamps),
            block_timestamps: !args.no_timestamps,
        },
        contracts,
        screening: crate::config::Screening::default(),
        flags: crate::config::Flags::default(),
        alerts: Vec::new(),
        templates,
        factories,
        webhooks: Vec::new(),
        extract: Extract::default(),
        calls: Vec::new(),
    };
    config.save(&dir)?;
    let table_count = write_nest_artifacts(&dir, &chain.name, &config)?;

    // ── the honest report ────────────────────────────────────────────────
    println!(
        "\n✓ scaffolded nest '{}' from subgraph {source}",
        config.nest.name
    );
    println!(
        "    {} contract(s), {} template(s), {} table(s) on {}",
        config.contracts.len(),
        config.templates.len(),
        table_count,
        chain.name
    );
    for f in &inferred {
        println!(
            "  ✓ factory: {}.{} → {} via `{}` ({})",
            f.watch, f.event, f.template, f.child_param, f.because
        );
    }
    for u in &unresolved {
        if u.candidates.is_empty() {
            println!(
                "  ⚠ template `{}` has no creating event in this manifest - it will index \
                 nothing until you add a [[factories]] rule",
                u.template
            );
        } else {
            println!(
                "  ⚠ template `{}` needs a creating event; candidates:\n      {}",
                u.template,
                u.candidates.join("\n      ")
            );
        }
    }
    for n in &notes {
        println!("  ⚠ {n}");
    }
    if !unresolved.is_empty() || !notes.is_empty() {
        println!(
            "\n  The warnings above are work the manifest cannot do for us: which template a \
             factory creates lives in the mapping WASM, not in the manifest. Resolve them by \
             adding [[factories]] rules to nuthatch.toml."
        );
    }
    if !ipfs_templates.is_empty() {
        // Deliberately *not* filed with the warnings above, and deliberately not called "skipped":
        // the remedy is a different config block, and telling someone to write `[[factories]]` for a
        // file template sends them to a rule that cannot express it.
        println!(
            "\n  {} `file/ipfs` template(s) - {} - index the *content* behind a CID. nuthatch \
             resolves those (RFC-0037), but which column carries the CID lives in the mapping WASM \
             rather than the manifest, so it needs one line each:\n\n    \
             [[ipfs]]\n    name = \"token_metadata\"     # the table to put documents in\n    \
             on = \"<table>\"                # the table whose rows carry the CID\n    \
             cid_column = \"<column>\"       # which column that is\n\n  \
             Then run with --ipfs <gateway-or-your-own-node>. Every document is verified against its \
             CID before it is stored, and one that will not resolve leaves no row rather than a wrong \
             one.",
            ipfs_templates.len(),
            ipfs_templates.join(", ")
        );
    }
    println!("\n  Next: nuthatch dev{}", dir_hint(&args.dir));
    Ok(())
}

/// Fetch one ABI by CID and vendor it into `abis/<alias>.json`.
async fn fetch_and_vendor_abi(
    dir: &Path,
    alias: &str,
    abi_ref: &crate::subgraph_import::AbiRef,
    gateways: &[String],
    notes: &mut Vec<String>,
    fetched: &mut std::collections::BTreeMap<String, serde_json::Value>,
) -> Result<serde_json::Value> {
    // One CID, one download. Sharing an ABI across dataSources is the normal shape - every
    // proxy in a beacon codebase pins the same implementation ABI - and re-fetching it per
    // source multiplies gateway load by a factor the manifest chooses. Each source still gets
    // its own file, because the alias is what the config points at.
    if let Some(cached) = fetched.get(&abi_ref.cid) {
        write_abi(dir, alias, cached)?;
        return Ok(cached.clone());
    }
    // From the manifest, so CID only - see `subgraph_import::Origin`.
    let raw = crate::subgraph_import::fetch_ipfs(
        &abi_ref.cid,
        gateways,
        crate::subgraph_import::Origin::Manifest,
    )
    .await
    .with_context(|| format!("fetching ABI `{}` ({})", abi_ref.name, abi_ref.cid))?;
    let parsed: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("ABI `{}` ({}) is not JSON", abi_ref.name, abi_ref.cid))?;
    if !parsed.is_array() {
        notes.push(format!(
            "ABI `{}` is not a JSON array - vendored as-is, but the registry may reject it",
            abi_ref.name
        ));
    }
    write_abi(dir, alias, &parsed)?;
    fetched.insert(abi_ref.cid.clone(), parsed.clone());
    Ok(parsed)
}

/// Vendor one ABI under its settled alias.
fn write_abi(dir: &Path, alias: &str, abi: &serde_json::Value) -> Result<()> {
    std::fs::write(
        dir.join(format!("abis/{alias}.json")),
        serde_json::to_string_pretty(abi).context("failed to serialise ABI")?,
    )
    .with_context(|| format!("failed to write abis/{alias}.json"))
}

fn write_nest_artifacts(dir: &Path, chain_name: &str, config: &Config) -> Result<usize> {
    let registry = crate::registry::from_nest(dir, config)?;
    let mut schema = registry.schema();
    // RFC-0014: a nest that declares `[extract]` also declares call/state tables. The decode identity
    // folds in the call surface, so two nests differing only in what they extract are not mistaken for
    // the same decode version - the hash is what segment reuse and `check` compare.
    let mut hash = registry.hash();
    if config.extract.decodes_calls() {
        let calls = crate::calldata::CallRegistry::from_nest(dir, config)?;
        schema.extend(calls.schema(&config.extract));
        let mut h = <sha2::Sha256 as sha2::Digest>::new();
        sha2::Digest::update(&mut h, hash);
        sha2::Digest::update(&mut h, calls.hash());
        hash = <sha2::Sha256 as sha2::Digest>::finalize(h).into();
    }
    // RFC-0023 tier 3: a declared `[[calls]]` read is a table too, and it moves the decode identity
    // for the same reason `[extract]` does - two nests differing only in what they read must not be
    // mistaken for the same decode version by segment reuse.
    if !config.ipfs.is_empty() {
        schema.extend(crate::ipfs::schema(&config.ipfs, registry.timestamps()));
        let mut h = <sha2::Sha256 as sha2::Digest>::new();
        sha2::Digest::update(&mut h, hash);
        sha2::Digest::update(&mut h, crate::ipfs::decl_hash(&config.ipfs));
        hash = <sha2::Sha256 as sha2::Digest>::finalize(h).into();
    }
    if !config.calls.is_empty() {
        schema.extend(crate::calls::schema(&config.calls, registry.timestamps()));
        let mut h = <sha2::Sha256 as sha2::Digest>::new();
        sha2::Digest::update(&mut h, hash);
        sha2::Digest::update(&mut h, crate::calls::decl_hash(&config.calls));
        hash = <sha2::Sha256 as sha2::Digest>::finalize(h).into();
    }
    std::fs::write(
        dir.join("schema.json"),
        serde_json::to_string_pretty(&serde_json::json!({
            "registry_hash": format!("0x{}", hex::encode(hash)),
            "tables": &schema,
        }))?,
    )
    .context("failed to write schema.json")?;
    scaffold_ai_surface(dir, chain_name, &config.contracts, &schema)?;

    // The logic layer (RFC-0018 §1): scaffold `views/` with a commented, ready-to-uncomment starter so
    // the authored-derivations layer is *discoverable* the moment you `init` - the happy path is
    // unchanged (a directory of comments; the commented starter is a no-op that validates clean).
    scaffold_views(dir, &schema)?;

    // The governed semantic layer (RFC-0016): generate `semantic.toml` from the registry - ABI-seeded
    // descriptions + derived footguns. On `add`, merge onto the existing file so authored descriptions
    // survive while the footguns are refreshed (init has no existing file, so it just writes fresh).
    let generated = crate::semantic::generate(&schema, &config.nest.name, chain_name);
    let sem = match crate::semantic::load(dir)? {
        Some(existing) => crate::semantic::merge(existing, generated),
        None => generated,
    };
    crate::semantic::save(dir, &sem)?;

    Ok(schema.len())
}

/// Scaffold the `views/` logic layer (RFC-0018 §1b) with a commented, ready-to-uncomment starter view
/// derived from the nest's own first table, plus a README. Idempotent: if `views/` already exists (an
/// `add` on a nest whose author already wrote views), it's left untouched. The starter is entirely
/// comments, so it's a no-op for the query surface and validates clean until the author uncomments it.
fn scaffold_views(dir: &Path, schema: &[crate::registry::TableSchema]) -> Result<()> {
    let views = dir.join("views");
    if views.exists() {
        return Ok(()); // author already has a views/ - never clobber it
    }
    std::fs::create_dir_all(&views)
        .with_context(|| format!("cannot create {}", views.display()))?;

    std::fs::write(
        views.join("README.md"),
        "# views/ - this nest's authored logic (RFC-0018 §1)\n\n\
         Drop `*.sql` files here, each a `CREATE VIEW …` over your nest's tables (the live tip ∪ sealed\n\
         history, one surface - recomputed per query, never materialised). Query a view by name with\n\
         `nuthatch sql` or the MCP `sql` tool. Files load in sorted filename order (`10-…`, `20-…`), so\n\
         a later view can build on an earlier one. Describe what a view *means* in `semantic.toml` under\n\
         `[view.<name>]` so an agent sees it. A broken/drifted view fails `nuthatch check` loudly.\n",
    )
    .context("failed to write views/README.md")?;

    // The starter references this nest's real first table when there is one, so it's copy-paste-true.
    let (table, alias) = schema
        .first()
        .map(|t| (t.table.as_str(), t.alias.as_str()))
        .unwrap_or(("your__event", "your"));
    let starter = format!(
        "-- views/10-example.sql - an authored derivation this nest computes. Uncomment to enable.\n\
         --\n\
         -- Read-only SQL over your nest's tables (tip ∪ sealed history), recomputed per query. Query it\n\
         -- by name via `nuthatch sql` or the MCP; describe it in semantic.toml `[view.<name>]`.\n\
         --\n\
         -- Footguns (see the builder skill's views.md):\n\
         --   • reserved-word columns like \"from\"/\"to\" must be double-quoted\n\
         --   • big-int columns are exact text - use the `<col>_dec` companion for SUM/AVG/compare\n\
         --\n\
         -- Example over this nest's `{table}` table:\n\
         --\n\
         -- CREATE VIEW {alias}_activity AS\n\
         --   SELECT count(*) AS events,\n\
         --          min(block_number) AS first_block,\n\
         --          max(block_number) AS last_block\n\
         --   FROM \"{table}\";\n"
    );
    std::fs::write(views.join("10-example.sql"), starter)
        .context("failed to write views/10-example.sql")?;
    Ok(())
}

/// Default aliases for `add`ed contracts: continue the `c<N>` sequence past the nest's existing
/// contracts, skipping any slot already taken. An explicit `--alias` list is validated and checked
/// for collisions with the existing contracts instead.
fn add_aliases(existing: &[Contract], provided: &[String], n: usize) -> Result<Vec<String>> {
    if !provided.is_empty() {
        if provided.len() != n {
            bail!("--alias expects {n} name(s), got {}", provided.len());
        }
        for a in provided {
            if !is_valid_alias(a) {
                bail!("alias '{a}' must match [a-z][a-z0-9_]*");
            }
            if existing.iter().any(|c| &c.alias == a) {
                bail!("alias '{a}' is already used in this nest");
            }
        }
        // Reject duplicates within the provided list too.
        for (i, a) in provided.iter().enumerate() {
            if provided[i + 1..].contains(a) {
                bail!("alias '{a}' given twice");
            }
        }
        return Ok(provided.to_vec());
    }
    let used: std::collections::HashSet<&str> = existing.iter().map(|c| c.alias.as_str()).collect();
    let mut out: Vec<String> = Vec::with_capacity(n);
    let mut k = existing.len();
    for _ in 0..n {
        let mut cand = format!("c{k}");
        while used.contains(cand.as_str()) || out.contains(&cand) {
            k += 1;
            cand = format!("c{k}");
        }
        out.push(cand);
        k += 1;
    }
    Ok(out)
}

/// Initialise a nest from a published one - a git URL or a local directory - instead of resolving
/// from addresses. The nest is self-contained (ABIs vendored, `nuthatch.toml` committed), so this
/// clones/copies it and validates it: the toml parses at a supported schema version and the decode
/// registry builds from the vendored ABIs. Publishing a nest is `git push`; consuming it is this.
fn init_from(source: &str, dir_arg: &str) -> Result<()> {
    // Default the target dir to the nest's own name (repo/dir basename) unless one was given.
    let target = if dir_arg == "." {
        PathBuf::from(source_basename(source))
    } else {
        PathBuf::from(dir_arg)
    };
    if target.exists() && target.read_dir().map(|mut d| d.next().is_some())? {
        bail!(
            "target '{}' already exists and is not empty",
            target.display()
        );
    }

    if is_git_source(source) {
        println!("→ cloning nest from {source} …");
        clone_repo(source, &target)?;
        // Drop the clone's history: a consumed nest is a plain working copy, not a live checkout.
        let _ = std::fs::remove_dir_all(target.join(".git"));
    } else {
        let src = PathBuf::from(source);
        if !src.is_dir() {
            bail!("--from '{source}' is neither a git URL nor an existing local directory");
        }
        println!("→ copying nest from {} …", src.display());
        copy_dir(&src, &target)?;
    }

    // Validate: it must be a real nest - toml at a supported schema version, ABIs present + decodable.
    let config = Config::load(&target)
        .with_context(|| format!("'{}' is not a valid nuthatch nest", target.display()))?;
    let registry = crate::registry::from_nest(&target, &config)
        .context("nest ABIs failed to build a decode registry (is the nest self-contained?)")?;
    // Validate factory rules (RFC-0009): references must resolve and depth stays within the ceiling.
    let factories = crate::factory::FactorySet::build(&config)
        .context("nest declares invalid factory/template rules")?;

    println!(
        "✓ nest '{}' ready - {} on {}, {} contract(s), {} table(s), {} anonymous event(s) skipped",
        config.nest.name,
        source_basename(source),
        config.nest.chain,
        config.contracts.len(),
        registry.tables().len(),
        registry.skipped_anonymous(),
    );
    if !factories.is_empty() {
        println!(
            "  factories: {} template(s), {} rule(s) - children discovered at runtime (RFC-0009)",
            config.templates.len(),
            config.factories.len(),
        );
    }
    println!("next:  nuthatch dev --dir {}", target.display());
    Ok(())
}

/// Whether `--from` names a git remote (vs. a local directory).
/// Transports git will accept from us. Anything else is not a git source, whatever it is named.
///
/// **Audit finding 6.** The `.git` suffix used to be sufficient on its own, so `ext::sh -c … .git`
/// reached `git clone` - and git's `ext::` transport *executes the command*. It is refused today only
/// by git's own `protocol.ext.allow=never` default (git >= 2.12). An operator with
/// `protocol.ext.allow=always` in their gitconfig - not unheard of in CI images - would have turned
/// `nuthatch init --from <url>` into remote code execution.
///
/// Being safe by someone else's default is not the same as being safe. The scheme is now checked here,
/// so the guarantee is ours: a `.git` suffix qualifies a source only when its transport is one we
/// named. `ext::`, `file::`, `transport-helper::` and anything else are simply not git sources.
const GIT_SCHEMES: &[&str] = &["http://", "https://", "ssh://", "git://"];

fn is_git_source(source: &str) -> bool {
    if source.starts_with("git@") {
        return true; // scp-like syntax: user@host:path, no scheme to check
    }
    if GIT_SCHEMES.iter().any(|p| source.starts_with(p)) {
        return true;
    }
    // A `.git` suffix alone is not enough: it must still look like a plain path or scp-like target,
    // never a transport helper. `::` is the marker git uses for those (`ext::`, `transport::`).
    source.ends_with(".git") && !source.contains("::")
}

/// The nest's own name: the last path component, minus a trailing `.git` or slash.
fn source_basename(source: &str) -> String {
    source
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit(['/', ':'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("nest")
        .to_string()
}

/// Shallow-clone a nest repo into `target` using the system `git` (no in-process git dependency).
fn clone_repo(url: &str, target: &Path) -> Result<()> {
    let status = std::process::Command::new("git")
        .args(["clone", "--depth", "1", url])
        .arg(target)
        .status()
        .context("failed to run `git` - is it installed and on PATH?")?;
    if !status.success() {
        bail!("git clone of '{url}' failed");
    }
    Ok(())
}

/// Recursively copy a local nest directory (skipping any `.git`).
pub(crate) fn copy_dir(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).with_context(|| format!("cannot create {}", dst.display()))?;
    for entry in std::fs::read_dir(src).with_context(|| format!("cannot read {}", src.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        if name == ".git" {
            continue;
        }
        let from = entry.path();
        let to = dst.join(&name);
        if entry.file_type()?.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .with_context(|| format!("failed to copy {}", from.display()))?;
        }
    }
    Ok(())
}

/// Well-known proxy implementation storage slots, tried in order - each holds the implementation
/// address *directly*:
/// - EIP-1967: keccak256("eip1967.proxy.implementation") − 1
/// - EIP-1822 (UUPS "Proxiable"): keccak256("PROXIABLE")
/// - legacy OpenZeppelin/zeppelinos: keccak256("org.zeppelinos.proxy.implementation") (e.g. USDC)
const PROXY_IMPL_SLOTS: &[&str] = &[
    "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc",
    "0xc5f16f0fcc639fa48a6947836d9850f504798523bf8c9a3a87d5876cf622bcf7",
    "0x7050c9e0f4ca769c69bd3a8ef740bc37934f8e2c036e5a723fd8ee048ed3f8c3",
];

/// EIP-1967 beacon slot: keccak256("eip1967.proxy.beacon") − 1. A *beacon* proxy stores a beacon
/// address here (not the implementation); the implementation comes from calling `implementation()` on
/// that beacon - a common shape for factory-deployed proxies that share one upgradeable logic contract.
const PROXY_BEACON_SLOT: &str =
    "0xa3f0ad74e5423aebfd80d3ef4346578335a9a72aeaee59ff6cb3582b35133d50";

/// Selector for `implementation()` - `keccak256("implementation()")[..4]`. Both a beacon and an
/// EIP-897 delegate proxy expose the implementation this way.
const IMPLEMENTATION_SELECTOR: &str = "0x5c60da1b";

/// Resolve the ABI to index events with. For a proxy (e.g. USDC), events emit from the proxy address
/// but use the *implementation's* event definitions, so resolve the implementation's ABI. Falls back
/// to the address's own ABI if it isn't a proxy or the implementation can't resolve. Init-time only -
/// the resolved ABI is vendored and frozen, so the deterministic decode path never depends on a live
/// proxy read.
struct ResolvedAbi {
    abi: serde_json::Value,
    /// `Some` only when the vendored ABI came from an implementation rather than the address itself.
    implementation: Option<String>,
}

async fn resolve_abi(rpc: &RpcClient, chain_id: u64, address: &str) -> Result<ResolvedAbi> {
    if let Some(implementation) = resolve_implementation(rpc, address).await {
        println!("  · proxy → implementation {implementation}");
        if let Ok(resolved) = abi::resolve(chain_id, &implementation).await {
            print_abi_resolved(&resolved);
            return Ok(ResolvedAbi {
                abi: resolved.abi,
                implementation: Some(implementation),
            });
        }
        println!("  · implementation ABI unresolved; using the proxy's own ABI");
    }
    let resolved = abi::resolve(chain_id, address).await?;
    print_abi_resolved(&resolved);
    Ok(ResolvedAbi {
        abi: resolved.abi,
        implementation: None,
    })
}

/// The pretty lines for a resolved ABI, in the same `→`/`✓`/`·` two-space-indented prose the rest of
/// `init`/`add` use - a pure function (no I/O) so the exact wording is unit-tested without a live
/// network call (see `abi_resolved_lines` tests below). Previously this was a raw `tracing::info!`
/// that printed its own ISO timestamp and log level through the middle of the pretty output (#675);
/// the resolver name is real information worth keeping, so it moves here rather than disappearing.
fn abi_resolved_lines(resolved: &abi::Resolved) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(reason) = &resolved.fallback_reason {
        lines.push(format!("  · {reason}"));
    }
    lines.push(format!("  ✓ ABI resolved via {}", resolved.via));
    lines
}

fn print_abi_resolved(resolved: &abi::Resolved) {
    for line in abi_resolved_lines(resolved) {
        println!("{line}");
    }
}

/// Follow the well-known proxy patterns to an implementation address, or `None` if `address` is not a
/// recognised proxy. Direct-slot proxies (EIP-1967 / EIP-1822 / legacy zeppelinos) hold the impl
/// address in a storage slot; an EIP-897 proxy exposes `implementation()` directly; a beacon proxy
/// holds a beacon whose `implementation()` we then call.
async fn resolve_implementation(rpc: &RpcClient, address: &str) -> Option<String> {
    for slot in PROXY_IMPL_SLOTS {
        if let Ok(word) = rpc.get_storage_at(address, slot).await {
            if let Some(implementation) = impl_from_slot(&word) {
                return Some(implementation);
            }
        }
    }
    // EIP-897 / Aragon AppProxyUpgradeable: unlike a beacon, the proxy itself exposes the logic
    // address through `implementation()`. stETH is this shape. We used to call this selector only
    // on a beacon found in EIP-1967 storage, so a proxy that had the standard public method but no
    // standard storage slot was needlessly treated as a bespoke, unresolvable proxy.
    if let Ok(ret) = rpc.eth_call(address, IMPLEMENTATION_SELECTOR).await {
        if let Some(implementation) = impl_from_slot(&ret) {
            return Some(implementation);
        }
    }
    // Beacon proxy: the implementation is one hop further - the proxy points at a beacon, and the
    // beacon answers `implementation()`. Both the stored word and the call return are 32-byte,
    // left-padded addresses, so `impl_from_slot` decodes either.
    if let Ok(word) = rpc.get_storage_at(address, PROXY_BEACON_SLOT).await {
        if let Some(beacon) = impl_from_slot(&word) {
            if let Ok(ret) = rpc.eth_call(&beacon, IMPLEMENTATION_SELECTOR).await {
                if let Some(implementation) = impl_from_slot(&ret) {
                    return Some(implementation);
                }
            }
        }
    }
    None
}

/// Extract a non-zero implementation address from a 32-byte storage word.
fn impl_from_slot(slot: &str) -> Option<String> {
    let h = slot.trim_start_matches("0x");
    if h.len() < 40 {
        return None;
    }
    let addr = &h[h.len() - 40..];
    if addr.chars().all(|c| c == '0') {
        return None;
    }
    Some(format!("0x{addr}"))
}

/// What a sample of the contract's real logs says about the ABI we resolved for it.
///
/// The failure this exists to catch is the quietest one nuthatch has: a proxy whose implementation
/// ABI the public resolvers don't return. Sourcify/Etherscan answer with the *proxy's* ABI, which is
/// usually two or three administrative events, so `init` succeeds, the schema looks plausible, and
/// `dev` then indexes **nothing** - no error, no warning, just empty tables. It cost us most of a day
/// on the Livepeer nest, whose `ManagerProxy` matches no standard proxy slot, and the only reason we
/// worked it out was noticing the event count looked too small.
///
/// Detecting it directly is cheap: fetch a handful of the address's actual logs and see whether the
/// ABI decodes any of them.
#[derive(Debug, PartialEq)]
enum AbiFit {
    /// At least one sampled log matches an event in the ABI. Says nothing about the other events,
    /// which is fine - it rules out the total-mismatch case, and that is the one that is silent.
    Fits,
    /// Logs exist and **none** of them match. This is the proxy signature.
    Mismatch { sampled: usize },
    /// The sample was empty, so there is nothing to conclude. A dormant contract and a wrong ABI look
    /// identical from here, and claiming otherwise would train people to ignore the warning.
    NoSample,
    /// The probe could not be run (RPC down, range refused, rate limited). Never blocks `init` -
    /// resolution is best-effort by design, like deploy-block detection above it.
    Unknown,
}

/// Sample recent logs from `address` and report whether `abi` decodes any of them.
///
/// Two probes at most. The tip window catches any active contract - which is what the people who hit
/// this are indexing - and the deployment window catches a contract that was busy once and has since
/// gone quiet. A dormant contract yields [`AbiFit::NoSample`] and no claim is made.
async fn check_abi_fits(
    rpc: &RpcClient,
    address: &str,
    abi: &serde_json::Value,
    tip: Option<u64>,
    start_block: Option<u64>,
    log_window: u64,
) -> AbiFit {
    let Some(tip) = tip else {
        return AbiFit::Unknown;
    };

    let topic0s = abi_event_topic0s(abi);
    if topic0s.is_empty() {
        // An ABI with no events at all decodes nothing by construction - the strongest version of the
        // signal, and worth reporting without spending an RPC call on it.
        return AbiFit::Mismatch { sampled: 0 };
    }

    let windows = probe_windows(tip, start_block, log_window);

    let mut any_error = false;
    for (from, to) in windows {
        match rpc.get_logs(&[address.to_string()], &[], from, to).await {
            Ok(logs) if !logs.is_empty() => {
                let sample: Vec<Option<&str>> = logs
                    .iter()
                    .map(|l| l.topics.first().map(|s| s.as_str()))
                    .collect();
                return fit_from_sample(&topic0s, &sample);
            }
            Ok(_) => {}
            Err(_) => any_error = true,
        }
    }
    if any_error {
        AbiFit::Unknown
    } else {
        AbiFit::NoSample
    }
}

/// Block ranges to probe, tip window first. `probe` is the chain's own measured, RPC-safe
/// `eth_getLogs` span (`Chain::log_window`) - not a fixed guess. A wider guess trips a busy mainnet
/// contract straight through every default provider's result-size cap (#512: a hardcoded 1,000-block
/// probe of USDC errored on all three mainnet defaults - nodies, drpc, and onfinality), and the
/// fallback below then reports a confident verdict from an ancient, unrepresentative sample instead
/// of the `Unknown` an error is supposed to produce.
fn probe_windows(tip: u64, start_block: Option<u64>, probe: u64) -> Vec<(u64, u64)> {
    let mut windows = vec![(tip.saturating_sub(probe), tip)];
    if let Some(start) = start_block {
        if start.saturating_add(probe) < tip.saturating_sub(probe) {
            windows.push((start, start + probe));
        }
    }
    windows
}

/// The verdict itself, separated from fetching so it can be tested without a chain. `sample` is each
/// log's topic0 (`None` for an anonymous event, which has no topic0 and can never match).
fn fit_from_sample(topic0s: &[String], sample: &[Option<&str>]) -> AbiFit {
    if sample.is_empty() {
        return AbiFit::NoSample;
    }
    let hit = sample
        .iter()
        .flatten()
        // Case-insensitively: `eth_getLogs` responses are lowercase hex by convention but that is a
        // convention, not a guarantee, and a case mismatch here would fire the warning on a nest that
        // is perfectly fine.
        .any(|t| topic0s.iter().any(|k| k.eq_ignore_ascii_case(t)));
    if hit {
        AbiFit::Fits
    } else {
        AbiFit::Mismatch {
            sampled: sample.len(),
        }
    }
}

/// Print the verdict from [`check_abi_fits`]. Only [`AbiFit::Mismatch`] is worth interrupting for;
/// the other three are silence, because a warning that fires when nothing is wrong gets ignored on
/// the day it fires when something is.
fn report_abi_fit(fit: AbiFit, alias: &str, address: &str) {
    let AbiFit::Mismatch { sampled } = fit else {
        return;
    };
    let seen = if sampled == 0 {
        "the resolved ABI declares no events at all".to_string()
    } else {
        format!("none of its last {sampled} log(s) match any event in the resolved ABI")
    };
    eprintln!();
    eprintln!("  ⚠ {alias} ({address}): {seen}.");
    eprintln!("    As configured this contract will index **zero rows**, silently.");
    eprintln!("    The usual cause is a proxy: the public ABI resolvers return the *proxy's* ABI,");
    eprintln!("    while the events are defined by the implementation behind it. nuthatch follows");
    eprintln!("    the standard proxy slots automatically, so this one uses a bespoke pattern.");
    eprintln!("    Fix: get the implementation's ABI and re-run with");
    eprintln!("      nuthatch init {address} --abi path/to/implementation.json");
    eprintln!("    or overwrite abis/{alias}.json and run `nuthatch schema` to regenerate.");
    eprintln!();
}

/// Per-address `--abi` overrides, positionally aligned with the addresses like `--alias`. An empty
/// entry means "resolve this one normally", so overriding the second of three contracts does not
/// force you to find local ABIs for the other two.
fn resolve_abi_overrides(provided: &[String], n: usize) -> Result<Vec<Option<String>>> {
    if provided.is_empty() {
        return Ok(vec![None; n]);
    }
    if provided.len() != n {
        bail!(
            "{} --abi path(s) for {n} address(es) - provide one per address (an empty entry resolves \
             that address normally) or none at all",
            provided.len()
        );
    }
    Ok(provided
        .iter()
        .map(|p| {
            let t = p.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        })
        .collect())
}

/// Read and validate a local ABI file. Parsed as a `JsonAbi` before it is accepted so a wrong file
/// (a subgraph manifest, a contract artifact wrapping the ABI under `"abi"`) fails here with a clear
/// message rather than at decode time as an empty registry.
fn read_local_abi(path: &str) -> Result<serde_json::Value> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("cannot read ABI file '{path}'"))?;
    let parsed: serde_json::Value =
        serde_json::from_str(&raw).with_context(|| format!("'{path}' is not valid JSON"))?;
    // Solidity build artifacts (Hardhat/Foundry) wrap the ABI in an object. Accepting that shape is
    // two lines here and saves everyone a confusing failure later.
    let abi = match parsed.get("abi") {
        Some(inner) => inner.clone(),
        None => parsed,
    };
    serde_json::from_value::<alloy_json_abi::JsonAbi>(abi.clone())
        .with_context(|| format!("'{path}' does not parse as a contract ABI"))?;
    Ok(abi)
}

/// `0x`-prefixed topic0 of every event in an ABI. Uses the same `alloy_json_abi` selector the decode
/// registry keys on, so a match here means a match at decode time - not an approximation of one.
fn abi_event_topic0s(abi: &serde_json::Value) -> Vec<String> {
    let Ok(parsed) = serde_json::from_value::<alloy_json_abi::JsonAbi>(abi.clone()) else {
        return Vec::new();
    };
    parsed
        .events()
        .map(|e| format!("0x{}", hex::encode(e.selector())))
        .collect()
}

/// Aliases from `--alias` (validated, one per address) or defaults c0, c1, ….
fn resolve_aliases(provided: &[String], n: usize) -> Result<Vec<String>> {
    if provided.is_empty() {
        return Ok((0..n).map(|i| format!("c{i}")).collect());
    }
    if provided.len() != n {
        bail!(
            "{} aliases for {n} address(es) - provide one alias per address or none",
            provided.len()
        );
    }
    for a in provided {
        if !is_valid_alias(a) {
            bail!("alias '{a}' must match [a-z][a-z0-9_]*");
        }
    }
    Ok(provided.to_vec())
}

fn is_valid_alias(a: &str) -> bool {
    let mut chars = a.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

fn nest_name(dir: &Path) -> String {
    dir.canonicalize()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .filter(|n| !n.is_empty() && n != ".")
        .unwrap_or_else(|| "nest".to_string())
}

/// Binary-search the deployment block: smallest block where the contract has code.
/// ~log2(tip) ≈ 25 `eth_getCode` calls. Best-effort - the caller tolerates failure.
async fn detect_deploy_block(rpc: &RpcClient, address: &str, tip: u64) -> Result<u64> {
    if is_empty_code(&rpc.get_code(address, tip).await?) {
        bail!("no code at tip");
    }
    let (mut lo, mut hi) = (0u64, tip);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if is_empty_code(&rpc.get_code(address, mid).await?) {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

/// A proxy cannot have emitted events decoded by its *current* implementation before that
/// implementation was deployed. This is not a proof that its event surface changed, but it is a
/// cheap, concrete warning that the automatically vendored ABI cannot describe all declared history.
fn proxy_history_may_be_missing(proxy_start: u64, implementation_start: u64) -> bool {
    proxy_start < implementation_start
}

/// Warn about the current-implementation trap from #773. Resolving the current ABI correctly is not
/// enough when the nest starts before that implementation existed: the old event names are absent
/// from the registry, so their logs would be skipped while the cursor appears healthy.
async fn report_proxy_history_gap(
    rpc: &RpcClient,
    implementation: Option<&str>,
    proxy_start: Option<u64>,
    tip: Option<u64>,
    alias: &str,
) {
    let (Some(implementation), Some(proxy_start), Some(tip)) = (implementation, proxy_start, tip)
    else {
        return;
    };
    let Ok(implementation_start) = detect_deploy_block(rpc, implementation, tip).await else {
        return;
    };
    if proxy_history_may_be_missing(proxy_start, implementation_start) {
        eprintln!();
        eprintln!(
            "  ⚠ {alias}: the proxy starts at block {proxy_start}, but its current implementation \
             was deployed at block {implementation_start}."
        );
        eprintln!(
            "    Events before block {implementation_start} may use earlier implementation ABIs and \
             will not be decoded by this scaffold."
        );
        eprintln!(
            "    Add the earlier ABI as a second contract entry at the same address, bounded to its \
             implementation era, before trusting historical totals."
        );
        eprintln!();
    }
}

fn is_empty_code(code: &str) -> bool {
    code.trim_start_matches("0x").is_empty()
}

/// Detect which registered chain a contract lives on by probing `eth_getCode` on each chain's
/// default endpoints in parallel. We probe the *first* address (a nest's contracts are expected to
/// share a chain - one cursor, one chain, per the non-negotiables) and pick, in registry order, the
/// first chain with bytecode there. Best-effort per chain: an unreachable endpoint reads as "not
/// here", never a hard failure, so one flaky RPC can't veto detection.
async fn detect_chain(addresses: &[String]) -> Result<&'static chains::Chain> {
    let probe = normalise_address(&addresses[0])?;
    println!("→ no --chain given; probing known chains for {probe}…");

    let probes = chains::all().iter().map(|chain| {
        let probe = probe.clone();
        async move {
            let rpc =
                RpcClient::new(chain.rpc_urls.iter().map(|s| s.to_string()).collect()).ok()?;
            let tip = rpc.block_number().await.ok()?;
            let code = rpc.get_code(&probe, tip).await.ok()?;
            (!is_empty_code(&code)).then_some(*chain)
        }
    });
    let found: Vec<&'static chains::Chain> = futures::future::join_all(probes)
        .await
        .into_iter()
        .flatten()
        .collect();

    match found.as_slice() {
        [] => bail!(
            "couldn't find bytecode for {probe} on any known chain (mainnet, arbitrum-one, base).\n\
             Pass --chain explicitly, or --rpc <url> for a custom endpoint."
        ),
        [only] => {
            println!("  ✓ found on {}", only.name);
            Ok(only)
        }
        [first, rest @ ..] => {
            let others: Vec<&str> = rest.iter().map(|c| c.name).collect();
            println!(
                "  ✓ found on {} (also deployed on {} - pass --chain to pick another)",
                first.name,
                others.join(", ")
            );
            Ok(first)
        }
    }
}

/// Detect a known chain through the endpoint(s) the operator supplied, without probing public
/// defaults. `--rpc` is commonly supplied precisely to keep a run inside a paid, audited pool.
async fn detect_chain_on_rpc(
    addresses: &[String],
    rpc_urls: &[String],
) -> Result<chains::ResolvedChain> {
    let probe = normalise_address(&addresses[0])?;
    println!("→ no --chain given; checking {probe} through the supplied RPC endpoint(s)…");
    let rpc = RpcClient::new(rpc_urls.to_vec())?;
    let chain_id = rpc
        .chain_id()
        .await
        .context("could not read chain id from --rpc")?;
    let chain = chains::all()
        .iter()
        .find(|chain| chain.chain_id == chain_id)
        .copied()
        .with_context(|| {
            format!(
                "--rpc reports unregistered chain id {chain_id}; pass --chain <name> to name it"
            )
        })?;
    let tip = rpc
        .block_number()
        .await
        .context("could not read tip from --rpc")?;
    let code = rpc
        .get_code(&probe, tip)
        .await
        .context("could not read contract bytecode from --rpc")?;
    if is_empty_code(&code) {
        bail!(
            "no bytecode for {probe} on {} via the supplied RPC endpoint(s)",
            chain.name
        );
    }
    println!("  ✓ found on {}", chain.name);
    Ok(chains::ResolvedChain {
        name: chain.name.to_string(),
        chain_id: chain.chain_id,
        rpc_urls: rpc_urls.to_vec(),
        finality: chain.finality,
        log_window: chain.log_window,
    })
}

fn scaffold_ai_surface(
    dir: &Path,
    chain: &str,
    contracts: &[Contract],
    schema: &[crate::registry::TableSchema],
) -> Result<()> {
    let list: String = contracts
        .iter()
        .map(|c| format!("- `{}` = {}\n", c.alias, c.address))
        .collect();
    let tables: String = schema
        .iter()
        .map(|t| {
            let cols: Vec<String> = t.columns.iter().map(|c| c.name.clone()).collect();
            format!("- `{}` - {} ({})\n", t.table, t.event, cols.join(", "))
        })
        .collect();
    let llms = format!(
        "# nuthatch nest on {chain}\n\
         \n\
         A self-hosted blockchain index. Query it locally; there is no third-party API.\n\
         \n\
         ## Contracts\n{list}\n\
         ## Tables (one per contract event)\n{tables}\n\
         ## Live HTTP API (run `nuthatch dev`)\n\
         - `GET /`                    index status\n\
         - `GET /tables`              every table with its columns\n\
         - `GET /table/{{name}}?limit=N` recent rows of one table (hot + sealed)\n\
         - `GET /entity/{{id}}`         one row by id (`{{block:012}}-{{logindex:06}}`)\n\
         - `GET /sql?q=SELECT...`     read-only SQL; each table is a view named `{{alias}}__{{event}}`\n\
         - `GET /balances?limit=N`    top holder balances (when an ERC-20 Transfer table is present)\n\
         - `GET /balance/{{address}}`   one address's derived balance\n\
         \n\
         ## MCP (for coding agents)\n\
         Run `nuthatch mcp` (stdio) to expose tools: status, schema, tables, table, sql, entity,\n\
         balance, top_balances. Fully offline against the local instance; nothing phones home.\n"
    );
    std::fs::write(dir.join("llms.txt"), llms).context("failed to write llms.txt")?;

    let skill_dir = dir.join(".claude/skills/nuthatch");
    std::fs::create_dir_all(&skill_dir).context("failed to create skill dir")?;
    let skill = format!(
        "---\n\
         name: nuthatch\n\
         description: Query this self-hosted nuthatch nest on {chain} - decoded events, balances, \
         and read-only SQL. Use when asked about on-chain activity for these contracts.\n\
         ---\n\
         \n\
         # Querying the nuthatch nest\n\
         \n\
         Contracts indexed on {chain}:\n{list}\n\
         Data is local - never call an external API for it.\n\
         \n\
         ## Preferred: MCP\n\
         If a `nuthatch` MCP server is configured, use its tools. Call `schema` first to learn the\n\
         data model, then `sql` / `entity` / `balance` / `top_balances`.\n\
         \n\
         ## Fallback: HTTP (a `nuthatch dev` must be running)\n\
         - Recent rows:  `curl localhost:8288/entities?limit=20`\n\
         - Read-only SQL: `curl -G localhost:8288/sql --data-urlencode 'q=SELECT count(*) FROM transfers'`\n\
         \n\
         `sql` sees finalized data only; balances/entity cover the live tip.\n"
    );
    std::fs::write(skill_dir.join("SKILL.md"), skill).context("failed to write SKILL.md")?;
    Ok(())
}

fn dir_hint(dir: &str) -> String {
    if dir == "." {
        String::new()
    } else {
        format!(" --dir {dir}")
    }
}

/// Minimal sanity check + lowercasing. Full checksum validation is a later concern.
fn normalise_address(addr: &str) -> Result<String> {
    let a = addr.trim();
    let hex = a.strip_prefix("0x").unwrap_or(a);
    if hex.len() != 40 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("'{addr}' is not a 20-byte hex address");
    }
    Ok(format!("0x{}", hex.to_ascii_lowercase()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure mirror of the deployment binary search, for algorithm confidence without RPC.
    fn find_deploy_block(tip: u64, deployed_from: u64) -> Option<u64> {
        let is_deployed = |b: u64| b >= deployed_from;
        if !is_deployed(tip) {
            return None;
        }
        let (mut lo, mut hi) = (0u64, tip);
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if is_deployed(mid) {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        Some(lo)
    }

    #[test]
    fn deploy_block_binary_search() {
        assert_eq!(find_deploy_block(1000, 137), Some(137));
        assert_eq!(find_deploy_block(1000, 0), Some(0));
        assert_eq!(find_deploy_block(1000, 1000), Some(1000));
        assert_eq!(find_deploy_block(100, 500), None); // not deployed by tip
    }

    #[test]
    fn aliases_default_and_validate() {
        assert_eq!(resolve_aliases(&[], 2).unwrap(), vec!["c0", "c1"]);
        assert_eq!(
            resolve_aliases(&["usdc".into(), "weth".into()], 2).unwrap(),
            vec!["usdc", "weth"]
        );
        assert!(resolve_aliases(&["usdc".into()], 2).is_err()); // count mismatch
        assert!(resolve_aliases(&["USDC".into()], 1).is_err()); // uppercase invalid
        assert!(resolve_aliases(&["1bad".into()], 1).is_err()); // leading digit invalid
    }

    fn contract(alias: &str) -> Contract {
        Contract {
            alias: alias.into(),
            address: format!("0x{alias}"),
            start_block: None,
            abi: format!("abis/{alias}.json"),
            events: Vec::new(),
        }
    }

    #[test]
    fn add_aliases_continue_and_avoid_collisions() {
        // Auto: continue the c<N> sequence past the existing count.
        let existing = vec![contract("c0"), contract("c1")];
        assert_eq!(add_aliases(&existing, &[], 2).unwrap(), vec!["c2", "c3"]);

        // Auto: skip a slot already taken by a custom alias so we never collide.
        let mixed = vec![contract("usdc"), contract("c1")];
        // len() == 2 → start at c2 (c1 is taken but c2 is free anyway).
        assert_eq!(add_aliases(&mixed, &[], 1).unwrap(), vec!["c2"]);

        // Explicit aliases are validated and collision-checked against the existing set.
        assert_eq!(
            add_aliases(&existing, &["weth".into()], 1).unwrap(),
            vec!["weth"]
        );
        assert!(add_aliases(&existing, &["c0".into()], 1).is_err()); // collides with existing
        assert!(add_aliases(&existing, &["WETH".into()], 1).is_err()); // invalid charset
        assert!(add_aliases(&existing, &["a".into()], 2).is_err()); // count mismatch
        assert!(add_aliases(&existing, &["x".into(), "x".into()], 2).is_err()); // dup in list
    }

    #[test]
    fn scaffold_views_creates_a_commented_starter_and_never_clobbers() {
        use crate::registry::{ColumnSchema, TableSchema};
        let dir = tempfile::tempdir().unwrap();
        let schema = vec![TableSchema {
            table: "usdc__transfer".into(),
            alias: "usdc".into(),
            kind: crate::registry::TableKind::Event,
            event: "Transfer".into(),
            topic0: "0xddf2".into(),
            function: String::new(),
            selector: String::new(),
            columns: vec![ColumnSchema {
                name: "value".into(),
                sol_type: "uint256".into(),
                storage: "word32".into(),
                indexed: false,
            }],
        }];
        scaffold_views(dir.path(), &schema).unwrap();
        let starter = std::fs::read_to_string(dir.path().join("views/10-example.sql")).unwrap();
        assert!(dir.path().join("views/README.md").exists());
        // References the nest's real table, and every line is a comment (a no-op that validates clean).
        assert!(starter.contains("usdc__transfer"));
        assert!(starter
            .lines()
            .all(|l| l.trim().is_empty() || l.trim_start().starts_with("--")));

        // Idempotent: a second call (e.g. `add` on a nest with authored views) never overwrites.
        std::fs::write(dir.path().join("views/10-example.sql"), "-- author's edit").unwrap();
        scaffold_views(dir.path(), &schema).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("views/10-example.sql")).unwrap(),
            "-- author's edit",
            "existing views/ is never clobbered"
        );
    }

    #[test]
    fn eip1967_impl_extraction() {
        assert!(impl_from_slot(
            "0x0000000000000000000000000000000000000000000000000000000000000000"
        )
        .is_none());
        assert_eq!(
            impl_from_slot("0x00000000000000000000000043506849d7c04f9138d1a2050bbf3a0c054402dd"),
            Some("0x43506849d7c04f9138d1a2050bbf3a0c054402dd".to_string())
        );
        // A beacon's `implementation()` return is the same 32-byte left-padded address as a slot word,
        // so the same decoder handles the beacon hop.
        assert_eq!(
            impl_from_slot("0x000000000000000000000000a0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"),
            Some("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48".to_string())
        );
        // An empty `eth_call` return (non-proxy / reverted) yields no implementation, not a bad address.
        assert!(impl_from_slot("0x").is_none());

        // stETH's Aragon AppProxyUpgradeable answers `implementation()` directly with this word.
        // Keep the observed shape here: its public ABI has only ProxyDeposit, so missing this hop
        // makes an otherwise ordinary `init <address>` scaffold an event surface that cannot match.
        assert_eq!(
            impl_from_slot("0x000000000000000000000000028271e30a695c0527a0c50ca30603fed004cdb0"),
            Some("0x028271e30a695c0527a0c50ca30603fed004cdb0".to_string())
        );
    }

    #[test]
    fn current_implementation_cannot_describe_history_before_it_existed() {
        assert!(proxy_history_may_be_missing(100, 200));
        assert!(!proxy_history_may_be_missing(200, 200));
        assert!(!proxy_history_may_be_missing(300, 200));
    }

    #[test]
    fn proxy_slots_are_well_formed() {
        // The three direct-address patterns (EIP-1967, EIP-1822, legacy zeppelinos) plus the beacon
        // slot are all 32-byte (66-char) storage keys.
        assert_eq!(PROXY_IMPL_SLOTS.len(), 3);
        assert!(PROXY_IMPL_SLOTS
            .iter()
            .all(|s| s.len() == 66 && s.starts_with("0x")));
        assert_eq!(PROXY_BEACON_SLOT.len(), 66);
        assert_eq!(IMPLEMENTATION_SELECTOR, "0x5c60da1b");
    }

    #[test]
    fn address_normalisation() {
        assert_eq!(
            normalise_address("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48").unwrap(),
            "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
        );
        assert!(normalise_address("0x123").is_err());
    }

    #[test]
    fn git_source_detection() {
        assert!(is_git_source("https://github.com/cargopete/horizon-nest"));
        assert!(is_git_source("git@github.com:cargopete/horizon-nest.git"));
        assert!(is_git_source("./local-bare-repo.git"));
        assert!(!is_git_source("./horizon-nest"));
        assert!(!is_git_source("/abs/path/to/nest"));
    }

    #[test]
    fn source_basename_derives_nest_dir() {
        assert_eq!(
            source_basename("https://github.com/cargopete/horizon-nest"),
            "horizon-nest"
        );
        assert_eq!(
            source_basename("https://github.com/cargopete/horizon-nest.git"),
            "horizon-nest"
        );
        assert_eq!(
            source_basename("git@github.com:cargopete/horizon-nest.git"),
            "horizon-nest"
        );
        assert_eq!(source_basename("./local/my-nest/"), "my-nest");
    }

    // ---- The silent-proxy check (RFC-0001 follow-up) --------------------------------------------

    /// The Livepeer shape: a `ManagerProxy` whose own ABI carries two administrative events, in front
    /// of an implementation that emits everything anyone actually wants. Sourcify returns the former.
    const PROXY_ABI: &str = r#"[
      {"type":"event","name":"ParameterUpdate","inputs":[{"name":"param","type":"string","indexed":false}],"anonymous":false},
      {"type":"event","name":"SetController","inputs":[{"name":"controller","type":"address","indexed":false}],"anonymous":false}
    ]"#;

    const IMPL_ABI: &str = r#"[
      {"type":"event","name":"Bond","inputs":[
        {"name":"newDelegate","type":"address","indexed":true},
        {"name":"oldDelegate","type":"address","indexed":true}],"anonymous":false}
    ]"#;

    fn topic0s_of(abi_src: &str) -> Vec<String> {
        abi_event_topic0s(&serde_json::from_str(abi_src).unwrap())
    }

    #[test]
    fn topic0s_come_from_the_same_selector_decode_uses() {
        let t = topic0s_of(IMPL_ABI);
        assert_eq!(t.len(), 1);
        // keccak("Bond(address,address)") - if this ever disagrees with the registry's topic0 the
        // check would pass on nests that cannot decode and fail on nests that can.
        let via_registry = {
            let abi: alloy_json_abi::JsonAbi = serde_json::from_str(IMPL_ABI).unwrap();
            let ev = abi.events().next().unwrap().clone();
            format!("0x{}", hex::encode(ev.selector()))
        };
        assert_eq!(t[0], via_registry);
    }

    /// The whole point: logs exist, the ABI decodes none of them.
    #[test]
    fn a_proxy_abi_against_implementation_logs_is_a_mismatch() {
        let bond = topic0s_of(IMPL_ABI)[0].clone();
        let fit = fit_from_sample(&topic0s_of(PROXY_ABI), &[Some(&bond), Some(&bond)]);
        assert_eq!(fit, AbiFit::Mismatch { sampled: 2 });
    }

    #[test]
    fn one_matching_log_is_enough_to_clear_the_check() {
        let bond = topic0s_of(IMPL_ABI)[0].clone();
        let other = "0xdeadbeef".repeat(8);
        assert_eq!(
            fit_from_sample(&topic0s_of(IMPL_ABI), &[Some(&other), Some(&bond)]),
            AbiFit::Fits,
            "a contract may emit events its ABI omits; only a total miss is the silent failure"
        );
    }

    /// A dormant contract and a wrong ABI look identical from an empty sample. Claiming a mismatch
    /// here would train people to ignore the warning.
    #[test]
    fn an_empty_sample_makes_no_claim() {
        assert_eq!(
            fit_from_sample(&topic0s_of(IMPL_ABI), &[]),
            AbiFit::NoSample
        );
    }

    #[test]
    fn topic0_matching_is_case_insensitive() {
        let upper = topic0s_of(IMPL_ABI)[0].to_uppercase().replace("0X", "0x");
        assert_eq!(
            fit_from_sample(&topic0s_of(IMPL_ABI), &[Some(&upper)]),
            AbiFit::Fits,
            "lowercase hex is a convention of eth_getLogs, not a guarantee"
        );
    }

    #[test]
    fn anonymous_events_never_match() {
        assert_eq!(
            fit_from_sample(&topic0s_of(IMPL_ABI), &[None]),
            AbiFit::Mismatch { sampled: 1 }
        );
    }

    /// #512: the tip probe must use the chain's own RPC-safe span, not a wider fixed guess. A
    /// hardcoded 1,000-block window is exactly what sent USDC's real probe through every default
    /// mainnet provider's result-size cap.
    #[test]
    fn tip_probe_window_matches_the_chains_log_window() {
        let tip = 25_745_042u64;
        let windows = probe_windows(tip, Some(6_082_465), 20);
        assert_eq!(windows[0], (tip - 20, tip));
    }

    /// Deep gap between deploy and tip: both probes stay within `probe` blocks wide, and never widen
    /// back out to some other constant regardless of how old the contract is.
    #[test]
    fn deploy_probe_window_is_also_bounded_by_log_window() {
        let tip = 25_745_042u64;
        let start = 6_082_465u64;
        let windows = probe_windows(tip, Some(start), 20);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[1], (start, start + 20));
    }

    /// A contract deployed within `2 * probe` of tip has overlapping windows - only the tip probe
    /// runs, so a young contract isn't probed twice for the same blocks.
    #[test]
    fn recently_deployed_contract_gets_one_window() {
        let tip = 25_745_042u64;
        let windows = probe_windows(tip, Some(tip - 10), 20);
        assert_eq!(windows.len(), 1);
    }

    // ---- --abi override -------------------------------------------------------------------------

    #[test]
    fn abi_overrides_align_positionally_and_allow_gaps() {
        assert_eq!(
            resolve_abi_overrides(&[], 3).unwrap(),
            vec![None, None, None]
        );
        assert_eq!(
            resolve_abi_overrides(&["".into(), "impl.json".into()], 2).unwrap(),
            vec![None, Some("impl.json".to_string())],
            "overriding one contract must not force you to find ABIs for the others"
        );
        assert!(
            resolve_abi_overrides(&["a.json".into()], 2).is_err(),
            "a count mismatch silently applying to the wrong address is the bug this prevents"
        );
    }

    #[test]
    fn a_local_abi_is_validated_and_artifact_wrappers_are_unwrapped() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain.json");
        std::fs::write(&plain, IMPL_ABI).unwrap();
        let got = read_local_abi(plain.to_str().unwrap()).unwrap();
        assert_eq!(abi_event_topic0s(&got).len(), 1);

        // A Hardhat/Foundry artifact wraps the ABI under "abi" - the file people reach for first.
        let wrapped = dir.path().join("artifact.json");
        std::fs::write(
            &wrapped,
            format!(r#"{{"contractName":"X","abi":{IMPL_ABI},"bytecode":"0x"}}"#),
        )
        .unwrap();
        assert_eq!(
            abi_event_topic0s(&read_local_abi(wrapped.to_str().unwrap()).unwrap()).len(),
            1
        );

        let junk = dir.path().join("junk.json");
        std::fs::write(&junk, r#"{"schema":"this is a subgraph manifest"}"#).unwrap();
        assert!(
            read_local_abi(junk.to_str().unwrap()).is_err(),
            "a wrong file must fail here, not silently become an empty registry"
        );
        assert!(read_local_abi(dir.path().join("nope.json").to_str().unwrap()).is_err());
    }

    /// **Issue #241 item 2.** A hand-written `nuthatch.toml` has no `schema.json`, and the schema tool
    /// then recommends `{col}_dec` companions that do not exist - `Binder Error: Referenced column
    /// "delta_dec" not found` for anyone (or any agent) that follows the advice.
    ///
    /// The advice comes from the live registry; the columns come from `schema.json`. They can only
    /// agree if the file is there and current, so startup makes it so.
    #[test]
    fn a_missing_schema_is_regenerated_not_merely_warned_about() {
        let dir = tempfile::tempdir().unwrap();
        write_hand_authored_nest(dir.path());
        assert!(!dir.path().join("schema.json").exists());

        let cfg = Config::load(dir.path()).unwrap();
        let what = refresh_stale_artifacts(dir.path(), &cfg).unwrap();
        assert!(
            what.as_deref().unwrap_or("").contains("was missing"),
            "it must say what it did and why: {what:?}"
        );
        assert!(
            dir.path().join("schema.json").exists(),
            "the file the _dec columns come from must now exist"
        );

        // Idempotent: a second call finds nothing to do, so `dev` does not rewrite artifacts on every
        // restart and churn anyone's git status.
        assert_eq!(refresh_stale_artifacts(dir.path(), &cfg).unwrap(), None);
    }

    /// A schema older than the config is the subtler half: `add` a contract by hand, and the file
    /// exists but describes the previous nest.
    #[test]
    fn a_schema_older_than_the_config_is_refreshed() {
        let dir = tempfile::tempdir().unwrap();
        write_hand_authored_nest(dir.path());
        let cfg = Config::load(dir.path()).unwrap();
        refresh_stale_artifacts(dir.path(), &cfg).unwrap();

        // Touch the config forward, which is what editing it does.
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
        let f = std::fs::File::options()
            .write(true)
            .open(dir.path().join(crate::config::CONFIG_FILE))
            .unwrap();
        f.set_modified(later).unwrap();

        let what = refresh_stale_artifacts(dir.path(), &cfg).unwrap();
        assert!(
            what.as_deref().unwrap_or("").contains("older than"),
            "a stale schema must be refreshed, not just a missing one: {what:?}"
        );
    }

    /// The regenerated schema must actually carry the big-int columns whose `_dec` companions the
    /// advice promises - otherwise the file exists and the advice is still wrong, which is the failure
    /// wearing a hat.
    #[test]
    fn the_regenerated_schema_carries_the_columns_the_advice_promises() {
        let dir = tempfile::tempdir().unwrap();
        write_hand_authored_nest(dir.path());
        let cfg = Config::load(dir.path()).unwrap();
        refresh_stale_artifacts(dir.path(), &cfg).unwrap();

        let raw = std::fs::read_to_string(dir.path().join("schema.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let cols: Vec<String> = v["tables"][0]["columns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap().to_string())
            .collect();
        assert!(
            cols.iter().any(|c| c == "value"),
            "the uint256 column must be described: {cols:?}"
        );
    }

    /// A nest written by hand: config + ABI, no derived artifacts. Exactly the POA case from #241.
    fn write_hand_authored_nest(dir: &Path) {
        std::fs::create_dir_all(dir.join("abis")).unwrap();
        std::fs::write(
            dir.join("abis/tok.json"),
            r#"[{"type":"event","name":"Transfer","anonymous":false,"inputs":[
                {"name":"from","type":"address","indexed":true},
                {"name":"to","type":"address","indexed":true},
                {"name":"value","type":"uint256","indexed":false}]}]"#,
        )
        .unwrap();
        std::fs::write(
            dir.join(crate::config::CONFIG_FILE),
            r#"
[nest]
name = "hand"
chain = "mainnet"
chain_id = 1
rpc_urls = ["https://rpc.example"]

[[contracts]]
alias = "tok"
address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
abi = "abis/tok.json"
"#,
        )
        .unwrap();
    }

    /// **Audit finding 6.** A `.git` suffix must not qualify a transport helper as a git source.
    ///
    /// `git clone 'ext::sh -c <cmd>'` executes the command. Before this, `ext::sh -c … .git` passed
    /// `is_git_source` and reached `git clone`, and was refused only by git's own
    /// `protocol.ext.allow=never` default. An operator who has set that to `always` - CI images do -
    /// would have had remote code execution from `init --from`.
    #[test]
    fn a_transport_helper_is_not_a_git_source() {
        for hostile in [
            "ext::sh -c touch /tmp/pwned .git",
            "ext::sh -c id",
            "transport-helper::whatever.git",
            "file::/etc/passwd.git",
        ] {
            assert!(
                !is_git_source(hostile),
                "must not be treated as a git source: {hostile}"
            );
        }
    }

    /// …and the real ones still are, or `init --from` stops working for everyone.
    #[test]
    fn ordinary_git_sources_still_qualify() {
        for ok in [
            "https://github.com/nightswatchhq/poa-nest",
            "https://github.com/nightswatchhq/poa-nest.git",
            "http://internal.example/nest.git",
            "ssh://git@github.com/x/y.git",
            "git@github.com:nightswatchhq/poa-nest.git",
            "git://legacy.example/x.git",
            "/srv/nests/mine.git",
        ] {
            assert!(is_git_source(ok), "must still be a git source: {ok}");
        }
        // A plain local directory is not a git source and must still load as a directory.
        assert!(!is_git_source("./my-nest"));
        assert!(!is_git_source("/srv/nests/mine"));
    }

    // ---- #535: init rejects chains the nest format supports ---------------------------------------
    //
    // `--from-subgraph` on a manifest for a chain outside the built-in three named a remedy
    // (`--chain <name> --rpc <url>`) that `chains::lookup` then refused. The round trip below is the
    // one the user actually took: fetch the manifest, hit the error, follow its own advice exactly as
    // printed, and expect it to work - not a test that pins whichever half of `init_from_subgraph` got
    // edited.

    /// A local HTTP server answering fixed paths with fixed bodies - stands in for the manifest URL
    /// (`--from-subgraph`, `Origin::Operator`, fetched directly) and an IPFS gateway (`--ipfs`,
    /// `Origin::Manifest`, `{gateway}{cid}`). Real HTTP, so `fetch_ipfs`'s actual request path runs.
    async fn fake_gateway(
        routes: Vec<(&'static str, &'static str)>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::{extract::State, routing::get, Router};
        use std::collections::BTreeMap;
        use std::sync::Arc;

        let routes: Arc<BTreeMap<&'static str, &'static str>> =
            Arc::new(routes.into_iter().collect());

        async fn handler(
            State(routes): State<Arc<BTreeMap<&'static str, &'static str>>>,
            uri: axum::http::Uri,
        ) -> Result<String, axum::http::StatusCode> {
            routes
                .get(uri.path())
                .map(|s| s.to_string())
                .ok_or(axum::http::StatusCode::NOT_FOUND)
        }

        let app = Router::new()
            .route("/{*rest}", get(handler))
            .with_state(routes);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), handle)
    }

    /// A one-endpoint fake JSON-RPC server answering `eth_chainId` - the round trip's `--rpc <url>`.
    async fn fake_chain_id_rpc(chain_id: u64) -> (String, tokio::task::JoinHandle<()>) {
        use axum::{extract::State, routing::post, Json, Router};
        use serde_json::{json, Value};

        async fn handler(State(chain_id): State<u64>, Json(_req): Json<Value>) -> Json<Value> {
            Json(json!({"jsonrpc": "2.0", "id": 1, "result": format!("0x{chain_id:x}")}))
        }

        let app = Router::new().route("/", post(handler)).with_state(chain_id);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), handle)
    }

    const BSC_MANIFEST: &str = r#"
specVersion: 0.0.5
dataSources:
  - kind: ethereum
    name: Pool
    network: bsc
    source:
      abi: Pool
      address: "0x0000000000000000000000000000000000000001"
    mapping:
      abis:
        - file:
            /: /ipfs/Qmco6j6G3fpC1VVoBFFYjTY6hvJxUxUrtaqgFCftA6RW4s
          name: Pool
      eventHandlers:
        - event: Transfer(indexed address,indexed address,uint256)
          handler: handleTransfer
"#;

    const POOL_ABI: &str = r#"[{"type":"event","name":"Transfer","anonymous":false,"inputs":[
        {"name":"from","type":"address","indexed":true},
        {"name":"to","type":"address","indexed":true},
        {"name":"value","type":"uint256","indexed":false}]}]"#;

    #[tokio::test]
    async fn from_subgraph_recommendation_is_followable_end_to_end() {
        // The CID is *derived from the content*, not invented. Before RFC-0037 slice 1 nothing
        // checked the two matched, so this fixture named a CID its body did not hash to - and the
        // test passed. It does not any more, which is the verification working.
        let pool_cid = crate::cid::cid_v0_for(POOL_ABI.as_bytes());
        // Leaked deliberately: `fake_gateway` wants `'static` paths and this is a test that ends.
        let pool_path: &'static str = Box::leak(format!("/ipfslike/{pool_cid}").into_boxed_str());
        // The manifest names the ABI by CID, so it has to name the *real* one too. It also has to
        // name a chain nuthatch genuinely does not ship: `bsc` became built-in when the top-25 list
        // added it, and this test is about the blind-then-remedy flow for an *unshipped* chain, which
        // stops being exercised the moment the chain is shipped.
        let manifest: &'static str = Box::leak(
            BSC_MANIFEST
                .replace("Qmco6j6G3fpC1VVoBFFYjTY6hvJxUxUrtaqgFCftA6RW4s", &pool_cid)
                .replace("bsc", "avalanche")
                .into_boxed_str(),
        );
        let (gateway, _gw) =
            fake_gateway(vec![("/manifest.yaml", manifest), (pool_path, POOL_ABI)]).await;
        let (rpc_url, _rpc) = fake_chain_id_rpc(43114).await;

        let dir = tempfile::tempdir().unwrap();
        let source = format!("{gateway}/manifest.yaml");
        let mut args = InitArgs {
            addresses: vec![],
            from: None,
            from_subgraph: Some(source.clone()),
            ipfs: vec![format!("{gateway}/ipfslike/")],
            alias: vec![],
            abi: vec![],
            chain: None,
            rpc: vec![],
            dir: dir.path().to_string_lossy().into_owned(),
            no_timestamps: false,
        };

        // Step 1: blind, this is the error the user actually hits - and the remedy it names.
        let err = init_from_subgraph(&source, &args).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains(
                "the manifest indexes 'avalanche', which nuthatch has no built-in chain for"
            ),
            "{msg}"
        );
        assert!(
            msg.contains("re-run with --chain <name> --rpc <url> to point at it yourself"),
            "{msg}"
        );

        // Step 2: follow that remedy exactly as printed - `--chain bsc --rpc <url>` - and it must work.
        args.chain = Some("avalanche".to_string());
        args.rpc = vec![rpc_url.clone()];
        init_from_subgraph(&source, &args)
            .await
            .expect("the recommended re-run must succeed - the nest format already supports it");

        let config = Config::load(dir.path()).unwrap();
        assert_eq!(config.nest.chain, "avalanche");
        assert_eq!(config.nest.chain_id, 43114);
        assert_eq!(config.nest.rpc_urls, vec![rpc_url]);
    }

    /// #675: a Sourcify resolve used to print through a raw `tracing::info!` line (ISO timestamp, log
    /// level, ANSI codes) crashing through the `→`/`✓` pretty output. It now prints only the tick.
    /// Covers `abi_resolved_lines`'s formatted wording only - not that `resolve_abi` calls it, so a
    /// regression that drops the call site entirely would still pass this test.
    #[test]
    fn abi_resolved_lines_sourcify_is_a_single_clean_tick() {
        let resolved = abi::Resolved {
            abi: serde_json::json!([]),
            via: "Sourcify",
            fallback_reason: None,
        };
        assert_eq!(
            abi_resolved_lines(&resolved),
            vec!["  ✓ ABI resolved via Sourcify".to_string()]
        );
    }

    #[test]
    fn abi_resolved_lines_names_the_keyless_fallback() {
        let resolved = abi::Resolved {
            abi: serde_json::json!([]),
            via: "Blockscout",
            fallback_reason: Some("Sourcify miss: no ABI".into()),
        };
        assert_eq!(
            abi_resolved_lines(&resolved),
            vec![
                "  · Sourcify miss: no ABI".to_string(),
                "  ✓ ABI resolved via Blockscout".to_string(),
            ]
        );
    }

    /// The Etherscan-fallback path (#675's "sweep the other paths" ask): the miss reason is real,
    /// non-redundant information, so it earns its own `·` line ahead of the `✓` tick rather than being
    /// dropped along with the redundant Sourcify-success announcement.
    #[test]
    fn abi_resolved_lines_etherscan_fallback_names_the_reason_then_the_tick() {
        let resolved = abi::Resolved {
            abi: serde_json::json!([]),
            via: "Etherscan",
            fallback_reason: Some(
                "Sourcify miss: Sourcify returned HTTP 404; Blockscout miss: no ABI".to_string(),
            ),
        };
        assert_eq!(
            abi_resolved_lines(&resolved),
            vec![
                "  · Sourcify miss: Sourcify returned HTTP 404; Blockscout miss: no ABI"
                    .to_string(),
                "  ✓ ABI resolved via Etherscan".to_string(),
            ]
        );
    }
}
