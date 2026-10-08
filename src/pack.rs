//! The compliance-pack manifest (RFC-0008 C6): a signed declaration of a nest's decode registry,
//! flag thresholds and alert sinks - the trust interface between an operator and its auditor.
//! `pack build` assembles it from the nest's config; `pack verify` checks the signature.

use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

pub const PACK_FILE: &str = "compliance-pack.toml";

/// The full manifest as written to `compliance-pack.toml`: the signed body plus an optional signature.
#[derive(Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub pack: Body,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<Signature>,
}

/// The signed portion. Field order is fixed (serde serialises structs in declaration order), so its
/// canonical JSON encoding - what we sign - is deterministic.
///
/// Unknown fields are refused: an older manifest can carry `screening` and `components`
/// entries this build no longer checks, and verifying it while ignoring them would overstate it.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Body {
    pub name: String,
    pub created: String,
    /// The decode-registry content hash - the data model the annotations were computed against.
    pub registry_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flags: Option<FlagsEntry>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alerts: Vec<AlertEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct FlagsEntry {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub threshold: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub velocity_amount: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub velocity_window: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AlertEntry {
    pub kinds: Vec<String>,
    pub url: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Signature {
    pub pubkey: String,
    pub sig: String,
}

/// An ed25519 keypair, stored in a local JSON file (no key service - the RFC's constraint).
#[derive(Serialize, Deserialize)]
struct KeyFile {
    secret: String,
    public: String,
}

/// `nuthatch pack keygen --out <file>` - generate a signing keypair into a local JSON file.
pub fn keygen(out: &Path) -> Result<()> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow!("OS randomness unavailable: {e}"))?;
    let sk = SigningKey::from_bytes(&seed);
    let kf = KeyFile {
        secret: hex::encode(sk.to_bytes()),
        public: hex::encode(sk.verifying_key().to_bytes()),
    };
    std::fs::write(out, serde_json::to_string_pretty(&kf)?)
        .with_context(|| format!("cannot write key file {}", out.display()))?;
    println!(
        "✓ wrote keypair to {} (public {})",
        out.display(),
        &kf.public[..16]
    );
    println!("  keep the secret safe; distribute the public key so auditors can verify your packs");
    Ok(())
}

fn load_signing_key(path: &Path) -> Result<SigningKey> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read key file {}", path.display()))?;
    let kf: KeyFile = serde_json::from_str(&raw).context("corrupt key file")?;
    let bytes: [u8; 32] = hex::decode(&kf.secret)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| anyhow!("key file secret is not 32 hex bytes"))?;
    Ok(SigningKey::from_bytes(&bytes))
}

/// The bytes we sign / verify: the body's canonical JSON. Deterministic because struct field order is
/// fixed and every value is a string/int/array (no float or map-ordering ambiguity).
fn signing_bytes(body: &Body) -> Result<Vec<u8>> {
    serde_json::to_vec(body).context("failed to canonicalise manifest body")
}

/// `nuthatch pack build [--key <file>]` - assemble the manifest from the nest's config, sign it if a
/// key is given, and write `compliance-pack.toml`.
pub fn build(dir: &Path, key: Option<&Path>, created: &str) -> Result<()> {
    let config = crate::config::Config::load(dir)?;
    let registry = crate::registry::from_nest(dir, &config)?;

    let flags = if config.flags.threshold.is_some()
        || config.flags.velocity_amount.is_some()
        || config.flags.velocity_window.is_some()
    {
        Some(FlagsEntry {
            threshold: config.flags.threshold.clone(),
            velocity_amount: config.flags.velocity_amount.clone(),
            velocity_window: config.flags.velocity_window,
        })
    } else {
        None
    };

    let alerts = config
        .alerts
        .iter()
        .map(|a| AlertEntry {
            kinds: a.kinds.clone(),
            url: a.url.clone(),
        })
        .collect();

    let body = Body {
        name: config.nest.name.clone(),
        created: created.to_string(),
        registry_hash: hex::encode(registry.hash()),
        flags,
        alerts,
    };

    let signature = match key {
        Some(k) => {
            let sk = load_signing_key(k)?;
            let sig = sk.sign(&signing_bytes(&body)?);
            Some(Signature {
                pubkey: hex::encode(sk.verifying_key().to_bytes()),
                sig: hex::encode(sig.to_bytes()),
            })
        }
        None => None,
    };

    let manifest = Manifest {
        pack: body,
        signature,
    };
    let toml = toml::to_string_pretty(&manifest).context("failed to serialise manifest")?;
    let path = dir.join(PACK_FILE);
    std::fs::write(&path, toml).with_context(|| format!("cannot write {}", path.display()))?;
    println!("✓ wrote {}", path.display());
    if key.is_some() {
        println!("  signed - auditors can `nuthatch pack verify` against your public key");
    } else {
        println!("  unsigned - pass --key <file> to sign (see `nuthatch pack keygen`)");
    }
    Ok(())
}

/// The outcome of `pack verify`, so it's testable without parsing stdout.
#[derive(Debug, Default)]
pub struct VerifyReport {
    pub signature: Option<bool>,
    pub problems: Vec<String>,
}

impl VerifyReport {
    pub fn ok(&self) -> bool {
        self.problems.is_empty() && self.signature != Some(false)
    }
}

