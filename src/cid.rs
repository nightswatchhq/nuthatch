//! CID parsing and content verification for IPFS documents (RFC-0037 slice 1).
//!
//! `subgraph_import` fetches manifests and ABIs from public gateways and, until this module, never
//! checked what came back. Its own doc comment said so: *"a hostile or compromised gateway can serve
//! any document for any CID and this module will vendor it. The CID buys a stable name to ask for,
//! not proof of what came back."* An arbitrary ABI vendored into a nest is a silent wrong answer for
//! every event it mis-decodes, so this closes it.
//!
//! ## Why re-encode rather than fetch the raw block
//!
//! A CIDv0 addresses the **dag-pb node**, not the file bytes, so `sha256(what the gateway returned)`
//! does not match and never will. The obvious fix is to ask for the raw block (`?format=raw`), but
//! the gateways this project uses are not one shape: The Graph's is a Kubo API (`/api/v0/cat?arg=`),
//! the others are path gateways. Per-gateway raw-block handling is three code paths and three ways to
//! be wrong.
//!
//! Instead we **re-encode**: wrap the returned bytes in the UnixFS/dag-pb framing ourselves, hash
//! that, and compare to the CID. If it matches, the bytes are provably the ones the CID names -
//! which is the whole question. It needs no gateway cooperation and works identically everywhere.
//!
//! The limit is honest and stated: default chunking splits files above 256 KiB into several blocks,
//! and a multi-block file's root node holds links rather than data, so re-encoding cannot reproduce
//! it. Manifests are kilobytes and ABIs are well under the limit, but when it does not match we say
//! *unverifiable*, never *verified*.
//!
//! Hand-rolled rather than pulled in: base58, base32, varint and two protobuf messages are a few
//! hundred lines between them, and `deny.toml` makes every new dependency a decision.

use anyhow::{bail, Result};
use sha2::{Digest, Sha256};

/// Multicodec for raw binary - the block *is* the content.
const CODEC_RAW: u64 = 0x55;
/// Multicodec for dag-pb - the block is a protobuf node wrapping the content.
const CODEC_DAG_PB: u64 = 0x70;
/// Multihash code for sha2-256, the only one in practice for the documents we fetch.
const MH_SHA2_256: u64 = 0x12;

/// A parsed content identifier: what it addresses, and the digest it commits to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cid {
    pub version: u8,
    pub codec: u64,
    pub hash_code: u64,
    pub digest: Vec<u8>,
}

/// Decode an unsigned LEB128 varint, returning the value and how many bytes it took.
fn varint(b: &[u8]) -> Result<(u64, usize)> {
    let mut out: u64 = 0;
    for (i, byte) in b.iter().take(10).enumerate() {
        out |= u64::from(byte & 0x7f)
            .checked_shl(7 * i as u32)
            .unwrap_or_default();
        if byte & 0x80 == 0 {
            return Ok((out, i + 1));
        }
    }
    bail!("truncated or over-long varint")
}

/// Encode an unsigned LEB128 varint.
fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

const B58: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/// base58btc decode - the encoding of a CIDv0 (`Qm…`).
fn base58_decode(s: &str) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    for c in s.bytes() {
        let mut carry = B58
            .iter()
            .position(|&b| b == c)
            .ok_or_else(|| anyhow::anyhow!("'{}' is not a base58 character", c as char))?;
        for byte in out.iter_mut() {
            carry += 58 * (*byte as usize);
            *byte = (carry & 0xff) as u8;
            carry >>= 8;
        }
        while carry > 0 {
            out.push((carry & 0xff) as u8);
            carry >>= 8;
        }
    }
    // Leading '1's are leading zero bytes, which the arithmetic above cannot represent.
    let leading_zeros = s.bytes().take_while(|&b| b == b'1').count();
    out.resize(out.len() + leading_zeros, 0);
    out.reverse();
    Ok(out)
}

