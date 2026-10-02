//! #946 - the README must name every ABI floor the Linux binary actually has.
//!
//! It once named glibc and nothing else, while the binary also linked `libstdc++.so.6` for DuckDB
//! and needed `GLIBCXX_3.4.29`. DuckDB left in 4.1 and the C++ floor with it, so there is one floor
//! now and a stated second one would be the same fault the other way round.

use std::path::PathBuf;

fn readme() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("README.md");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
fn the_install_section_names_the_glibc_floor_and_no_other() {
    let s = readme();
    assert!(
        s.contains("glibc 2.35"),
        "README no longer states the measured glibc ABI floor (2.35 since 4.1.0, which references \
         hypot at GLIBC_2.35), which is the number that decides whether the binary runs (#946, #978)"
    );
    assert!(
        !s.contains("GLIBCXX_"),
        "README states a libstdc++ floor. The binary has linked no C++ runtime since DuckDB left \
         it in 4.1; a requirement nobody has is as wrong as one nobody stated (#946)."
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
    let at = s.find("glibc 2.35").expect("the measured glibc floor");
    let window = &s[at.saturating_sub(200)..(at + 700).min(s.len())];

    assert!(
        window.contains("2.34"),
        "the README names the 4.1.0 floor but no longer says it was 2.34 before, with the symbol that \
         raised it. Both belong: a floor without its cause is a number nobody may ever lower (#978):\n{window}"
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
        !s.contains("RHEL 9 and Amazon Linux 2023 clear"),
        "the README lists RHEL 9 as clearing the floor. It ships glibc 2.34 and 4.1.0 needs 2.35 (#978)."
    );
}

/// The stated runtime requirement, parsed out of the requirement bullet rather than searched for.
///
/// # Why this is a parser and not a `contains` (#1026)
///
/// The first version of this test asserted that the README mentioned `RHEL 9` and `glibc 2.34`
/// *somewhere*. It could not tell the fixed README from the broken one, because the corrected text
/// mentions **both** 2.34 and 2.35 - one as the requirement, one as the build baseline. A README
/// that said `glibc 2.35 or newer` as the requirement and explained 2.34 in the following sentence
/// passed it, which is precisely the #978 defect it was written to catch.
///
/// So the requirement is *identified*: the bullet in the runtime-requirements list that states a
/// glibc version with "or newer". Anything else in the document - the build baseline, the
/// explanation, the platform list - is prose, and prose is not a requirement.
fn stated_glibc_requirement(readme: &str) -> Option<(u32, u32)> {
    readme.lines().find_map(|l| {
        let t = l.trim_start();
        // The requirement bullet is the one under "needs one thing", `- **<thing>**`.
        let rest = t.strip_prefix("- **glibc ")?;
        // "2.34 or newer** - ..." ; require the "or newer" so a passing mention is not a requirement.
        let (ver, tail) = rest.split_once(' ')?;
        if !tail.starts_with("or newer") {
            return None;
        }
        let (maj, min) = ver.split_once('.')?;
        Some((maj.parse().ok()?, min.trim_end_matches('*').parse().ok()?))
    })
}

/// The supported-platform list has to agree with the requirement, or one of them is wrong.
///
/// This is the assertion that would have caught #978 on the day it was written: the contradiction
/// was not in either claim alone but between them, and nothing compared the two.
#[test]
fn the_supported_platforms_clear_the_stated_requirement() {
    let s = readme();
    // Oldest glibc among the platforms the README says clear the floor, at the time of writing.
    // Ubuntu 22.04 ships 2.35; Debian 12 ships 2.36. RHEL 9 and Amazon Linux 2023 (2.34) are named
    // as not clearing it since 4.1.0.
    const OLDEST_LISTED: (&str, (u32, u32)) = ("Ubuntu 22.04", (2, 35));

    assert!(
        s.contains(OLDEST_LISTED.0),
        "the platform list no longer names {} - if it was dropped, this test's floor needs \
         recomputing rather than deleting (#978)",
        OLDEST_LISTED.0
    );

    let req = stated_glibc_requirement(&s).unwrap_or_else(|| {
        panic!(
            "no glibc runtime requirement found in README.md. The requirement is the bullet reading \
             `- **glibc <version> or newer**`; if its shape changed, update the parser rather than \
             falling back to searching for the number anywhere in the file (#1026)."
        )
    });

    assert!(
        req <= OLDEST_LISTED.1,
        "README states a runtime requirement of glibc {}.{} while listing {} as supported, which \
         ships glibc {}.{}. One of the two is wrong, and a user on that platform is the one who \
         finds out (#978, #1026).",
        req.0,
        req.1,
        OLDEST_LISTED.0,
        OLDEST_LISTED.1 .0,
        OLDEST_LISTED.1 .1
    );
}

// -------------------------------------------------------------------------------------------
// #1026 regression controls. The parser is the thing under test here, so it is driven with
// synthetic README text - the shape a broken document takes, rather than the one we ship.
// -------------------------------------------------------------------------------------------

/// The exact defect #978 was: build baseline stated as the requirement, 2.34 explained beside it.
///
/// The previous `contains`-based test passed on this text, because both numbers are present.
#[test]
fn control_the_original_contradictory_readme_is_detected() {
    let broken = "\
- **glibc 2.35 or newer.** The measured floor is 2.34; 2.35 is what the release is built against, \
so it is the number to trust.\n\
\n\
Debian 12, Ubuntu 22.04, RHEL 9 and Amazon Linux 2023 clear it.\n";
    let req = stated_glibc_requirement(broken).expect("the broken form still states a requirement");
    assert_eq!(
        req,
        (2, 35),
        "the parser must read the *stated* requirement, not the nearby prose"
    );
    assert!(
        req > (2, 34),
        "this is the #978 text and it must compare as stricter than RHEL 9's 2.34, or the test \
         cannot distinguish the broken README from the fixed one"
    );
    assert!(
        broken.contains("2.34"),
        "the broken text mentions 2.34 as well - which is why a `contains` check could never see \
         the defect, and why this parses instead"
    );
}

/// An alternative contradictory spelling, as the issue asks for.
#[test]
fn control_a_differently_worded_contradiction_is_also_detected() {
    for broken in [
        "- **glibc 2.36 or newer** - required at runtime.\n",
        "- **glibc 2.39 or newer** - what the CI image ships.\n",
    ] {
        let req = stated_glibc_requirement(broken).expect("a requirement is stated");
        assert!(
            req > (2, 34),
            "`{broken}` states a requirement above RHEL 9's floor and must be caught"
        );
    }
}

/// The false-positive direction: a passing *mention* is not a requirement.
#[test]
fn control_a_mention_without_or_newer_is_not_read_as_the_requirement() {
    let prose = "- **glibc 2.39** appears in our CI image, which is not a requirement.\n";
    assert_eq!(
        stated_glibc_requirement(prose),
        None,
        "a bullet without `or newer` is a statement of fact, not a runtime requirement; reading it \
         as one would make the test fire on documentation that is correct"
    );
}

/// And the shipped README must parse, or every assertion above is vacuous.
#[test]
fn the_shipped_readme_states_a_parseable_requirement() {
    let req = stated_glibc_requirement(&readme());
    assert_eq!(
        req,
        Some((2, 35)),
        "the shipped README must state glibc 2.35 as the runtime requirement: the floor measured on \
         the 4.1.0 artifact (hypot at GLIBC_2.35)"
    );
}
