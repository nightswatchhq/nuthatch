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
        s.contains("glibc 2.34"),
        "README no longer states the measured glibc ABI floor (2.34), which is the number that \
         decides whether the binary runs (#946, #978)"
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

/// #978 - the ABI floor and the build baseline are different numbers, and conflating them is not a
/// rounding error: it excluded a platform this README lists as supported.
///
/// The README said *"glibc 2.35 or newer. The measured floor is 2.34; 2.35 is what the release is
/// built against, so it is the number to trust"* - and eleven lines later listed **RHEL 9**, which
/// ships glibc **2.34**. A reader on RHEL 9 was told both that they were below the requirement and
/// that their platform cleared it. The measured floor is the requirement; the build baseline is
/// trivia about our CI.
#[test]
fn the_abi_floor_is_not_stated_as_the_build_baseline() {
    let s = readme();
    let at = s.find("glibc 2.34").expect("the measured glibc floor");
    let window = &s[at.saturating_sub(200)..(at + 700).min(s.len())];

    assert!(
        window.contains("2.35"),
        "the README names an ABI floor but no longer says which glibc the release is built on. Both \
         belong: dropping one is how they get conflated again (#978):\n{window}"
    );
    for needed in ["built", "run"] {
        assert!(
            window.contains(needed),
            "the two glibc numbers are stated without saying which is which. 2.34 is what you need \
             to RUN the binary; 2.35 is what we BUILD it on. Without that distinction the larger \
             number reads as the requirement, which is the #978 defect:\n{window}"
        );
    }
    assert!(
        !s.contains("glibc 2.35 or newer"),
        "the README is back to stating the build baseline as the runtime requirement. It is not: \
         the binary references no symbol newer than GLIBC_2.34, and RHEL 9 - listed below as \
         supported - ships 2.34 (#978)."
    );
}

/// The supported-platform list has to agree with the floor, or one of them is wrong.
///
/// This is the assertion that would have caught #978 on the day it was written: the contradiction
/// was not in either claim alone but between them, and nothing compared the two.
#[test]
fn the_supported_platforms_clear_the_stated_floor() {
    let s = readme();
    // Oldest glibc among the platforms the README names, at the time of writing.
    // RHEL 9 and Amazon Linux 2023 both ship 2.34; Ubuntu 22.04 ships 2.35; Debian 12 ships 2.36.
    const OLDEST_LISTED: (&str, &str) = ("RHEL 9", "2.34");
    assert!(
        s.contains(OLDEST_LISTED.0),
        "the platform list no longer names {} - if it was dropped, this test's floor needs \
         recomputing rather than deleting (#978)",
        OLDEST_LISTED.0
    );
    assert!(
        s.contains(&format!("glibc {}", OLDEST_LISTED.1)),
        "the README lists {} as supported, which ships glibc {}, but states a different floor. One \
         of the two is wrong, and a user on that platform is the one who finds out (#978).",
        OLDEST_LISTED.0,
        OLDEST_LISTED.1
    );
}