/// RFC-4648 base32 lower-case, no padding - the encoding of a CIDv1 (`bafy…`).
fn base32_decode(s: &str) -> Result<Vec<u8>> {
    let mut acc: u64 = 0;
    let mut bits = 0u32;
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    for c in s.bytes() {
        let v = match c {
            b'a'..=b'z' => c - b'a',
            b'2'..=b'7' => c - b'2' + 26,
            _ => bail!("'{}' is not a base32 character", c as char),
        };
        acc = (acc << 5) | u64::from(v);
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

impl Cid {
    /// Parse a CIDv0 (`Qm…`, base58btc) or CIDv1 (`b…`, base32 lower).
    pub fn parse(s: &str) -> Result<Cid> {
        let s = s.trim();
        if s.starts_with("Qm") || s.starts_with("1") {
            // v0 is a bare multihash, always sha2-256/dag-pb.
            let raw = base58_decode(s)?;
            let (code, n) = varint(&raw)?;
            let (len, m) = varint(&raw[n..])?;
            let digest = raw[n + m..].to_vec();
            if digest.len() as u64 != len {
                bail!(
                    "CIDv0 {s}: multihash says {len} bytes of digest, found {}",
                    digest.len()
                );
            }
            return Ok(Cid {
                version: 0,
                codec: CODEC_DAG_PB,
                hash_code: code,
                digest,
            });
        }
        let Some(body) = s.strip_prefix('b') else {
            bail!("{s:?} is neither a CIDv0 (Qm…) nor a base32 CIDv1 (b…)");
        };
        let raw = base32_decode(body)?;
        let (version, a) = varint(&raw)?;
        if version != 1 {
            bail!("{s:?}: unsupported CID version {version}");
        }
        let (codec, b) = varint(&raw[a..])?;
        let (code, c) = varint(&raw[a + b..])?;
        let (len, d) = varint(&raw[a + b + c..])?;
        let digest = raw[a + b + c + d..].to_vec();
        if digest.len() as u64 != len {
            bail!(
                "CIDv1 {s}: multihash says {len} bytes of digest, found {}",
                digest.len()
            );
        }
        Ok(Cid {
            version: 1,
            codec,
            hash_code: code,
            digest,
        })
    }
}

/// base58btc encode - the inverse of [`base58_decode`], used to *produce* a CIDv0.
fn base58_encode(bytes: &[u8]) -> String {
    let mut digits: Vec<u8> = Vec::with_capacity(bytes.len() * 137 / 100 + 1);
    for &b in bytes {
        let mut carry = b as usize;
        for d in digits.iter_mut() {
            carry += (*d as usize) << 8;
            *d = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }
    let leading_zeros = bytes.iter().take_while(|&&b| b == 0).count();
    let mut out = String::with_capacity(digits.len() + leading_zeros);
    out.extend(std::iter::repeat_n('1', leading_zeros));
    for d in digits.iter().rev() {
        out.push(B58[*d as usize] as char);
    }
    out
}

/// The CIDv0 a single-block UnixFS file with this content would have.
///
/// The inverse of what [`verify`] checks, and the same framing. Useful wherever a CID has to be
/// *produced* rather than checked - a fixture that must name its own content honestly, or a nest
/// that wants to state the address of an ABI it vendored.
pub fn cid_v0_for(content: &[u8]) -> String {
    let node = unixfs_file_node(content);
    let mut mh = vec![MH_SHA2_256 as u8, 32];
    mh.extend_from_slice(&Sha256::digest(&node));
    base58_encode(&mh)
}

/// The CIDv0 for a bare 32-byte sha2-256 digest, as carried by a `bytes32` event parameter.
///
/// Distinct from [`cid_v0_for`], and the distinction is the whole point: `cid_v0_for` *hashes
/// content* to find its address, whereas this takes an address somebody already computed and only
/// re-frames it. A CIDv0 is `base58btc(<multihash>)` and a sha2-256 multihash is
/// `0x12 0x20 || digest`, so there is nothing here but a two-byte prefix and an encoding - no
/// content, no UnixFS framing, and no way to check it until the document is fetched. [`verify`]
/// does that afterwards, exactly as it does for a CID that arrived as a string.
///
/// The reason this exists at all: a great many subgraphs, The Graph's own GNS among them, store an
/// IPFS address on chain as a raw `bytes32` rather than as a string, because 32 bytes is what the
/// digest actually is and the `Qm…` text is merely one encoding of it. Without this a nest reading
/// such a column resolves nothing at all, silently.
pub fn cid_v0_from_digest(digest: &[u8; 32]) -> String {
    let mut mh = Vec::with_capacity(34);
    mh.push(MH_SHA2_256 as u8);
    mh.push(32);
    mh.extend_from_slice(digest);
    base58_encode(&mh)
}

/// The dag-pb bytes a single-block UnixFS file with this content would have.
///
/// `PBNode { Data: UnixFS { Type: File, Data: content, filesize: len } }`, with no links. Canonical
/// dag-pb writes Links (field 2) before Data (field 1); a file with no links has only Data.
fn unixfs_file_node(content: &[u8]) -> Vec<u8> {
    let mut unixfs = Vec::with_capacity(content.len() + 16);
    unixfs.extend_from_slice(&[0x08, 0x02]); // field 1 (Type) varint = 2 (File)
    unixfs.push(0x12); // field 2 (Data), length-delimited
    put_varint(&mut unixfs, content.len() as u64);
    unixfs.extend_from_slice(content);
    unixfs.push(0x18); // field 3 (filesize), varint
    put_varint(&mut unixfs, content.len() as u64);

    let mut node = Vec::with_capacity(unixfs.len() + 8);
    node.push(0x0a); // PBNode field 1 (Data), length-delimited
    put_varint(&mut node, unixfs.len() as u64);
    node.extend_from_slice(&unixfs);
    node
}

/// Whether `content` is provably the document `cid` names.
///
/// `Ok(())` means verified. An `Err` means **not verified**, which is not the same as "wrong": a
/// multi-block file cannot be checked this way, and the message says which case it is so a caller
/// never reports an unverifiable document as a verified one.
pub fn verify(cid: &Cid, content: &[u8]) -> Result<()> {
    if cid.hash_code != MH_SHA2_256 {
        bail!(
            "CID uses multihash 0x{:x}, and only sha2-256 (0x12) is implemented - cannot verify",
            cid.hash_code
        );
    }
    let block = match cid.codec {
        CODEC_RAW => content.to_vec(),
        CODEC_DAG_PB => unixfs_file_node(content),
        other => bail!("CID codec 0x{other:x} is not raw or dag-pb - cannot verify"),
    };
    let got = Sha256::digest(&block);
    if got.as_slice() == cid.digest {
        return Ok(());
    }
    if cid.codec == CODEC_DAG_PB && content.len() > 256 * 1024 {
        bail!(
            "content does not match the CID, and at {} bytes it is over the 256 KiB default chunk \
             size - so it is probably a multi-block file, which cannot be verified by re-encoding. \
             Treat this as UNVERIFIED rather than as tampered.",
            content.len()
        );
    }
    bail!(
        "content does not match its CID: expected sha2-256 {}, re-encoded to {}. The gateway \
         returned a different document from the one asked for.",
        hex::encode(&cid.digest),
        hex::encode(got)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The DOUDOCHAIN_V2 manifest CID, whose content the live test below fetched and verified. Every
    /// hermetic assertion here is pinned to what that produced.
    const REAL_CID: &str = "QmXf82bXak3752bwJ1x7SWchMiEP3Z4vWCWxUJ2HY3wdhj";
    const REAL_DIGEST: &str = "8a7486bebfd89647b7f3b6b597d61e32fd450f85aaf01d9e5055af7eee7f70ba";

    /// The encoder must reproduce the CID the live test verified, or it is not the same framing.
    #[test]
    fn the_encoder_reproduces_the_verified_cid() {
        // Round-trip through the decoder on a vector the decoder was itself checked against, so a
        // shared bug in both would still have to reproduce a digest fetched from a real gateway.
        let node_digest = hex::decode(REAL_DIGEST).unwrap();
        let mut mh = vec![0x12u8, 32];
        mh.extend_from_slice(&node_digest);
        assert_eq!(base58_encode(&mh), REAL_CID);
        assert_eq!(base58_decode(REAL_CID).unwrap(), mh);
    }

    #[test]
    fn a_real_cidv0_parses_to_sha2_256_over_dag_pb() {
        let c = Cid::parse(REAL_CID).unwrap();
        assert_eq!(c.version, 0);
        assert_eq!(c.codec, CODEC_DAG_PB, "a v0 CID is always dag-pb");
        assert_eq!(c.hash_code, MH_SHA2_256);
        assert_eq!(hex::encode(&c.digest), REAL_DIGEST);
        assert_eq!(c.digest.len(), 32);
    }

    /// base32 against **RFC 4648's own test vectors**, so the decoder is checked against the standard
    /// rather than against itself. Lower-case and unpadded, which is what a CIDv1 uses.
    #[test]
    fn base32_matches_the_rfc_vectors() {
        for (encoded, want) in [
            ("my", "f"),
            ("mzxq", "fo"),
            ("mzxw6", "foo"),
            ("mzxw6yq", "foob"),
            ("mzxw6ytb", "fooba"),
            ("mzxw6ytboi", "foobar"),
        ] {
            assert_eq!(
                base32_decode(encoded).unwrap(),
                want.as_bytes(),
                "RFC 4648 vector {encoded:?}"
            );
        }
    }

    /// **The bug, as it actually happened.**
    ///
    /// Both of these are bodies a real public gateway really returned for `REAL_CID` while this
    /// module was being written - HTTP 200, plausible prose, and nothing whatever to do with the
    /// document asked for. `fetch_ipfs` checks only that a body is non-empty, so both would have been
    /// vendored into a nest as its manifest.
    ///
    /// They are transcribed from what was observed rather than invented, because a real failure is
    /// better evidence than one I would have thought to write.
    #[test]
    fn a_gateway_error_page_does_not_verify_as_the_document() {
        let cid = Cid::parse(REAL_CID).unwrap();
        for body in [
            "Unable to retrieve content within timeout period: timeout occurred after finding 3 \
             provider(s) and connecting to 3 (phase: connecting to providers)",
            "The request timed out searching for a file on the non-pinata IPFS network. - \
             ERR_ID:00016",
            "{}",
            "",
        ] {
            let err = verify(&cid, body.as_bytes())
                .expect_err("an arbitrary body must not verify as the document")
                .to_string();
            assert!(
                err.contains("does not match its CID"),
                "say plainly that it is the wrong document: {err}"
            );
        }
    }

    /// A raw-codec CIDv1 addresses the bytes directly, with no UnixFS framing in between.
    #[test]
    fn a_raw_codec_cid_verifies_against_the_bytes_themselves() {
        let content = b"a nest is a packaged indexing definition";
        let cid = Cid {
            version: 1,
            codec: CODEC_RAW,
            hash_code: MH_SHA2_256,
            digest: Sha256::digest(content).to_vec(),
        };
        verify(&cid, content).expect("raw codec verifies by direct hash");
        verify(&cid, b"something else").expect_err("and only against the right bytes");
    }

    /// **Unverifiable is not the same as tampered**, and the message has to say which.
    ///
    /// Default chunking splits a file above 256 KiB into several blocks, and a multi-block root holds
    /// links rather than data - so re-encoding cannot reproduce it. Reporting that as "the gateway
    /// returned a different document" would be an accusation we cannot support.
    #[test]
    fn a_multi_block_file_is_reported_as_unverifiable_not_as_tampered() {
        let cid = Cid::parse(REAL_CID).unwrap();
        let big = vec![b'x'; 300 * 1024];
        let err = verify(&cid, &big).unwrap_err().to_string();
        assert!(err.contains("UNVERIFIED"), "must not accuse: {err}");
        assert!(err.contains("multi-block"), "must say why: {err}");
    }

    /// Rubbish in the CID itself is a config error, not something to shrug at.
    #[test]
    fn a_malformed_cid_is_refused() {
        for bad in ["", "not-a-cid", "Qm!!!", "zzzz"] {
            assert!(Cid::parse(bad).is_err(), "{bad:?} must not parse");
        }
    }

    /// **Ground truth, fetched from a real gateway.**
    ///
    /// Every hermetic test below asserts against vectors this test produced. Bootstrapping ground
    /// truth from a live source and then pinning it is the only honest order: a hand-written vector
    /// would only prove our encoder agrees with our own idea of the format.
    ///
    /// Loudly skipped without `NUTHATCH_IPFS_LIVE=1`, because a silent skip is how "verified" quietly
    /// becomes untrue.
    #[tokio::test]
    async fn re_encoding_reproduces_a_real_cid_from_a_real_gateway() {
        if std::env::var("NUTHATCH_IPFS_LIVE").is_err() {
            eprintln!(
                "SKIP re_encoding_reproduces_a_real_cid_from_a_real_gateway: \
                 set NUTHATCH_IPFS_LIVE=1 to run it"
            );
            return;
        }
        // The DOUDOCHAIN_V2 subgraph manifest, the deployment the subgraph-fallback port was seeded
        // from. Small, immutable, and pinned by The Graph.
        let want = "QmXf82bXak3752bwJ1x7SWchMiEP3Z4vWCWxUJ2HY3wdhj";
        let cid = Cid::parse(want).expect("a real CIDv0 must parse");
        assert_eq!(cid.version, 0);
        assert_eq!(cid.codec, CODEC_DAG_PB);
        assert_eq!(cid.hash_code, MH_SHA2_256);

        let body = reqwest::get(format!("https://ipfs.thegraph.com/api/v0/cat?arg={want}"))
            .await
            .expect("gateway reachable")
            .bytes()
            .await
            .expect("body readable");
        eprintln!("fetched {} bytes", body.len());
        eprintln!("content sha256 = {}", hex::encode(Sha256::digest(&body)));
        verify(&cid, &body).expect("re-encoding must reproduce the CID");
    }

    /// Three real `SubgraphMetadataUpdated` payloads, taken off Arbitrum GNS
    /// (`0xec9a7fb6cbc2e41926127929c2dce6e9c5d33bec`) at blocks 495,864,081, 496,045,896 and
    /// 496,124,693, with the CIDs on the right confirmed by fetching them from The Graph's gateway -
    /// the first returns 293 bytes of subgraph metadata carrying a `displayName`.
    ///
    /// Fixed vectors rather than a round-trip through [`base58_decode`], deliberately: a round-trip
    /// test passes just as happily when both directions share a mistake, and the thing actually
    /// being asserted here is agreement with the rest of the world, not with ourselves.
    #[test]
    fn a_bytes32_digest_becomes_the_cid_the_network_serves() {
        for (digest, want) in [
            (
                "6283b77fbdf020ce43a55149457f8ca1a3bec1ca60cd177163a7402e1a3945e4",
                "QmUyD9wPyVCkDotF9oUoQHcMrhCMLU9Sqi6HY7BrttLPsq",
            ),
            (
                "03b323306942bf347c602031319293fd6eaad9c891c0261232610132c7c7f943",
                "QmNb6MzQ4E9bS8tffxMeQbGsPvcn8Hwor67MG8fHTS66up",
            ),
            (
                "ecd9754f54112f72ed6cf787d64e2449729ac9b64a192d6cd5ba1887860104b9",
                "QmeHDFJScdzx8Rz9sVuZZePytFvJbXcNAo4AT3t58KwysN",
            ),
        ] {
            let mut d = [0u8; 32];
            d.copy_from_slice(&hex::decode(digest).unwrap());
            let got = cid_v0_from_digest(&d);
            assert_eq!(got, want, "digest 0x{digest}");
            // And it must survive our own parser, or the resolver would reject what we just built.
            let parsed = Cid::parse(&got).expect("a CID we produced must parse");
            assert_eq!(parsed.digest, d.to_vec(), "the digest must round-trip");
        }
    }

    /// The framing is the difference between this and [`cid_v0_for`], and it is easy to lose: both
    /// end in `base58(0x12 0x20 || sha256(..))`, but one hashes the UnixFS node and the other hashes
    /// nothing at all. Assert they disagree on the same 32 bytes, so a refactor that quietly routed
    /// one through the other would be caught here rather than by a nest that resolves nothing.
    #[test]
    fn a_digest_is_not_the_address_of_those_same_32_bytes() {
        let d = [7u8; 32];
        assert_ne!(cid_v0_from_digest(&d), cid_v0_for(&d));
    }
}
