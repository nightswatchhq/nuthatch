#![no_main]

//! Byte-mutation fuzzer over raw ABI JSON text (nuthatch#290). This is the "an ABI is
//! attacker-supplied input the moment a nest is pulled from a registry" target: libFuzzer mutates
//! the seed corpus in `corpus/abi_json/` directly as JSON bytes, so it finds malformed *shapes*
//! (missing fields, wrong JSON types, truncated documents) that a structured generator would never
//! emit. `abi_arbitrary` covers the semantically-valid-but-extreme cases (absurd tuple depth,
//! duplicate topic0) this one is unlikely to stumble into by random byte flips alone.

use libfuzzer_sys::fuzz_target;
use nuthatch::registry::{ContractSpec, DecodeRegistry};

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(abi) = serde_json::from_str::<alloy_json_abi::JsonAbi>(text) else {
        return;
    };
    // A fixed dummy address: the address itself is never attacker-controlled input here, only the
    // ABI shape is under test.
    let address = alloy_primitives::Address::ZERO;
    let spec = ContractSpec {
        alias: "fuzz".to_string(),
        address,
        abi,
        events: Vec::new(),
    };
    // Hunting panics and unbounded allocation (nuthatch#290), not wrong answers - either outcome
    // of `build` is fine, a panic or an OOM abort is the only failure this target can observe.
    let _ = DecodeRegistry::build(vec![spec]);
});
