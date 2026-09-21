//! Pure, bounded conversions for event-shaped SQL. Decimal arithmetic must not pass through
//! DOUBLE, and a digest-to-CID conversion must not fetch content or hash the digest again.
use anyhow::{bail, Context, Result};
use duckdb::{
    arrow::{
        array::{Array, RecordBatch, StringArray},
        datatypes::DataType,
    },
    vscalar::arrow::{ArrowFunctionSignature, VArrowScalar},
    Connection,
};
use num_bigint::BigUint;
use std::sync::Arc;

pub(crate) fn register(connection: &Connection) -> Result<()> {
    connection.register_scalar_function::<Scalar<1>>("nuthatch_uint256")?;
    connection.register_scalar_function::<Scalar<2>>("nuthatch_mul_div")?;
    connection.register_scalar_function::<Scalar<3>>("nuthatch_cid_v0")?;
    connection.register_scalar_function::<Scalar<4>>("nuthatch_base58_uint256")?;
    connection.register_scalar_function::<Scalar<5>>("nuthatch_uint256_word")?;
    connection.register_scalar_function::<Scalar<6>>("nuthatch_keccak256")?;
    connection.register_scalar_function::<Scalar<7>>("nuthatch_abi_tuple")?;
    Ok(())
}

fn decimal(value: &str) -> Result<BigUint> {
    // 512-bit intermediates, with a little room for event-ledger sums, without admitting
    // arbitrarily large user strings into the bigint allocator.
    if value.is_empty() || value.len() > 160 || !value.bytes().all(|b| b.is_ascii_digit()) {
        bail!("expected an unsigned decimal integer of at most 160 digits");
    }
    BigUint::parse_bytes(value.as_bytes(), 10).context("invalid decimal integer")
}

fn evaluate(kind: u8, values: &[&str]) -> Result<String> {
    match kind {
        1 => {
            let word = values[0];
            if word.len() != 66
                || !word.starts_with("0x")
                || !word[2..].bytes().all(|b| b.is_ascii_hexdigit())
            {
                bail!(
                    "expected one 32-byte ABI uint256 word (received {} characters)",
                    word.len()
                );
            }
            Ok(BigUint::parse_bytes(&word.as_bytes()[2..], 16)
                .context("invalid ABI word")?
                .to_string())
        }
        2 => {
            let numerator = decimal(values[0])? * decimal(values[1])?;
            let denominator = decimal(values[2])?;
            if denominator == BigUint::default() {
                bail!("integer division by zero");
            }
            Ok((numerator / denominator).to_string())
        }
        3 => {
            let word = values[0];
            if word.len() != 66 || !word.starts_with("0x") {
                bail!("expected a 32-byte 0x-prefixed digest");
            }
            let mut digest = [0u8; 32];
            hex::decode_to_slice(&word[2..], &mut digest).context("invalid digest")?;
            Ok(crate::cid::cid_v0_from_digest(&digest))
        }
        4 => {
            let integer = decimal(values[0])?;
            if integer.bits() > 256 {
                bail!("integer exceeds uint256");
            }
            Ok(crate::cid::base58_encode(&integer.to_bytes_be()))
        }
        5 => {
            let integer = decimal(values[0])?;
            if integer.bits() > 256 {
                bail!("integer exceeds uint256");
            }
            Ok(format!("0x{integer:064x}"))
        }
        6 => {
            let value = values[0];
            if !value.starts_with("0x") || value.len() > 2050 {
                bail!("expected at most 1024 hex-encoded bytes");
            }
            let bytes = hex::decode(&value[2..]).context("invalid hex bytes")?;
            Ok(format!("{:#x}", alloy_primitives::keccak256(bytes)))
        }
        7 => {
            use alloy_dyn_abi::{DynSolType, DynSolValue};
            if values[0].len() > 160 || values[1].len() > 131074 {
                bail!("ABI tuple exceeds the type or payload limit");
            }
            let types = values[0]
                .split(',')
                .map(|name| match name {
                    "string" => Ok(DynSolType::String),
                    "bytes" => Ok(DynSolType::Bytes),
                    "address" => Ok(DynSolType::Address),
                    "uint256" => Ok(DynSolType::Uint(256)),
                    "bytes32" => Ok(DynSolType::FixedBytes(32)),
                    "bool" => Ok(DynSolType::Bool),
                    _ => anyhow::bail!("unsupported ABI tuple member"),
                })
                .collect::<Result<Vec<_>>>()?;
            if types.len() > 16 {
                bail!("ABI tuple exceeds 16 members");
            }
            let bytes = hex::decode(
                values[1]
                    .strip_prefix("0x")
                    .context("ABI tuple requires 0x hex")?,
            )?;
            let DynSolValue::Tuple(items) = DynSolType::Tuple(types).abi_decode_params(&bytes)?
            else {
                bail!("ABI value was not a tuple");
            };
            let json = items
                .into_iter()
                .map(|item| match item {
                    DynSolValue::String(s) => serde_json::Value::String(s),
                    DynSolValue::Bytes(b) => serde_json::json!(format!("0x{}", hex::encode(b))),
                    DynSolValue::Address(a) => serde_json::json!(format!("{a:#x}")),
                    DynSolValue::Uint(n, _) => serde_json::json!(n.to_string()),
                    DynSolValue::FixedBytes(b, _) => serde_json::json!(format!("{b:#x}")),
                    DynSolValue::Bool(b) => serde_json::json!(b),
                    _ => unreachable!("bounded flat ABI types"),
                })
                .collect::<Vec<_>>();
            Ok(serde_json::to_string(&json)?)
        }
        _ => unreachable!("registered scalar kind"),
    }
}

