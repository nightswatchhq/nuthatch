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
use nuthatch::registry::{ContractSpec, DecodeRegistry};

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
    let mut components = serde_json::json!([{"name": "leaf", "type": "uint256", "indexed": false}]);
    for i in 0..depth {
        components = serde_json::json!([{
            "name": format!("t{i}"),
            "type": "tuple",
            "components": components,
            "indexed": false,
        }]);
    }
    components
}

fuzz_target!(|input: FuzzInput| {
    // Still absurd (up to 4095 levels), but capped so corpus entries don't balloon the JSON text
    // itself into gigabytes before nuthatch's parser even runs.
    let depth = input.tuple_depth % 4096;
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
