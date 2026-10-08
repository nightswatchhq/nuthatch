//! #296: the prototype compact row encoding, and the check that it and the JSON form decode to the
//! same row. The RSS measurement that used it is `tests/bench_compact_rows.rs` at `ede03ca7`; its
//! result is in `docs/decisions/296-compact-rows.md`.

use serde_json::json;

/// A row the shape of a real one: the implicit columns plus an ERC-20 transfer's parameters.
///
/// Modelled on `staking__tokens_delegated` and `usdc__transfer`, the two shapes the decision
/// document measured, so the payload ratio here can be checked against the 2.45-2.49x it reported.
fn row_json(i: u64) -> String {
    let addr = |n: u64| format!("0x{:040x}", n);
    let h32 = |n: u64| format!("0x{:064x}", n);
    json!({
        "table": "usdc__transfer",
        "block_number": 60_000_000 + i,
        "block_hash": h32(i),
        "block_timestamp": 1_700_000_000u64 + i,
        "tx_hash": h32(i.wrapping_mul(7)),
        "log_index": i % 8,
        "address": addr(0xa0b8),
        "_seq": ((60_000_000 + i) << 20) | (i % 8),
        "from": addr(i % 5_000),
        "to": addr((i * 3) % 5_000),
        "value": format!("{}", 1_000_000_000_000_000_000u128 + i as u128),
        "value_dec": format!("{}", 1_000_000_000_000_000_000u128 + i as u128),
        "value_overflow": false,
    })
    .to_string()
}

/// The compact form, faithful to the model the decision document priced.
///
/// Field names are dropped (the schema has them), hashes are 32 raw bytes rather than 66-char hex,
/// addresses 20 rather than 42, the `uint256` is its 32-byte word rather than a decimal string,
/// block numbers and timestamps are varints, and `_seq` is **not stored at all** because it is
/// derived from `(block << 20) | log_index`.
fn row_compact(i: u64) -> Vec<u8> {
    fn varint(out: &mut Vec<u8>, mut n: u64) {
        while n >= 0x80 {
            out.push((n as u8) | 0x80);
            n >>= 7;
        }
        out.push(n as u8);
    }
    // The same values `row_json` writes, not zeros. The first version filled the fixed-width tail
    // with zero bytes while the JSON row carried values derived from `i`, so the two encodings did
    // not represent the same row and nothing in the harness noticed. `decoders_agree` below is what
    // stops that recurring.
    fn word32(n: u128) -> [u8; 32] {
        let mut w = [0u8; 32];
        w[16..].copy_from_slice(&n.to_be_bytes());
        w
    }
    fn addr20(n: u64) -> [u8; 20] {
        let mut a = [0u8; 20];
        a[12..].copy_from_slice(&n.to_be_bytes());
        a
    }
    let mut b = Vec::with_capacity(160);
    b.extend_from_slice(&[0u8, 1]); // table id, from a per-store dictionary
    varint(&mut b, 60_000_000 + i); // block_number
    varint(&mut b, 1_700_000_000 + i); // block_timestamp
    varint(&mut b, i % 8); // log_index
    b.extend_from_slice(&word32(i as u128)); // block_hash
    b.extend_from_slice(&word32(i.wrapping_mul(7) as u128)); // tx_hash
    b.extend_from_slice(&addr20(0xa0b8)); // address
    b.extend_from_slice(&addr20(i % 5_000)); // from
    b.extend_from_slice(&addr20((i * 3) % 5_000)); // to
    b.extend_from_slice(&word32(1_000_000_000_000_000_000u128 + i as u128)); // value
    b.push(0); // value_overflow
               // `value_dec` is not stored: it is the same number, and the duplicate is schema redundancy
               // rather than encoding. The decision document excludes it from every figure for exactly this
               // reason, so it is excluded here too.
    b
}

/// The fields a caller gets back from one stored row. Both decoders must produce **all** of it, or
/// the comparison is not one.
///
/// The first version of this harness parsed the JSON object in full with `serde_json` and, on the
/// compact side, read three varints and sliced twenty bytes - then reported the difference as a
/// decode win. Review caught it. That is a full parse against a partial one, and the number it
/// produced was not a measurement of anything. This struct exists so the two paths cannot drift
/// apart again: every field is materialised on both sides, and `checksum` forces all of it to be
/// read rather than optimised out.
#[derive(Default)]
struct Row {
    block: u64,
    ts: u64,
    log_index: u64,
    seq: u64,
    block_hash: [u8; 32],
    tx_hash: [u8; 32],
    address: [u8; 20],
    from: [u8; 20],
    to: [u8; 20],
    value: [u8; 32],
    overflow: bool,
}

