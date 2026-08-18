//! The decode registry (RFC-0001): ABI-driven, deterministic event decode for N contracts.
//!
//! The implementation lives in `nuthatch-decode` (a dependency-light crate that the fuzz
//! targets can link without pulling in `dbsp`). This module re-exports every public item
//! and adds `from_nest`, which requires `Config` and `blob::checked_join` from the main crate.

pub use nuthatch_decode::registry::*;

use alloy_json_abi::JsonAbi;
use anyhow::{anyhow, Context, Result};
use std::path::Path;

use crate::config::Config;

/// Build a [`DecodeRegistry`] from a nest's config: load each contract's vendored ABI
/// and register its events.
///
/// Left in the main crate because it depends on [`Config`] and
/// [`crate::blob::checked_join`]; `nuthatch-decode` has no config or blob dependency.
/// All callers within this crate reach it as `crate::registry::from_nest(dir, config)`.
pub fn from_nest(dir: &Path, config: &Config) -> Result<DecodeRegistry> {
    let mut specs = Vec::with_capacity(config.contracts.len());
    for c in &config.contracts {
        // Guard the ABI path against traversal/absolute-path escape (`abi = "/etc/shadow"` or
        // `abi = "../.."`): untrusted `.bundle`/config input must resolve inside the nest dir.
        let abi_path = crate::blob::checked_join(dir, &c.abi)?;
        let raw = std::fs::read_to_string(&abi_path)
            .with_context(|| format!("reading ABI {}", abi_path.display()))?;
        let abi: JsonAbi = serde_json::from_str(&raw)
            .with_context(|| format!("parsing ABI {}", abi_path.display()))?;
        specs.push(ContractSpec {
            alias: c.alias.clone(),
            address: parse_address_hex(&c.address)?,
            abi,
            events: c.events.clone(),
        });
    }
    // Template ABIs (RFC-0009): loaded the same way, keyed by template name (not an address).
    let mut templates = Vec::with_capacity(config.templates.len());
    for t in &config.templates {
        let abi_path = crate::blob::checked_join(dir, &t.abi)?;
        let raw = std::fs::read_to_string(&abi_path)
            .with_context(|| format!("reading template ABI {}", abi_path.display()))?;
        let abi: JsonAbi = serde_json::from_str(&raw)
            .with_context(|| format!("parsing template ABI {}", abi_path.display()))?;
        templates.push(TemplateSpec {
            name: t.name.clone(),
            abi,
            events: t.events.clone(),
        });
    }
    Ok(DecodeRegistry::build_with_templates(specs, templates)?
        .with_timestamps(config.nest.block_timestamps)
        .with_blocks(config.extract.blocks))
}

fn parse_address_hex(s: &str) -> Result<alloy_primitives::Address> {
    let bytes = hex::decode(s.trim_start_matches("0x")).context("bad address hex")?;
    if bytes.len() != 20 {
        return Err(anyhow!("address is not 20 bytes"));
    }
    Ok(alloy_primitives::Address::from_slice(&bytes))
}
