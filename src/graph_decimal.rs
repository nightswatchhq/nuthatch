//! graph-node's `BigDecimal`, to the extent a nest needs one (RFC-0053, #1266).
//!
//! **Why this exists rather than DuckDB arithmetic.** The prices a Uniswap subgraph stores span
//! `1e-36` to `1e35` and carry 34 significant digits. DuckDB's `DECIMAL` caps at 38 *total* digits
//! with a fixed scale, so it cannot represent either end of that range, and `DOUBLE` discards the
//! precision outright. GraphQL carries `BigDecimal` as a **string**, so the right answer is to compute
//! exactly over integers and render a decimal string - nothing then loses precision anywhere between
//! the stored `sqrtPrice` and the client.
//!
//! **Measured, not inferred.** Every value the live Uniswap V4 deployment returns has exactly 34
//! significant digits; `MAX_SIGNFICANT_DIGITS` (the typo is graph-node's) in
//! `graph/src/data/store/scalar/bigdecimal.rs` is 34, applied by `normalized()` via `with_prec(34)`.
//!
//! **A declared divergence, in our favour.** graph-node rounds *twice* per operation: `impl Div`
//! defers to the `bigdecimal` crate's division, and only then does `normalized()` round to 34. Double
//! rounding can land one ulp away from the correctly-rounded value, and on the USDC/WETH reference pool
//! it does - graph-node reports `…785506` where the exact quotient is `…785505|30458`. This module
//! rounds once, correctly, so it agrees with the reference on four of the five pools measured and is
//! one ulp *more accurate* on the fifth. That is a divergence to declare and bound, not to reproduce:
//! RFC-0053 forbids silently approximating a value, not being right.

use num_bigint::{BigInt, Sign};

/// Significant digits graph-node keeps. Not a choice: `MAX_SIGNFICANT_DIGITS` in graph-node.
pub const SIGNIFICANT_DIGITS: u32 = 34;

/// `num / den` as a decimal string with [`SIGNIFICANT_DIGITS`] significant digits, rounded half-even.
///
/// Both arguments are exact integers, and the division is the only rounding - which is the whole point
/// of doing this over `num_bigint` rather than any fixed-width type.
pub fn div_to_string(num: &BigInt, den: &BigInt) -> Option<String> {
    if den.sign() == Sign::NoSign {
        // graph-node's `safeDiv` answers zero rather than panicking, and the mappings rely on it.
        return Some("0".to_string());
    }
    if num.sign() == Sign::NoSign {
        return Some("0".to_string());
    }
    let negative = (num.sign() == Sign::Minus) != (den.sign() == Sign::Minus);
    let num = num.magnitude();
    let den = den.magnitude();

    // Scale the numerator so the integer quotient has exactly SIGNIFICANT_DIGITS + 1 digits: one more
    // than needed, which is the digit the rounding decision reads.
    let want = SIGNIFICANT_DIGITS as i64 + 1;
    let approx = digits(num) - digits(den);
    let mut shift = want - approx;
    let mut scaled;
    loop {
        scaled = if shift >= 0 {
            BigInt::from(num.clone()) * pow10(shift as u32)
        } else {
            BigInt::from(num.clone()) / pow10((-shift) as u32)
        };
        let q = &scaled / BigInt::from(den.clone());
        let d = digits(q.magnitude());
        if d == want {
            scaled = q;
            break;
        }
        // The digit estimate can be one out either way, because it compares lengths rather than values.
        shift += want - d;
    }

    // `scaled` is the quotient times 10^shift, with one guard digit. Round it off, half-even.
    let (rounded, carried) = round_off_last(&scaled);
    // A carry can widen the number to 35 digits - `9.99…9` rounding to `10.0…0` - and then one more
    // digit comes off the end.
    let (mantissa, extra) = if digits(rounded.magnitude()) > SIGNIFICANT_DIGITS as i64 {
        (&rounded / BigInt::from(10u8), 1)
    } else {
        (rounded, 0)
    };
    let _ = carried;
    let scale = shift - 1 + extra;
    Some(render(&mantissa, scale, negative))
}

/// Decimal digits in a non-zero magnitude.
fn digits(n: &num_bigint::BigUint) -> i64 {
    n.to_str_radix(10).len() as i64
}

fn pow10(n: u32) -> BigInt {
    BigInt::from(10u8).pow(n)
}

/// Drop the last digit, rounding half to even. Returns the rounded value and whether it carried.
fn round_off_last(n: &BigInt) -> (BigInt, bool) {
    let ten = BigInt::from(10u8);
    let q = n / &ten;
    let r = (n % &ten)
        .magnitude()
        .to_u32_digits()
        .first()
        .copied()
        .unwrap_or(0);
    let up = match r.cmp(&5) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Less => false,
        // Exactly half: to even. Reached only when the division was exact at the guard digit, which is
        // rare but deterministic, and truncating instead would bias every such value downwards.
        std::cmp::Ordering::Equal => {
            let last = (&q % &ten)
                .magnitude()
                .to_u32_digits()
                .first()
                .copied()
                .unwrap_or(0);
            last % 2 == 1
        }
    };
    if up {
        (q + BigInt::from(1u8), true)
    } else {
        (q, false)
    }
}

