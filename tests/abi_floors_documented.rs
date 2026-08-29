//! #946 - the README must name **both** ABI floors the Linux binary actually has.
//!
//! It named glibc and nothing else. The binary also links `libstdc++.so.6`, because it embeds DuckDB,
//! and therefore requires `GLIBCXX_3.4.29` (GCC 11+). Every platform the README lists clears it, so
//! this was incompleteness rather than a broken promise - but a reader on new glibc with an old
//! libstdc++ meets a requirement nobody stated.
//!
//! The failure this guards is the one that produced it: a floor gets measured once, written once, and
//! the *other* floor is never noticed because nothing looks for it. If RFC-0042 ever removes DuckDB
//! the C++ line should go - and this test failing is how somebody finds out it should.

use std::path::PathBuf;

fn readme() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("README.md");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
fn the_install_section_names_both_abi_floors() {
    let s = readme();
    assert!(
        s.contains("glibc 2.35"),
        "README no longer states the glibc floor the release is built against (#946)"
    );
    assert!(
        s.contains("GLIBCXX_3.4.29"),
        "README states a glibc floor but not the libstdc++ one. The Linux binary links \
         libstdc++.so.6 because it embeds DuckDB, and needs GLIBCXX_3.4.29 (GCC 11+). A reader on \
         new glibc with an old libstdc++ meets a requirement we never mentioned (#946)."
    );
}

/// The C++ floor exists *because* of DuckDB. Saying so is what makes it removable knowledge rather
/// than a magic number, and it is the concrete form of RFC-0042's Tier 2 payoff.
#[test]
fn the_libstdcxx_floor_says_why_it_exists() {
    let s = readme();
    let at = s.find("GLIBCXX_3.4.29").expect("the libstdc++ floor");
    let window = &s[at.saturating_sub(400)..(at + 400).min(s.len())];
    assert!(
        window.contains("DuckDB"),
        "the libstdc++ requirement is stated without its cause. It exists because the binary embeds \
         DuckDB; without that, it reads as an arbitrary number nobody may ever remove:\n{window}"
    );
}
