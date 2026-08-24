//! #661: launch copy that still describes 2.5.0. The RFC-index half of that issue is done; these
//! three files were the rest. A phrase returning here is a published claim that is false.

use std::path::PathBuf;

fn launch_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("docs/launch")
}

const STALE: &[&str] = &[
    "has no executor yet",
    "cannot resolve yet",
    "aren't indexed at all yet",
    "It's v2.5.0",
    "currently reasoned from first principles",
];

#[test]
fn launch_copy_does_not_describe_2_5_0() {
    let files = ["show-hn.md", "port-queue-nest.md", "community.md"];
    let mut offenders = Vec::new();
    for name in files {
        let text = std::fs::read_to_string(launch_dir().join(name))
            .unwrap_or_else(|e| panic!("read docs/launch/{name}: {e}"));
        for phrase in STALE {
            if text.contains(phrase) {
                offenders.push(format!("{name}: still contains `{phrase}`"));
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "launch copy still describes a binary we no longer ship:\n{}",
        offenders.join("\n")
    );
}