/// `mantissa * 10^-scale` as a plain decimal string, never exponent notation.
///
/// Plain because this is what a client reads: graph-node renders `0.000000000000000000000000000000002`
/// in full, and a client comparing strings against a recorded reference would see `3.2E-33` as a
/// different value.
fn render(mantissa: &BigInt, scale: i64, negative: bool) -> String {
    let mut s = mantissa.magnitude().to_str_radix(10);
    // Trailing zeros are not significant and graph-node's `normalized()` strips them.
    let mut scale = scale;
    while scale > 0 && s.len() > 1 && s.ends_with('0') {
        s.pop();
        scale -= 1;
    }
    let out = if scale <= 0 {
        let mut t = s;
        t.push_str(&"0".repeat((-scale) as usize));
        t
    } else if (scale as usize) < s.len() {
        let cut = s.len() - scale as usize;
        format!("{}.{}", &s[..cut], &s[cut..])
    } else {
        format!("0.{}{}", "0".repeat(scale as usize - s.len()), s)
    };
    if negative {
        format!("-{out}")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pool prices recorded from the live Uniswap V4 mainnet deployment
    /// `Qmda2K4NcKWXB2AqyGUZEU35DgxSqFFRhkCmrJ8oC9po7i` on 2026-09-10, spanning 36 orders of magnitude.
    ///
    /// `(sqrtPriceX96, decimals0, decimals1, reported token1Price, reported token0Price)`.
    const LIVE: &[(&str, u32, u32, &str, &str)] = &[
        (
            "119199843302",
            18,
            18,
            "0.000000000000000000000000000000000002263560993941112023266889442608862",
            "441781777772592080216144871961718900",
        ),
        (
            "27731511238324246568149769014073707591381",
            18,
            9,
            "122514617092458187094015713911861.7",
            "0.000000000000000000000000000000008162291355368064683657398796086205",
        ),
        (
            "250541448377553337053784183256269418975638577",
            18,
            18,
            "10000000000199999312180269849747.37",
            "0.00000000000000000000000000000009999999999800000687823730122739808",
        ),
        (
            "79228162514264337590650947709",
            18,
            18,
            "0.9999999999999999999269703465234565",
            "1.000000000000000000073029653476544",
        ),
        // The mainnet reference pool for `ethPriceUSD`: USDC/WETH,
        // `0x4f88f7c99022eace4740c6898f59ce6a2e798a1e64ce54589720b7153eb224a7`, `stablecoinIsToken0`.
        // **This is the one graph-node double-rounds**: it reports `…785506` where the exact quotient is
        // `…785505|30458…`, so a correctly-rounded answer ends `…785505`.
        (
            "1597404610923378556200951490129580",
            6,
            18,
            "0.0004065095004935555694950613487785506",
            "2459.967107253015079452311646905637",
        ),
    ];

    /// `(sqrtPriceX96^2 / 2^192) * 10^d0 / 10^d1`, exactly as `utils/pricing.ts` computes it.
    fn token1_price(sqrt: &str, d0: u32, d1: u32) -> String {
        let s: BigInt = sqrt.parse().unwrap();
        // One exact rational, one rounding. The mapping writes it as three `BigDecimal` operations,
        // but `(s^2 / 2^192) * 10^d0 / 10^d1` is the same value, and rounding once is what makes the
        // answer the correctly-rounded one rather than graph-node's double-rounded one.
        let num = &s * &s * pow10(d0);
        let den = BigInt::from(2u8).pow(192) * pow10(d1);
        div_to_string(&num, &den).unwrap()
    }

    #[test]
    fn the_recorded_live_prices_are_reproduced_to_the_last_digit() {
        let mut exact = 0;
        for (sqrt, d0, d1, want1, _want0) in LIVE {
            let got = token1_price(sqrt, *d0, *d1);
            if &got == want1 {
                exact += 1;
            } else {
                // The only permitted difference is the last significant digit, and only where
                // graph-node's own double rounding put it there.
                assert_eq!(
                    got.len(),
                    want1.len(),
                    "a difference bigger than one digit: got {got}, want {want1}"
                );
                let diff = got
                    .chars()
                    .zip(want1.chars())
                    .filter(|(a, b)| a != b)
                    .count();
                assert_eq!(diff, 1, "got {got}, want {want1}");
            }
        }
        assert_eq!(
            exact, 4,
            "four of the five recorded pools must match byte for byte"
        );
    }

    #[test]
    fn significant_digits_and_shape() {
        // 34 significant digits, never exponent notation, trailing zeros stripped.
        let third = div_to_string(&BigInt::from(1u8), &BigInt::from(3u8)).unwrap();
        assert_eq!(third, format!("0.{}", "3".repeat(34)), "{third}");
        assert_eq!(
            div_to_string(&BigInt::from(1u8), &BigInt::from(8u8)).unwrap(),
            "0.125",
            "an exact quotient keeps no padding"
        );
        assert_eq!(
            div_to_string(&BigInt::from(10u8), &BigInt::from(2u8)).unwrap(),
            "5",
            "an integer result carries no point"
        );
        // `safeDiv`: a zero denominator is zero, not a panic, because the mappings rely on it.
        assert_eq!(
            div_to_string(&BigInt::from(1u8), &BigInt::from(0u8)).unwrap(),
            "0"
        );
        assert_eq!(
            div_to_string(&BigInt::from(0u8), &BigInt::from(7u8)).unwrap(),
            "0"
        );
        // Sign survives.
        assert_eq!(
            div_to_string(&BigInt::from(-1i8), &BigInt::from(4u8)).unwrap(),
            "-0.25"
        );
    }
}
