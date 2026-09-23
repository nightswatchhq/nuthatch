//! `nuthatch nest nid` must print the identity a nest's data is actually stored under.
//!
//! The comparison is against `nuthatch migrate`, which is what moves a nest into `data/<nid>/`. A test
//! that compared the command with the function it wraps would pass whatever that function returned.

use std::path::Path;
use std::process::Command;

fn nuthatch(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_nuthatch"))
        .args(args)
        .env("NO_COLOR", "1")
        .output()
        .expect("running nuthatch")
}

fn write_nest(nest: &Path, name: &str) {
    std::fs::create_dir_all(nest).unwrap();
    std::fs::write(
        nest.join("nuthatch.toml"),
        format!(
            "[nest]\nname = \"{name}\"\nchain = \"arbitrum-one\"\nchain_id = 42161\n\
             rpc_urls = []\n\n[[contracts]]\nalias = \"t\"\n\
             address = \"0xaf88d065e77c8cc2239327c5edb3a432268e5831\"\nabi = \"abi.json\"\n"
        ),
    )
    .unwrap();
    std::fs::write(nest.join("abi.json"), "[]").unwrap();
}

#[test]
fn nest_nid_names_the_directory_migrate_stores_the_data_under() {
    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    std::fs::write(
        root.join("mounts.toml"),
        "[runtime]\nname = \"r\"\nchain = \"arbitrum-one\"\nchain_id = 42161\n\
         rpc_urls = []\nnests = [\"usdc\"]\n",
    )
    .unwrap();
    let nest = root.join("nests").join("usdc");
    write_nest(&nest, "usdc");

    let out = nuthatch(&["nest", "nid", "--dir", nest.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "nest nid failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed = String::from_utf8(out.stdout).unwrap().trim().to_string();
    assert_eq!(
        printed.len(),
        64,
        "a full NID, not an abbreviation: {printed:?}"
    );

    let out = nuthatch(&["migrate", "--dir", root.to_str().unwrap()]);
    assert!(
        out.status.success(),
        "migrate failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        root.join("data")
            .join(&printed)
            .join("nuthatch.toml")
            .is_file(),
        "migrate did not store the nest under the printed NID {printed}"
    );
    let mounts = std::fs::read_to_string(root.join("mounts.toml")).unwrap();
    assert!(
        mounts.contains(&format!("nid = \"{printed}\"")),
        "the mount record names a different NID:\n{mounts}"
    );
}

#[test]
fn nest_nid_refuses_a_directory_that_is_not_a_nest() {
    let empty = tempfile::tempdir().unwrap();
    let out = nuthatch(&["nest", "nid", "--dir", empty.path().to_str().unwrap()]);
    assert!(!out.status.success());
    assert!(
        out.stdout.is_empty(),
        "printed something that looks like an identity"
    );
}
