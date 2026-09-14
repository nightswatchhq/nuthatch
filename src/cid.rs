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
//! A file above 256 KiB is several blocks under a root that holds links rather than data, so it is
//! re-encoded the way Kubo imports by default: fixed 256 KiB chunks, a balanced tree of at most 174
//! links. Every one of 4,025 QoS oracle payloads measured on 2026-09-13 verifies that way. A file
//! built any other way cannot be re-encoded, and is proven instead from its blocks
//! ([`content_from_car`]), each checked against the CID that names it; when neither proves it we say
//! *unverifiable*, never *verified*.
//!
//! Hand-rolled rather than pulled in: base58, base32, varint and two protobuf messages are a few
//! hundred lines between them, and `deny.toml` makes every new dependency a decision.

use anyhow::{anyhow, bail, Result};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

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
    if !content.is_empty() {
        // Kubo omits an empty Data field, and the empty file's well-known CID depends on it.
        unixfs.push(0x12); // field 2 (Data), length-delimited
        put_varint(&mut unixfs, content.len() as u64);
        unixfs.extend_from_slice(content);
    }
    unixfs.push(0x18); // field 3 (filesize), varint
    put_varint(&mut unixfs, content.len() as u64);

    let mut node = Vec::with_capacity(unixfs.len() + 8);
    node.push(0x0a); // PBNode field 1 (Data), length-delimited
    put_varint(&mut node, unixfs.len() as u64);
    node.extend_from_slice(&unixfs);
    node
}

/// Kubo's import defaults, which every QoS oracle payload measured on 2026-09-13 was built with.
pub const DEFAULT_CHUNK: usize = 256 * 1024;
pub const DEFAULT_MAX_LINKS: usize = 174;

/// A document whose bytes may be right but cannot be proven by re-encoding. Not an accusation.
#[derive(Debug)]
pub struct Unprovable(pub String);

/// A document or DAG past [`Caps`]. Counted apart, because it describes the CID's author rather than
/// the gateway.
#[derive(Debug)]
pub struct OverCap(pub String);

impl std::fmt::Display for Unprovable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Unprovable {}
impl std::fmt::Display for OverCap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for OverCap {}

/// Whether `content` is provably the document `cid` names.
///
/// `Ok(())` means verified. An [`Unprovable`] error means **not verified**, which is not the same as
/// "wrong": a file imported with a non-default layout cannot be re-encoded, and only its blocks can
/// prove it. Any other error means the bytes are not the document *as re-encoded*: a dag-pb body under
/// one default chunk may still be a file cut into smaller blocks, which is why
/// [`fetch_ipfs_proven`](crate::subgraph_import::fetch_ipfs_proven) asks for the blocks after any
/// mismatch and refuses only when they do not prove it either.
pub fn verify(cid: &Cid, content: &[u8]) -> Result<()> {
    if cid.hash_code != MH_SHA2_256 {
        bail!(
            "CID uses multihash 0x{:x}, and only sha2-256 (0x12) is implemented - cannot verify",
            cid.hash_code
        );
    }
    let matches = match cid.codec {
        CODEC_RAW => Sha256::digest(content).as_slice() == cid.digest,
        CODEC_DAG_PB if content.len() <= DEFAULT_CHUNK => {
            Sha256::digest(unixfs_file_node(content)).as_slice() == cid.digest
        }
        CODEC_DAG_PB => {
            // CIDv0 can only link dag-pb leaves; `ipfs add --cid-version 1` defaults to raw ones.
            let layouts: &[Leaves] = if cid.version == 0 {
                &[Leaves::DagPb]
            } else {
                &[Leaves::Raw, Leaves::DagPb]
            };
            layouts.iter().any(|&leaves| {
                let root = balanced_dag(
                    content,
                    cid.version,
                    leaves,
                    DEFAULT_CHUNK,
                    DEFAULT_MAX_LINKS,
                    &mut |_, _| {},
                );
                root.digest() == cid.digest.as_slice()
            })
        }
        other => bail!("CID codec 0x{other:x} is not raw or dag-pb - cannot verify"),
    };
    if matches {
        return Ok(());
    }
    if cid.codec == CODEC_DAG_PB && content.len() > DEFAULT_CHUNK {
        return Err(Unprovable(format!(
            "{} bytes do not re-encode to the CID as a multi-block file in Kubo's default layout \
             (256 KiB chunks, 174 links). It may have been imported another way, which only its \
             blocks can prove. Treat this as UNVERIFIED rather than as tampered.",
            content.len()
        ))
        .into());
    }
    bail!(
        "content does not re-encode to its CID: expected sha2-256 {}. Either the gateway returned a \
         different document, or the file was cut into smaller blocks than one default chunk, which \
         only its blocks can prove.",
        hex::encode(&cid.digest)
    )
}