struct Scalar<const KIND: u8>;
impl<const KIND: u8> VArrowScalar for Scalar<KIND> {
    type State = ();

    fn invoke(
        _: &(),
        input: RecordBatch,
    ) -> std::result::Result<Arc<dyn Array>, Box<dyn std::error::Error>> {
        let columns = input
            .columns()
            .iter()
            .map(|c| {
                c.as_any()
                    .downcast_ref::<StringArray>()
                    .context("expected VARCHAR vector")
            })
            .collect::<Result<Vec<_>>>()?;
        let mut output = Vec::with_capacity(input.num_rows());
        for row in 0..input.num_rows() {
            if columns.iter().any(|c| c.is_null(row)) {
                output.push(None);
                continue;
            }
            let values = columns.iter().map(|c| c.value(row)).collect::<Vec<_>>();
            output.push(Some(evaluate(KIND, &values)?));
        }
        Ok(Arc::new(StringArray::from(output)))
    }

    fn signatures() -> Vec<ArrowFunctionSignature> {
        vec![ArrowFunctionSignature::exact(
            vec![
                DataType::Utf8;
                match KIND {
                    2 => 3,
                    7 => 2,
                    _ => 1,
                }
            ],
            DataType::Utf8,
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_case_guard_does_not_decode_an_empty_predeployment_word() {
        let conn = Connection::open_in_memory().unwrap();
        register(&conn).unwrap();
        let word = format!("0x{:064x}", 16_083_151);
        let mut statement = conn
            .prepare(
                "SELECT DISTINCT CASE WHEN reverted OR result = '0x' THEN 0 \
             ELSE CAST(nuthatch_uint256(result) AS INTEGER) END AS value \
             FROM (VALUES ('0x', false), (?1, false)) t(result, reverted) ORDER BY value",
            )
            .unwrap();
        let values = statement
            .query_map([word], |row| row.get::<_, i32>(0))
            .unwrap()
            .collect::<duckdb::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(values, vec![0, 16_083_151]);
    }

    #[test]
    fn sql_scalars_preserve_full_width_and_nulls_and_refuse_bad_inputs() {
        let conn = Connection::open_in_memory().unwrap();
        register(&conn).unwrap();
        let max = "115792089237316195423570985008687907853269984665640564039457584007913129639935";
        let value: String = conn
            .query_row("SELECT nuthatch_mul_div(?1, ?1, ?1)", [max], |r| r.get(0))
            .unwrap();
        assert_eq!(value, max);
        let value: String = conn
            .query_row(
                "SELECT nuthatch_uint256(?1)",
                [format!("0x{}", "f".repeat(64))],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(value, max);
        let value: String = conn
            .query_row("SELECT nuthatch_mul_div('11','3','2')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, "16");
        let value: Option<String> = conn
            .query_row("SELECT nuthatch_mul_div(NULL,'3','2')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, None);
        let value: String = conn
            .query_row(
                "SELECT nuthatch_cid_v0(?1)",
                [format!("0x{}", "00".repeat(32))],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(value, "QmNLei78zWmzUdbeRB3CiUfAizWUrbeeZh5K1rhAQKCh51");
        for sql in [
            "SELECT nuthatch_uint256('0x01')",
            "SELECT nuthatch_cid_v0('0x00')",
            "SELECT nuthatch_mul_div('1','2','0')",
            "SELECT nuthatch_mul_div('-1','2','3')",
        ] {
            assert!(
                conn.query_row::<String, _, _>(sql, [], |r| r.get(0))
                    .is_err(),
                "{sql}"
            );
        }
        assert!(decimal(&"9".repeat(161)).is_err());
        let value: String = conn
            .query_row("SELECT nuthatch_base58_uint256('256')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, "5R");
        let value: String = conn
            .query_row("SELECT nuthatch_base58_uint256('0')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(value, "1");
        assert!(evaluate(4, &[&"9".repeat(79)]).is_err());
        assert_eq!(evaluate(5, &["256"]).unwrap(), format!("0x{:064x}", 256));
        assert_eq!(
            evaluate(6, &["0x"]).unwrap(),
            "0xc5d2460186f7233c927e7db2dcc703c0e500b653ca82273b7bfad8045d85a470"
        );
        let tuple = alloy_dyn_abi::DynSolValue::Tuple(vec![
            alloy_dyn_abi::DynSolValue::String("https://indexer.example".into()),
            alloy_dyn_abi::DynSolValue::String("u10".into()),
            alloy_dyn_abi::DynSolValue::Address(alloy_primitives::Address::ZERO),
        ]);
        let encoded = format!("0x{}", hex::encode(tuple.abi_encode_params()));
        let decoded: serde_json::Value =
            serde_json::from_str(&evaluate(7, &["string,string,address", &encoded]).unwrap())
                .unwrap();
        assert_eq!(
            decoded,
            serde_json::json!([
                "https://indexer.example",
                "u10",
                "0x0000000000000000000000000000000000000000"
            ])
        );
        let invalid: Option<String> = conn
            .query_row(
                "SELECT TRY(nuthatch_abi_tuple('string,string,address', '0x'))",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(invalid.is_none());
        // More than one DuckDB chunk, including NULLs: do not assume a single vector or
        // reuse a previous invocation's result buffer.
        let count: u64 = conn.query_row("SELECT count(*) FROM (SELECT nuthatch_mul_div(CASE WHEN i % 2 = 0 THEN NULL ELSE CAST(i AS VARCHAR) END, '3', '3') AS v, i FROM range(10000) t(i)) WHERE v = CAST(i AS VARCHAR)", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 5000);
    }
}
