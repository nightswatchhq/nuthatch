#![no_main]

//! Structured ABI fuzzer (nuthatch#290): unlike `abi_json`'s raw byte mutation, this drives a
//! small `Arbitrary`-derived generator so libFuzzer reliably reaches the two shapes raw JSON
//! mutation rarely stumbles into on its own:
//!   - absurd tuple depth: a `Deep` event whose single param is a tuple nested `tuple_depth`
//!     levels deep, built iteratively (not recursively) so the *generator* can't stack-overflow
//!     before nuthatch's own decode path gets a turn.
//!   - duplicate topic0: `duplicate_count` copies of an identically-named, identically-typed `Dup`
//!     event, which collide on `keccak(signature)` by construction.
//! Also throws in a `uint256[huge]` fixed-array param type, since a component-count multiplied by
//! a fuzzed array size is exactly the kind of arithmetic that overflows or over-allocates.

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use nuthatch_decode::registry::{ContractSpec, DecodeRegistry};

#[derive(Arbitrary, Debug)]
struct FuzzInput {
    tuple_depth: u16,
    duplicate_count: u8,
    param_types: Vec<SimpleType>,
    huge_array_size: u32,
}

#[derive(Arbitrary, Debug, Clone, Copy)]
enum SimpleType {
    Uint256,
    Int256,
    Address,
    Bool,
    Bytes32,
    Bytes,
    String,
    DynArray,
    FixedHugeArray,
}

impl SimpleType {
    fn sol(self, huge: u32) -> String {
        match self {
            Self::Uint256 => "uint256".to_string(),
            Self::Int256 => "int256".to_string(),
            Self::Address => "address".to_string(),
            Self::Bool => "bool".to_string(),
            Self::Bytes32 => "bytes32".to_string(),
            Self::Bytes => "bytes".to_string(),
            Self::String => "string".to_string(),
            Self::DynArray => "uint256[]".to_string(),
            Self::FixedHugeArray => format!("uint256[{huge}]"),
        }
    }
}

/// Builds `inputs: [{"type":"tuple","components":[{"type":"tuple","components":[...]}]}]`,
/// bottoming out in a plain `uint256` leaf, `depth` levels deep. Iterative so the depth the fuzzer
/// asks for is exactly the depth nuthatch's decode path has to deal with, not bounded by our own
/// call stack.
fn nested_tuple_inputs(depth: u16) -> serde_json::Value {
    let mut components = serde_json::json!([{"name": "leaf", "type": "uint256"}]);
    for i in 0..depth {
        components = serde_json::json!([{
            "name": format!("t{i}"),
            "type": "tuple",
            "components": components,
        }]);
    }
    // "indexed" is only valid on the outermost event input, never on nested tuple components -
    // alloy-json-abi rejects it there ("indexed is not supported in params"). Setting it at
    // every level made every tuple_depth > 0 input fail ABI parsing before DecodeRegistry::build
    // ever ran, so the depth-fuzzing this target exists for (nuthatch#290) had zero coverage.
    if let serde_json::Value::Array(arr) = &mut components {
        if let Some(serde_json::Value::Object(obj)) = arr.get_mut(0) {
            obj.insert("indexed".to_string(), serde_json::Value::Bool(false));
        }
    }
    components
}

fuzz_target!(|input: FuzzInput| {
    // Capped at 256, not the old 4096 (nuthatch#603): under the ASan+coverage-instrumented build
    // this target actually runs as (cargo-fuzz's default, and what CI's fuzz-smoke job now uses
    // post-#593/#614), the recursive `Param` deserialize call stack for a nested-tuple `components`
    // chain stack-overflows and aborts the whole fuzzer process - measured on a stock dev box,
    // crash onset between depth 1700 (5.5s, survives) and depth 1800 (stack-overflow). 256 is a
    // >100x safety margin below that boundary, well clear of any future ASan/runner-stack variance,
    // and still two orders of magnitude past any nesting a real Solidity ABI would ever use. Before
    // this cap, a single near-max-depth draw could cost 20-60s+ under ASan (or crash outright),
    // which is why a 180s/300000-run CI budget was completing 99-108 executed units total - the
    // depth space alone was consuming the whole run. See nuthatch#603 / NIG-257 for the full
    // before/after measurement (also fixes the "indexed" placement bug above, without which
    // DecodeRegistry::build was never reached for any tuple_depth > 0 in the first place).
    let depth = input.tuple_depth % 256;
    let huge = input.huge_array_size;

    let mut events = vec![serde_json::json!({
        "type": "event",
        "name": "Deep",
        "anonymous": false,
        "inputs": nested_tuple_inputs(depth),
    })];

    for (i, ty) in input.param_types.iter().take(16).enumerate() {
        events.push(serde_json::json!({
            "type": "event",
            "name": format!("Ev{i}"),
            "anonymous": false,
            "inputs": [{"name": "p", "type": ty.sol(huge), "indexed": i % 2 == 0}],
        }));
    }

    for _ in 0..(input.duplicate_count % 8) {
        events.push(serde_json::json!({
            "type": "event",
            "name": "Dup",
            "anonymous": false,
            "inputs": [{"name": "a", "type": "uint256", "indexed": true}],
        }));
    }

    let Ok(abi) =
        serde_json::from_value::<alloy_json_abi::JsonAbi>(serde_json::Value::Array(events))
    else {
        return;
    };
    let spec = ContractSpec {
        alias: "fuzz".to_string(),
        address: alloy_primitives::Address::ZERO,
        abi,
        events: Vec::new(),
    };
    let _ = DecodeRegistry::build(vec![spec]);
});