#[derive(Clone, Copy)]
enum Leaves {
    DagPb,
    Raw,
}

/// A child as its parent records it: the address it is linked by, the encoded size of everything
/// beneath it (`Tsize`), and the file bytes it holds (`blocksizes`).
struct Child {
    link: Vec<u8>,
    tsize: u64,
    filesize: u64,
}

impl Child {
    fn digest(&self) -> &[u8] {
        &self.link[self.link.len() - 32..]
    }
}

/// The address a `PBLink.Hash` holds: a bare multihash under CIDv0, a binary CIDv1 otherwise.
fn link_bytes(version: u8, codec: u64, digest: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(digest.len() + 4);
    if version == 1 {
        put_varint(&mut out, 1);
        put_varint(&mut out, codec);
    }
    out.push(MH_SHA2_256 as u8);
    out.push(digest.len() as u8);
    out.extend_from_slice(digest);
    out
}

fn put_bytes_field(out: &mut Vec<u8>, key: u8, bytes: &[u8]) {
    out.push(key);
    put_varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// A UnixFS file node over `children`: links first, then Data, as canonical dag-pb orders them.
/// Every link carries an empty `Name`, because Kubo writes one and the hash covers it.
fn unixfs_branch_node(children: &[Child]) -> Vec<u8> {
    let mut node = Vec::new();
    for c in children {
        let mut link = Vec::with_capacity(c.link.len() + 12);
        put_bytes_field(&mut link, 0x0a, &c.link); // PBLink.Hash
        put_bytes_field(&mut link, 0x12, &[]); // PBLink.Name
        link.push(0x18); // PBLink.Tsize
        put_varint(&mut link, c.tsize);
        put_bytes_field(&mut node, 0x12, &link); // PBNode.Links
    }
    let mut unixfs = vec![0x08, 0x02, 0x18]; // Type = File, then filesize
    put_varint(&mut unixfs, children.iter().map(|c| c.filesize).sum());
    for c in children {
        unixfs.push(0x20); // blocksizes: repeated, unpacked
        put_varint(&mut unixfs, c.filesize);
    }
    put_bytes_field(&mut node, 0x0a, &unixfs); // PBNode.Data
    node
}

/// `content` imported as a balanced DAG, handing every block to `sink` and returning the root.
///
/// Bottom-up grouping yields the same tree as Kubo's balanced builder: every subtree full except the
/// last. The caller guarantees more than one chunk, or the root would wrap a lone leaf.
fn balanced_dag(
    content: &[u8],
    version: u8,
    leaves: Leaves,
    chunk: usize,
    max_links: usize,
    sink: &mut dyn FnMut(&[u8], Vec<u8>),
) -> Child {
    let mut level: Vec<Child> = content
        .chunks(chunk)
        .map(|piece| {
            let (codec, block) = match leaves {
                Leaves::DagPb => (CODEC_DAG_PB, unixfs_file_node(piece)),
                Leaves::Raw => (CODEC_RAW, piece.to_vec()),
            };
            let child = Child {
                link: link_bytes(version, codec, &Sha256::digest(&block)),
                tsize: block.len() as u64,
                filesize: piece.len() as u64,
            };
            sink(&child.link, block);
            child
        })
        .collect();
    loop {
        let mut parents: Vec<Child> = level
            .chunks(max_links)
            .map(|kids| {
                let block = unixfs_branch_node(kids);
                let child = Child {
                    link: link_bytes(version, CODEC_DAG_PB, &Sha256::digest(&block)),
                    tsize: block.len() as u64 + kids.iter().map(|k| k.tsize).sum::<u64>(),
                    filesize: kids.iter().map(|k| k.filesize).sum(),
                };
                sink(&child.link, block);
                child
            })
            .collect();
        if parents.len() == 1 {
            return parents.remove(0);
        }
        level = parents;
    }
}

/// What a trustless fetch may cost before it is refused. Visits bound the work as well as the blocks,
/// because one small block can link the same child thousands of times.
pub struct Caps {
    pub max_blocks: usize,
    pub max_bytes: usize,
    pub max_depth: usize,
}

pub const CAPS: Caps = Caps {
    max_blocks: 4_096,
    max_bytes: 16 * 1024 * 1024,
    max_depth: 16,
};

enum Field<'a> {
    Varint(u64),
    Bytes(&'a [u8]),
}

/// The fields of one protobuf message. Only the two wire types dag-pb and UnixFS use are accepted.
fn pb_fields(b: &[u8]) -> Result<Vec<(u64, Field<'_>)>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let (key, n) = varint(&b[i..])?;
        i += n;
        match key & 7 {
            0 => {
                let (v, n) = varint(&b[i..])?;
                i += n;
                out.push((key >> 3, Field::Varint(v)));
            }
            2 => {
                let (len, n) = varint(&b[i..])?;
                i += n;
                let end = usize::try_from(len)
                    .ok()
                    .and_then(|len| i.checked_add(len))
                    .filter(|&end| end <= b.len())
                    .ok_or_else(|| anyhow!("protobuf field runs past the end of its block"))?;
                out.push((key >> 3, Field::Bytes(&b[i..end])));
                i = end;
            }
            wire => bail!("protobuf wire type {wire} does not occur in dag-pb or UnixFS"),
        }
    }
    Ok(out)
}