impl Row {
    fn checksum(&self) -> u64 {
        let b = |x: &[u8]| {
            x.iter()
                .fold(0u64, |a, &c| a.wrapping_mul(31).wrapping_add(c as u64))
        };
        self.block
            ^ self.ts
            ^ self.log_index
            ^ self.seq
            ^ b(&self.block_hash)
            ^ b(&self.tx_hash)
            ^ b(&self.address)
            ^ b(&self.from)
            ^ b(&self.to)
            ^ b(&self.value)
            ^ self.overflow as u64
    }
}

/// Decode one stored row into `Row`, by whichever encoding it is in.
fn decode_one(buf: &[u8], compact: bool) -> u64 {
    let mut r = Row::default();
    if compact {
        fn varint(buf: &[u8], p: &mut usize) -> u64 {
            let (mut n, mut shift) = (0u64, 0u32);
            loop {
                let b = buf[*p];
                *p += 1;
                n |= ((b & 0x7f) as u64) << shift;
                if b < 0x80 {
                    return n;
                }
                shift += 7;
            }
        }
        let mut p = 2usize; // table id
        r.block = varint(buf, &mut p);
        r.ts = varint(buf, &mut p);
        r.log_index = varint(buf, &mut p);
        let mut take = |n: usize, out: &mut [u8]| {
            out.copy_from_slice(&buf[p..p + n]);
            p += n;
        };
        take(32, &mut r.block_hash);
        take(32, &mut r.tx_hash);
        take(20, &mut r.address);
        take(20, &mut r.from);
        take(20, &mut r.to);
        take(32, &mut r.value);
        r.overflow = buf[p] != 0;
        // `_seq` is derived rather than stored - that saving is part of the encoding.
        r.seq = (r.block << 20) | r.log_index;
    } else {
        let v: serde_json::Value = serde_json::from_slice(buf).expect("json");
        r.block = v["block_number"].as_u64().unwrap_or(0);
        r.ts = v["block_timestamp"].as_u64().unwrap_or(0);
        r.log_index = v["log_index"].as_u64().unwrap_or(0);
        r.seq = v["_seq"].as_u64().unwrap_or(0);
        // The hex strings must actually be turned into bytes: that is the work the compact side
        // does not have to do, and it is the whole of the difference being measured.
        let un = |v: &serde_json::Value, out: &mut [u8]| {
            let s = v.as_str().unwrap_or("");
            let _ = hex::decode_to_slice(s.strip_prefix("0x").unwrap_or(s), out);
        };
        un(&v["block_hash"], &mut r.block_hash);
        un(&v["tx_hash"], &mut r.tx_hash);
        un(&v["address"], &mut r.address);
        un(&v["from"], &mut r.from);
        un(&v["to"], &mut r.to);
        // `value` is stored as a decimal string and its word form is what a caller wants.
        let n: u128 = v["value"].as_str().unwrap_or("0").parse().unwrap_or(0);
        r.value[16..].copy_from_slice(&n.to_be_bytes());
        r.overflow = v["value_overflow"].as_bool().unwrap_or(false);
    }
    r.checksum()
}

/// The two encodings must represent the **same row**, or the latency comparison above is between two
/// different pieces of work and means nothing.
///
/// This is the check whose absence review found twice: the first pass compared a full JSON parse
/// against a partial compact read, and the second still wrote zero bytes into every fixed-width
/// field of the compact row while the JSON row carried real values. Both were invisible because
/// nothing ever compared the two decoders' output. Now something does, and it is a **normal test**
/// rather than an ignored one, so it runs in CI where the measurement itself does not.
#[test]
fn the_two_encodings_decode_to_the_same_row() {
    for i in [0u64, 1, 7, 127, 128, 5_000, 999_999, 1_599_999] {
        let j = decode_one(row_json(i).as_bytes(), false);
        let c = decode_one(&row_compact(i), true);
        assert_eq!(j, c, "row {i}: json decoded to {j:#x}, compact to {c:#x}");
    }

    // And it must be able to tell rows apart - an encoder that returned a constant would satisfy the
    // loop above without encoding anything at all.
    let distinct: std::collections::HashSet<u64> =
        (0..64).map(|i| decode_one(&row_compact(i), true)).collect();
    assert_eq!(distinct.len(), 64, "compact decode is not row-dependent");
}
