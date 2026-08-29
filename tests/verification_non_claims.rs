//! #890 - `docs/verification.md` is the one page in the repo whose entire purpose is being checkable,
//! so its **limits** need a checker as much as its results do.
//!
//! The specific limit: nuthatch follows an execution-layer RPC endpoint and hash-links the headers it
//! is served. That proves the blocks form an internally consistent chain. It does not prove that chain
//! is the one network consensus agreed on, because settling that needs consensus-layer data nuthatch
//! does not read. An endpoint that lies consistently is indistinguishable, to us, from one that tells
//! the truth.
//!
//! Edge & Node stated the same limitation publicly about their own system (RFC-0043 §7.3). It applies
//! to us in precisely the same way, and the failure mode this guards is the one the issue names: being
//! **quietly assumed away**. A section can be deleted in a tidy-up by someone who reads it as
//! defeatist, and nothing else in the repo would notice.

use std::path::PathBuf;

fn verification() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/verification.md");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
fn the_canonical_chain_limit_is_stated_as_a_non_claim() {
    let s = verification();

    assert!(
        s.contains("## What we do not claim") || s.contains("### What we do not claim"),
        "docs/verification.md has lost its non-claims section. A page that lists only what it \
         verifies is a page that overstates itself (#890)."
    );

    // The substance, not just the heading. A section that exists but no longer says what it was for
    // is the "assertion outlived its fact" shape from #913.
    for needed in ["consensus", "hash-link"] {
        assert!(
            s.to_ascii_lowercase().contains(needed),
            "the non-claims section no longer mentions `{needed}`. The limit is specifically that \
             hash-linking proves internal consistency and consensus-layer data is what would prove \
             canonicality; a section that has lost that says nothing (#890)."
        );
    }
}

/// The reorg step says "converges to canonical state", which is true of the chain we were served and
/// is the sentence a reader is most likely to over-read. It must point at the limit rather than leave
/// the word unqualified.
#[test]
fn the_reorg_step_qualifies_what_canonical_means() {
    let s = verification();
    let step = s
        .split_once("**2.3 A reorg converges**")
        .expect("step 2.3 has been renamed or removed")
        .1;
    let step = &step[..step.len().min(1200)];
    assert!(
        step.contains("what the configured endpoint served"),
        "step 2.3 uses `canonical` without saying whose canonical it is. It means what the endpoint \
         served, which is not necessarily what consensus agreed (#890):\n{step}"
    );
}