/// A binary CID, as a CAR section or a `PBLink.Hash` carries it, and how many bytes it took.
fn cid_from_bytes(b: &[u8]) -> Result<(Cid, usize)> {
    if b.first() == Some(&(MH_SHA2_256 as u8)) && b.get(1) == Some(&32) {
        let digest = b.get(2..34).ok_or_else(|| anyhow!("truncated CIDv0"))?;
        let cid = Cid {
            version: 0,
            codec: CODEC_DAG_PB,
            hash_code: MH_SHA2_256,
            digest: digest.to_vec(),
        };
        return Ok((cid, 34));
    }
    let (version, a) = varint(b)?;
    if version != 1 {
        bail!("unsupported binary CID version {version}");
    }
    let (codec, c) = varint(&b[a..])?;
    let (hash_code, d) = varint(&b[a + c..])?;
    let (len, e) = varint(&b[a + c + d..])?;
    let start = a + c + d + e;
    let end = usize::try_from(len)
        .ok()
        .and_then(|len| start.checked_add(len))
        .ok_or_else(|| anyhow!("binary CID digest length overflows"))?;
    let digest = b
        .get(start..end)
        .ok_or_else(|| anyhow!("truncated binary CID"))?;
    let cid = Cid {
        version: 1,
        codec,
        hash_code,
        digest: digest.to_vec(),
    };
    Ok((cid, end))
}

