//! #946 - the README must name every ABI floor the Linux binary actually has.
//!
//! It once named glibc and nothing else, while the binary also linked `libstdc++.so.6` for DuckDB
//! and needed `GLIBCXX_3.4.29`. DuckDB left in 4.1 and the C++ floor with it, so there is one floor
//! now and a stated second one would be the same fault the other way round.
//!
//! The floor itself is measured off the published artifact, not read off the build image. It was
//! 2.34 up to 4.0.2 and became 2.35 at 4.1.0, when `hypot` and `hypotf` picked up libm's
//! re-versioned symbols (#1649). This file pins the measured number; a release that moves it has to
//! come back here.

use std::path::PathBuf;

/// The highest versioned glibc symbol the published Linux artifact references.
const MEASURED_FLOOR: (u32, u32) = (2, 35);

fn readme() -> String {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("README.md");
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

#[test]
fn the_install_section_names_the_glibc_floor_and_no_other() {
    let s = readme();
    assert!(
        s.contains("glibc 2.35"),
        "README no longer states the measured glibc ABI floor (2.35), which is the number that \
         decides whether the binary runs (#946, #978, #1649)"
    );
    assert!(
        !s.contains("GLIBCXX_"),
        "README states a libstdc++ floor. The binary has linked no C++ runtime since DuckDB left \
         it in 4.1; a requirement nobody has is as wrong as one nobody stated (#946)."
    );
}

/// #978 - the ABI floor and the build baseline are different questions, and conflating them is not
/// a rounding error: it once excluded a platform the README listed as supported.
///
/// At 4.1.0 the two happen to be the same number. The README still has to say which is which, or
/// the next release that moves one of them will be documented by someone who cannot tell them apart.
#[test]
fn the_abi_floor_is_told_apart_from_the_build_baseline() {
    let s = readme();
    // The requirement bullet, not the first mention: the one-line summary near the top states the
    // number without the explanation, and the explanation is what this test is about.
    let at = s.find("- **glibc 2.35").expect("the requirement bullet");
    let window = &s[at..(at + 900).min(s.len())];

    for needed in ["built", "run"] {
        assert!(
            window.contains(needed),
            "the glibc number is stated without saying whether it is what you need to RUN the \
             binary or what we BUILD it on. Both belong, named: dropping the distinction is how \
             the #978 defect comes back:\n{window}"
        );
    }
}

/// The stated runtime requirement, parsed out of the requirement bullet rather than searched for.
///
/// # Why this is a parser and not a `contains` (#1026)
///
/// The first version of this test asserted that the README mentioned `RHEL 9` and `glibc 2.34`
/// *somewhere*. It could not tell the fixed README from the broken one, because the corrected text
/// mentioned **both** 2.34 and 2.35 - one as the requirement, one as the build baseline. So the
/// requirement is *identified*: the bullet in the runtime-requirements list that states a glibc
/// version with "or newer". Anything else in the document - the build baseline, the explanation,
/// the platform list - is prose, and prose is not a requirement.
fn stated_glibc_requirement(readme: &str) -> Option<(u32, u32)> {
    readme.lines().find_map(|l| {
        let t = l.trim_start();
        // The requirement bullet is the one under "needs one thing", `- **<thing>**`.
        let rest = t.strip_prefix("- **glibc ")?;
        // "2.35 or newer** - ..." ; require the "or newer" so a passing mention is not a requirement.
        let (ver, tail) = rest.split_once(' ')?;
        if !tail.starts_with("or newer") {
            return None;
        }
        let (maj, min) = ver.split_once('.')?;
        Some((maj.parse().ok()?, min.trim_end_matches('*').parse().ok()?))
    })
}

/// The sentence that lists the platforms as clearing the requirement. The README phrases it as
/// `<platforms> clear it.`; what follows on the same line may name the platforms that do not, so
/// only the text up to `clear it` is the list.
fn platforms_said_to_clear_it(readme: &str) -> &str {
    let line = readme
        .lines()
        .find(|l| l.contains("clear it"))
        .expect("the README names the platforms that clear the requirement, in a sentence ending `clear it.`");
    let end = line.find("clear it").expect("found above");
    &line[..end]
}

/// Distros a reader might run the binary on, and the glibc each ships. The README may list any of
/// them as clearing the requirement only if its glibc is at or above the stated floor.
const DISTROS: &[(&str, (u32, u32))] = &[
    ("RHEL 9", (2, 34)),
    ("Amazon Linux 2023", (2, 34)),
    ("Ubuntu 22.04", (2, 35)),
    ("Debian 12", (2, 36)),
];

/// The supported-platform list has to agree with the requirement, or one of them is wrong.
///
/// This is the assertion that would have caught #978 on the day it was written: the contradiction
/// was not in either claim alone but between them, and nothing compared the two. It is also what
/// #1649 was: the floor moved and the platform list did not.
#[test]
fn the_supported_platforms_clear_the_stated_requirement() {
    let s = readme();
    let req = stated_glibc_requirement(&s).unwrap_or_else(|| {
        panic!(
            "no glibc runtime requirement found in README.md. The requirement is the bullet reading \
             `- **glibc <version> or newer**`; if its shape changed, update the parser rather than \
             falling back to searching for the number anywhere in the file (#1026)."
        )
    });
    let clearing = platforms_said_to_clear_it(&s);

    let mut listed = 0;
    for (name, ships) in DISTROS {
        if !clearing.contains(name) {
            continue;
        }
        listed += 1;
        assert!(
            req <= *ships,
            "README states a runtime requirement of glibc {}.{} while listing {name} as clearing \
             it, which ships glibc {}.{}. One of the two is wrong, and a user on that platform is \
             the one who finds out (#978, #1026, #1649).",
            req.0,
            req.1,
            ships.0,
            ships.1
        );
    }
    assert!(
        listed > 0,
        "the platform list names none of the distros this test knows; if the list changed, teach \
         the test the new names rather than letting it pass on nothing (#978):\n{clearing}"
    );
}

/// And the shipped README must state the measured floor, or every assertion above is vacuous.
#[test]
fn the_shipped_readme_states_the_measured_floor() {
    assert_eq!(
        stated_glibc_requirement(&readme()),
        Some(MEASURED_FLOOR),
        "the shipped README must state glibc {}.{} as the runtime requirement: the floor measured \
         off the published artifact with `objdump -T`. A release that moves it updates this \
         constant and the README together (#1649).",
        MEASURED_FLOOR.0,
        MEASURED_FLOOR.1
    );
}

// -------------------------------------------------------------------------------------------
// #1026 regression controls. The parser is the thing under test here, so it is driven with
// synthetic README text - the shape a broken document takes, rather than the one we ship.
// -------------------------------------------------------------------------------------------

/// The exact defect #978 was: build baseline stated as the requirement, 2.34 explained beside it,
/// and RHEL 9 listed as clearing it. A `contains` check passed on this text, because both numbers
/// are present; the parser reads the stated requirement and the comparison against RHEL 9 fails.
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
    let clearing = platforms_said_to_clear_it(broken);
    let contradicted: Vec<_> = DISTROS
        .iter()
        .filter(|(name, ships)| clearing.contains(name) && req > *ships)
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(
        contradicted,
        ["RHEL 9", "Amazon Linux 2023"],
        "this is the #978 text: a 2.35 requirement over a list naming two 2.34 distros, and the \
         test must see both or it cannot distinguish the broken README from the fixed one"
    );
}

/// The #1649 shape: the floor moved to 2.35 and the platform list was left as it was.
#[test]
fn control_a_moved_floor_over_a_stale_platform_list_is_detected() {
    let stale = "\
- **glibc 2.35 or newer** - the measured ABI floor.\n\
\n\
Debian 12, Ubuntu 22.04, RHEL 9 and Amazon Linux 2023 clear it.\n";
    let req = stated_glibc_requirement(stale).expect("a requirement is stated");
    let clearing = platforms_said_to_clear_it(stale);
    assert!(
        DISTROS
            .iter()
            .any(|(name, ships)| clearing.contains(name) && req > *ships),
        "a 2.35 floor over a list still naming RHEL 9 must be caught (#1649)"
    );
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
