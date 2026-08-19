//! Re-exports from `nuthatch-decode` plus the config-aware constructor `from_nest`, which lives
//! here because it depends on `Config` and `blob::checked_join` from the main crate.
//!
//! Everything else (the types, `build`, `build_with_templates`, all decode logic) lives in
//! `nuthatch-decode` so fuzz targets can build without pulling in dbsp (nuthatch#581).

pub use nuthatch_decode::registry::*;

use crate::config::Config;
use alloy_json_abi::JsonAbi;
use alloy_primitives::Address;
use anyhow::{anyhow, Context, Result};
use std::path::Path;

fn parse_address_local(s: &str) -> Result<Address> {
    let bytes = hex::decode(s.trim_start_matches("0x")).context("bad address hex")?;
    if bytes.len() != 20 {
        return Err(anyhow!("address is not 20 bytes"));
    }
    Ok(Address::from_slice(&bytes))
}

/// Build from a nest's config: load each contract's vendored ABI and register its events.
///
/// Lives here rather than in `nuthatch-decode` because it depends on [`Config`] and
/// [`crate::blob::checked_join`], both of which are main-crate concerns. The decode logic
/// itself (all pure, no dbsp, no I/O) is in `nuthatch-decode::registry`.
pub fn from_nest(dir: &Path, config: &Config) -> Result<DecodeRegistry> {
    let mut specs = Vec::with_capacity(config.contracts.len());
    for c in &config.contracts {
        let abi_path = crate::blob::checked_join(dir, &c.abi)?;
        let raw = std::fs::read_to_string(&abi_path)
            .with_context(|| format!("reading ABI {}", abi_path.display()))?;
        let abi: JsonAbi = serde_json::from_str(&raw)
            .with_context(|| format!("parsing ABI {}", abi_path.display()))?;
        specs.push(ContractSpec {
            alias: c.alias.clone(),
            address: parse_address_local(&c.address)?,
            abi,
            events: c.events.clone(),
        });
    }
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