/// The file a CARv1 carries under `root`, with every block checked against the CID that names it.
///
/// The walk starts from the root we asked for, never from the roots the CAR's header claims, so a
/// gateway cannot substitute another DAG by naming it there.
pub fn content_from_car(root: &Cid, car: &[u8], caps: &Caps) -> Result<Vec<u8>> {
    let (header_len, n) = varint(car)?;
    let mut i = usize::try_from(header_len)
        .ok()
        .and_then(|len| n.checked_add(len))
        .filter(|&end| end <= car.len())
        .ok_or_else(|| anyhow!("CAR header runs past the end of the body"))?;
    let mut blocks: HashMap<(u64, Vec<u8>), &[u8]> = HashMap::new();
    while i < car.len() {
        let (len, n) = varint(&car[i..])?;
        let start = i + n;
        let end = usize::try_from(len)
            .ok()
            .and_then(|len| start.checked_add(len))
            .filter(|&end| end <= car.len())
            .ok_or_else(|| anyhow!("CAR section runs past the end of the body"))?;
        let (cid, used) = cid_from_bytes(&car[start..end])?;
        let block = &car[start + used..end];
        if cid.hash_code != MH_SHA2_256 {
            bail!(
                "CAR block uses multihash 0x{:x}; only sha2-256 is implemented",
                cid.hash_code
            );
        }
        if Sha256::digest(block).as_slice() != cid.digest {
            bail!(
                "CAR block {} does not hash to the CID that names it - the gateway sent a \
                 different block",
                hex::encode(&cid.digest)
            );
        }
        if blocks.len() >= caps.max_blocks {
            return Err(OverCap(format!(
                "the CAR holds more than {} blocks",
                caps.max_blocks
            ))
            .into());
        }
        blocks.insert((cid.codec, cid.digest), block);
        i = end;
    }
    let mut out = Vec::new();
    let mut visits = 0;
    walk(root, &blocks, caps, 0, &mut visits, &mut out)?;
    Ok(out)
}

/// A file imported as a balanced CIDv0 DAG with `chunk`-byte leaves, as its CID and a CAR of its
/// blocks, for tests that need a layout other than Kubo's default.
#[cfg(test)]
pub(crate) fn dag_for_tests(content: &[u8], chunk: usize) -> (String, Vec<u8>) {
    let mut car = Vec::new();
    put_varint(&mut car, 8);
    car.extend_from_slice(b"not read");
    let root = balanced_dag(
        content,
        0,
        Leaves::DagPb,
        chunk,
        DEFAULT_MAX_LINKS,
        &mut |link, block| {
            put_varint(&mut car, (link.len() + block.len()) as u64);
            car.extend_from_slice(link);
            car.extend_from_slice(&block);
        },
    );
    let mut digest = [0u8; 32];
    digest.copy_from_slice(root.digest());
    (cid_v0_from_digest(&digest), car)
}