/// `nuthatch pack verify` - check the signature over the manifest body. Returns a structured report.
pub fn verify(dir: &Path) -> Result<VerifyReport> {
    let path = dir.join(PACK_FILE);
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("no {} - run `nuthatch pack build` first", path.display()))?;
    let manifest: Manifest = toml::from_str(&raw).context("corrupt compliance-pack.toml")?;
    let mut report = VerifyReport::default();

    // 1. Signature (if present) over the canonical body.
    if let Some(sig) = &manifest.signature {
        report.signature = Some(check_signature(&manifest.pack, sig).unwrap_or(false));
        if report.signature == Some(false) {
            report.problems.push("signature does not verify".into());
        }
    }

    Ok(report)
}

fn check_signature(body: &Body, sig: &Signature) -> Result<bool> {
    let pk: [u8; 32] = hex::decode(&sig.pubkey)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| anyhow!("pubkey is not 32 hex bytes"))?;
    let sb: [u8; 64] = hex::decode(&sig.sig)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| anyhow!("signature is not 64 hex bytes"))?;
    let vk = VerifyingKey::from_bytes(&pk).context("invalid pubkey")?;
    let signature = ed25519_dalek::Signature::from_bytes(&sb);
    Ok(vk.verify(&signing_bytes(body)?, &signature).is_ok())
}

/// CLI entry: build/verify/keygen dispatch.
pub fn run(args: crate::cli::PackArgs, created: &str) -> Result<()> {
    match args.what {
        crate::cli::PackWhat::Keygen(a) => keygen(&PathBuf::from(&a.out)),
        crate::cli::PackWhat::Build(a) => build(
            &PathBuf::from(&a.dir),
            a.key.as_deref().map(Path::new),
            created,
        ),
        crate::cli::PackWhat::Verify(a) => {
            let report = verify(&PathBuf::from(&a.dir))?;
            match report.signature {
                Some(true) => println!("✓ signature verifies"),
                Some(false) => println!("✗ signature does NOT verify"),
                None => println!("· unsigned manifest (no signature to check)"),
            }
            if report.ok() {
                println!("PASS");
                Ok(())
            } else {
                bail!("pack verification FAILED")
            }
        }
    }
}

/// The sha256 of a file's bytes - the content address used throughout the pack.
#[allow(dead_code)]
pub fn file_hash(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body() -> Body {
        Body {
            name: "t".into(),
            created: "unix:1".into(),
            registry_hash: "abcd".into(),
            flags: None,
            alerts: vec![],
        }
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let mut seed = [7u8; 32];
        seed[0] = 42;
        let sk = SigningKey::from_bytes(&seed);
        let b = body();
        let sig = sk.sign(&signing_bytes(&b).unwrap());
        let signature = Signature {
            pubkey: hex::encode(sk.verifying_key().to_bytes()),
            sig: hex::encode(sig.to_bytes()),
        };
        assert!(
            check_signature(&b, &signature).unwrap(),
            "genuine signature verifies"
        );

        // Tamper with the body → signature no longer verifies.
        let mut tampered = body();
        tampered.registry_hash = "ffff".into();
        assert!(
            !check_signature(&tampered, &signature).unwrap(),
            "tampered body fails"
        );
    }

    #[test]
    fn build_then_verify_a_signed_pack() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(crate::config::CONFIG_FILE),
            r#"
[nest]
name = "audit-nest"
chain = "mainnet"
chain_id = 1
rpc_urls = ["https://rpc.example"]

[[contracts]]
alias = "usdc"
address = "0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48"
abi = "abis/usdc.json"

[flags]
threshold = "1000"
"#,
        )
        .unwrap();
        std::fs::create_dir_all(dir.path().join("abis")).unwrap();
        std::fs::write(
            dir.path().join("abis/usdc.json"),
            r#"[{"type":"event","name":"Transfer","anonymous":false,"inputs":[{"name":"from","type":"address","indexed":true},{"name":"to","type":"address","indexed":true},{"name":"value","type":"uint256","indexed":false}]}]"#,
        )
        .unwrap();

        let keyfile = dir.path().join("key.json");
        keygen(&keyfile).unwrap();
        build(dir.path(), Some(&keyfile), "unix:1").unwrap();

        let report = verify(dir.path()).unwrap();
        assert_eq!(report.signature, Some(true), "signed pack verifies");
        assert!(report.ok(), "problems: {:?}", report.problems);

        let path = dir.path().join(PACK_FILE);
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("\"1000\""), "{raw}");
        std::fs::write(&path, raw.replace("\"1000\"", "\"999\"")).unwrap();
        let report = verify(dir.path()).unwrap();
        assert_eq!(report.signature, Some(false), "an edited body fails");
        assert!(!report.ok());
    }

    #[test]
    fn a_manifest_with_screening_entries_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(PACK_FILE),
            r#"
[pack]
name = "t"
created = "unix:1"
registry_hash = "abcd"

[[pack.screening]]
list_snapshot = "deadbeef"
addresses = 3
"#,
        )
        .unwrap();
        let err = format!("{:#}", verify(dir.path()).unwrap_err());
        assert!(err.contains("screening"), "{err}");
    }
}