fn walk(
    cid: &Cid,
    blocks: &HashMap<(u64, Vec<u8>), &[u8]>,
    caps: &Caps,
    depth: usize,
    visits: &mut usize,
    out: &mut Vec<u8>,
) -> Result<()> {
    *visits += 1;
    if *visits > caps.max_blocks {
        return Err(OverCap(format!(
            "walking the DAG took more than {} block visits",
            caps.max_blocks
        ))
        .into());
    }
    if depth > caps.max_depth {
        return Err(OverCap(format!("the DAG is deeper than {} levels", caps.max_depth)).into());
    }
    let block = blocks
        .get(&(cid.codec, cid.digest.clone()))
        .ok_or_else(|| {
            anyhow!(
                "the CAR is missing block {} - an incomplete DAG proves nothing",
                hex::encode(&cid.digest)
            )
        })?;
    let push = |out: &mut Vec<u8>, bytes: &[u8]| -> Result<()> {
        if out.len() + bytes.len() > caps.max_bytes {
            return Err(OverCap(format!("the file exceeds {} bytes", caps.max_bytes)).into());
        }
        out.extend_from_slice(bytes);
        Ok(())
    };
    match cid.codec {
        CODEC_RAW => return push(out, block),
        CODEC_DAG_PB => {}
        other => bail!("codec 0x{other:x} is not part of a UnixFS file"),
    }
    let start = out.len();
    let (mut links, mut data) = (Vec::new(), None);
    for (field, value) in pb_fields(block)? {
        match (field, value) {
            (1, Field::Bytes(d)) => data = Some(d),
            (2, Field::Bytes(l)) => links.push(l),
            _ => bail!("dag-pb node carries a field dag-pb does not define"),
        }
    }
    let data = data.ok_or_else(|| anyhow!("dag-pb node has no UnixFS data"))?;
    let (mut kind, mut inline, mut filesize, mut blocksizes) = (None, &[][..], None, Vec::new());
    for (field, value) in pb_fields(data)? {
        match (field, value) {
            (1, Field::Varint(t)) => kind = Some(t),
            (2, Field::Bytes(b)) => inline = b,
            (3, Field::Varint(s)) => filesize = Some(s),
            (4, Field::Varint(s)) => blocksizes.push(s),
            // hashType, fanout, mode, mtime: metadata that does not change the bytes.
            (5..=8, _) => {}
            _ => bail!("UnixFS data carries a field UnixFS does not define"),
        }
    }
    if !matches!(kind, Some(0 | 2)) {
        bail!("UnixFS node of type {kind:?} is not a file");
    }
    if !blocksizes.is_empty() && blocksizes.len() != links.len() {
        bail!(
            "UnixFS node has {} links but {} blocksizes",
            links.len(),
            blocksizes.len()
        );
    }
    push(out, inline)?;
    for (n, link) in links.iter().enumerate() {
        let hash = pb_fields(link)?
            .into_iter()
            .find_map(|(field, value)| match (field, value) {
                (1, Field::Bytes(h)) => Some(h),
                _ => None,
            })
            .ok_or_else(|| anyhow!("dag-pb link has no hash"))?;
        let (child, _) = cid_from_bytes(hash)?;
        let before = out.len();
        walk(&child, blocks, caps, depth + 1, visits, out)?;
        if let Some(&want) = blocksizes.get(n) {
            let got = (out.len() - before) as u64;
            if got != want {
                bail!("child {n} reassembles to {got} bytes, and its parent says {want}");
            }
        }
    }
    if let Some(want) = filesize {
        let got = (out.len() - start) as u64;
        if got != want {
            bail!("UnixFS node says its file is {want} bytes, and its blocks reassemble to {got}");
        }
    }
    Ok(())
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

    /// The root block of QoS query-result payload `QmWGJRtnm…` (548,413 bytes, three chunks), fetched
    /// raw from Pinata on 2026-09-13. The first assertion below checks it against its CID.
    const REAL_ROOT_CID: &str = "QmWGJRtnmRj1EPsfAgsay5LQErp6V5AGAJd8Fu2QMRsYAW";
    const REAL_ROOT_BLOCK: &str = "122a0a22122097e521759a0574d290bc3e4d405f583629a197b8c8f7aaedf8c287b28\
        9284d7f1200188e8010122a0a22122091b67ad0fb1b6dee6839656bf27da34db12dc89cc442b3c38ea0166905010b2a12\
        00188e8010122a0a221220688716427570ac8e03378ec47eb645ec994013d315d8826670f6f702a5bab47d120018cbbc01\
        0a12080218bdbc21208080102080801020bdbc01";

    fn real_root() -> Vec<u8> {
        hex::decode(REAL_ROOT_BLOCK.split_whitespace().collect::<String>()).unwrap()
    }

    fn links_of(block: &[u8]) -> Vec<(Vec<u8>, u64)> {
        pb_fields(block)
            .unwrap()
            .into_iter()
            .filter_map(|(f, v)| match (f, v) {
                (2, Field::Bytes(l)) => Some(l),
                _ => None,
            })
            .map(|l| {
                let parts = pb_fields(l).unwrap();
                let hash = parts.iter().find_map(|(f, v)| match (f, v) {
                    (1, Field::Bytes(h)) => Some(h.to_vec()),
                    _ => None,
                });
                let tsize = parts.iter().find_map(|(f, v)| match (f, v) {
                    (3, Field::Varint(t)) => Some(*t),
                    _ => None,
                });
                (hash.unwrap(), tsize.unwrap())
            })
            .collect()
    }

    /// **The branch encoding against a block the network actually serves.** Rebuilt from its own
    /// links and sizes it must come out byte for byte, empty `Name`s and unpacked `blocksizes` included.
    #[test]
    fn a_real_multi_block_root_re_encodes_byte_for_byte() {
        let real = real_root();
        let cid = Cid::parse(REAL_ROOT_CID).unwrap();
        assert_eq!(Sha256::digest(&real).as_slice(), cid.digest);
        let links = links_of(&real);
        assert_eq!(links.len(), 3);
        let filesizes = [262_144, 262_144, 548_413 - 2 * 262_144];
        let children: Vec<Child> = links
            .into_iter()
            .zip(filesizes)
            .map(|((link, tsize), filesize)| Child {
                link,
                tsize,
                filesize,
            })
            .collect();
        assert_eq!(unixfs_branch_node(&children), real);
    }

    /// The 256 KiB chunk is the one real leaves were cut to: a full leaf of that network block is
    /// exactly a UnixFS node around 262,144 bytes.
    #[test]
    fn the_default_chunk_is_the_one_real_leaves_were_cut_to() {
        let (_, full_leaf_tsize) = links_of(&real_root())[0];
        assert_eq!(
            full_leaf_tsize,
            unixfs_file_node(&vec![0u8; DEFAULT_CHUNK]).len() as u64
        );
    }

    /// The empty file's CID is the one every IPFS implementation gives it, and Kubo omits the empty
    /// Data field to get there.
    #[test]
    fn the_empty_file_has_the_cid_the_network_gives_it() {
        const EMPTY: &str = "QmbFMke1KXqnYyBBWxB74N4c5SBnJMVAiMNRcGu6x1AwQH";
        assert_eq!(cid_v0_for(b""), EMPTY);
        verify(&Cid::parse(EMPTY).unwrap(), b"").expect("the empty file verifies");
    }

    fn bytes(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i * 31 % 251) as u8).collect()
    }

    fn root_cid(version: u8, root: &Child) -> Cid {
        Cid {
            version,
            codec: CODEC_DAG_PB,
            hash_code: MH_SHA2_256,
            digest: root.digest().to_vec(),
        }
    }

    const LAYOUTS: [(u8, Leaves); 3] = [(0, Leaves::DagPb), (1, Leaves::Raw), (1, Leaves::DagPb)];

    #[test]
    fn a_multi_block_file_in_kubos_default_layout_verifies_and_one_changed_byte_does_not() {
        let content = bytes(DEFAULT_CHUNK * 2 + 12_345);
        for (version, leaves) in LAYOUTS {
            let root = balanced_dag(
                &content,
                version,
                leaves,
                DEFAULT_CHUNK,
                DEFAULT_MAX_LINKS,
                &mut |_, _| {},
            );
            let cid = root_cid(version, &root);
            verify(&cid, &content).expect("the default layout re-encodes");
            let mut changed = content.clone();
            changed[DEFAULT_CHUNK + 7] ^= 1;
            let err = verify(&cid, &changed).unwrap_err();
            assert!(
                err.is::<Unprovable>(),
                "a large mismatch is unprovable, not an accusation: {err}"
            );
        }
    }

    /// Each block with the link bytes that name it.
    type Blocks = Vec<(Vec<u8>, Vec<u8>)>;

    /// A CARv1 of `blocks`. The header is filler: the walker never trusts what it claims.
    fn car(blocks: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        let header = b"not read";
        put_varint(&mut out, header.len() as u64);
        out.extend_from_slice(header);
        for (link, block) in blocks {
            put_varint(&mut out, (link.len() + block.len()) as u64);
            out.extend_from_slice(link);
            out.extend_from_slice(block);
        }
        out
    }

    fn dag(
        content: &[u8],
        version: u8,
        leaves: Leaves,
        chunk: usize,
        links: usize,
    ) -> (Cid, Blocks) {
        let mut blocks = Vec::new();
        let root = balanced_dag(
            content,
            version,
            leaves,
            chunk,
            links,
            &mut |link, block| blocks.push((link.to_vec(), block)),
        );
        (root_cid(version, &root), blocks)
    }

    /// **Any depth, either leaf kind, both CID versions**: 63 leaves under four levels reassemble from
    /// blocks that each had to hash to the CID naming them.
    #[test]
    fn a_deep_dag_reassembles_from_verified_blocks() {
        let content = bytes(1_000);
        for (version, leaves) in LAYOUTS {
            let (root, blocks) = dag(&content, version, leaves, 16, 3);
            let got = content_from_car(&root, &car(&blocks), &CAPS).unwrap();
            assert_eq!(got, content);
        }
    }

    #[test]
    fn a_tampered_block_is_refused_even_though_the_rest_would_reassemble() {
        let (root, mut blocks) = dag(&bytes(1_000), 1, Leaves::Raw, 16, 3);
        blocks[5].1[0] ^= 1;
        let err = content_from_car(&root, &car(&blocks), &CAPS)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not hash to the CID"), "{err}");
    }

    #[test]
    fn a_missing_block_is_refused() {
        let (root, mut blocks) = dag(&bytes(1_000), 0, Leaves::DagPb, 16, 3);
        blocks.remove(3);
        let err = content_from_car(&root, &car(&blocks), &CAPS)
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing block"), "{err}");
    }

    /// A branch with chosen sizes, for nodes that lie about them. No `Name`s: the walker must not care.
    fn lying_branch(children: &[Child], filesize: u64, blocksizes: &[u64]) -> (Cid, Vec<u8>) {
        let mut node = Vec::new();
        for c in children {
            let mut link = Vec::new();
            put_bytes_field(&mut link, 0x0a, &c.link);
            link.push(0x18);
            put_varint(&mut link, c.tsize);
            put_bytes_field(&mut node, 0x12, &link);
        }
        let mut unixfs = vec![0x08, 0x02, 0x18];
        put_varint(&mut unixfs, filesize);
        for s in blocksizes {
            unixfs.push(0x20);
            put_varint(&mut unixfs, *s);
        }
        put_bytes_field(&mut node, 0x0a, &unixfs);
        let root = Cid {
            version: 0,
            codec: CODEC_DAG_PB,
            hash_code: MH_SHA2_256,
            digest: Sha256::digest(&node).to_vec(),
        };
        (root, node)
    }

    fn two_leaves() -> (Vec<Child>, Blocks) {
        let mut kids = Vec::new();
        let mut blocks = Vec::new();
        for piece in [b"first half ".as_slice(), b"second half"] {
            let block = unixfs_file_node(piece);
            let link = link_bytes(0, CODEC_DAG_PB, &Sha256::digest(&block));
            kids.push(Child {
                link: link.clone(),
                tsize: block.len() as u64,
                filesize: piece.len() as u64,
            });
            blocks.push((link, block));
        }
        (kids, blocks)
    }

    fn walk_branch(
        kids: &[Child],
        leaves: &[(Vec<u8>, Vec<u8>)],
        filesize: u64,
        sizes: &[u64],
    ) -> Result<Vec<u8>> {
        let (root, node) = lying_branch(kids, filesize, sizes);
        let mut blocks = leaves.to_vec();
        blocks.push((link_bytes(0, CODEC_DAG_PB, &root.digest), node));
        content_from_car(&root, &car(&blocks), &CAPS)
    }

    #[test]
    fn a_node_that_misstates_its_file_size_is_refused() {
        let (kids, leaves) = two_leaves();
        let honest = walk_branch(&kids, &leaves, 22, &[11, 11]).unwrap();
        assert_eq!(honest, b"first half second half");
        let err = walk_branch(&kids, &leaves, 23, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("says its file is 23 bytes"), "{err}");
    }

    /// The total can be right while the split is wrong, which only the per-child check sees.
    #[test]
    fn a_node_whose_blocksizes_misplace_bytes_between_children_is_refused() {
        let (kids, leaves) = two_leaves();
        let err = walk_branch(&kids, &leaves, 22, &[12, 10])
            .unwrap_err()
            .to_string();
        assert!(err.contains("child 0 reassembles to 11 bytes"), "{err}");
    }

    #[test]
    fn a_dag_past_any_cap_is_refused_as_over_cap() {
        let (root, blocks) = dag(&bytes(1_000), 1, Leaves::Raw, 16, 3);
        let body = car(&blocks);
        for (caps, why) in [
            (
                Caps {
                    max_blocks: 20,
                    max_bytes: 1 << 20,
                    max_depth: 16,
                },
                "blocks",
            ),
            (
                Caps {
                    max_blocks: 4_096,
                    max_bytes: 999,
                    max_depth: 16,
                },
                "bytes",
            ),
            (
                Caps {
                    max_blocks: 4_096,
                    max_bytes: 1 << 20,
                    max_depth: 2,
                },
                "levels",
            ),
        ] {
            let err = content_from_car(&root, &body, &caps).unwrap_err();
            assert!(err.is::<OverCap>(), "{why}: {err}");
            assert!(err.to_string().contains(why), "{why}: {err}");
        }
    }

    /// One byte linked a thousand times, three levels deep: a few kilobytes on the wire, and a billion
    /// visits without the cap.
    #[test]
    fn a_dag_that_links_one_block_over_and_over_is_stopped_by_the_visit_cap() {
        let leaf = b"x".to_vec();
        let leaf_link = link_bytes(1, CODEC_RAW, &Sha256::digest(&leaf));
        let mut blocks = vec![(leaf_link.clone(), leaf)];
        let mut child = Child {
            link: leaf_link,
            tsize: 1,
            filesize: 1,
        };
        for _ in 0..3 {
            let kids: Vec<Child> = (0..1000)
                .map(|_| Child {
                    link: child.link.clone(),
                    tsize: child.tsize,
                    filesize: child.filesize,
                })
                .collect();
            let block = unixfs_branch_node(&kids);
            let link = link_bytes(1, CODEC_DAG_PB, &Sha256::digest(&block));
            child = Child {
                tsize: block.len() as u64 + child.tsize * 1000,
                filesize: child.filesize * 1000,
                link: link.clone(),
            };
            blocks.push((link, block));
        }
        let root = root_cid(1, &child);
        let caps = Caps {
            max_blocks: 4_096,
            max_bytes: 64 << 20,
            max_depth: 16,
        };
        let err = content_from_car(&root, &car(&blocks), &caps).unwrap_err();
        assert!(
            err.is::<OverCap>() && err.to_string().contains("visits"),
            "{err}"
        );
    }

    /// **A real QoS payload, both ways**: re-encoding The Graph's copy of the file, and reassembling
    /// Pinata's CAR of its blocks, must each prove it.
    ///
    /// Loudly skipped without `NUTHATCH_IPFS_LIVE=1`.
    #[tokio::test]
    async fn a_real_qos_payload_verifies_by_re_encoding_and_from_its_blocks() {
        if std::env::var("NUTHATCH_IPFS_LIVE").is_err() {
            eprintln!(
                "SKIP a_real_qos_payload_verifies_by_re_encoding_and_from_its_blocks: \
                 set NUTHATCH_IPFS_LIVE=1 to run it"
            );
            return;
        }
        let want = "QmTXcz8kqsWuGeasoixcRwngpozM6Xd2WRtdNSDemYdSiQ";
        let cid = Cid::parse(want).unwrap();
        let file = reqwest::get(format!("https://ipfs.thegraph.com/ipfs/{want}"))
            .await
            .expect("gateway reachable")
            .bytes()
            .await
            .expect("body readable");
        assert!(
            file.len() > DEFAULT_CHUNK,
            "must be multi-block to prove anything"
        );
        let started = std::time::Instant::now();
        verify(&cid, &file).expect("a default-layout payload re-encodes");
        eprintln!("re-encoded {} bytes in {:?}", file.len(), started.elapsed());
        let car_body = reqwest::Client::new()
            .get(format!(
                "https://gateway.pinata.cloud/ipfs/{want}?format=car"
            ))
            .header("accept", "application/vnd.ipld.car")
            .send()
            .await
            .expect("pinata reachable")
            .bytes()
            .await
            .expect("CAR readable");
        let content = content_from_car(&cid, &car_body, &CAPS).expect("its blocks prove the file");
        assert_eq!(content.as_slice(), file.as_ref());
    }
}
