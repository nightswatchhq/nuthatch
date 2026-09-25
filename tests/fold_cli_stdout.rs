//! `nuthatch fold read` prints JSON lines on stdout, so nothing else may: its log lines, such as the
//! warning for a fold over a view that looks back into history, go to stderr.

use std::process::Command;

#[test]
fn fold_read_keeps_its_log_lines_off_stdout() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    std::fs::write(
        d.join("schema.json"),
        r#"{"tables":[{"table":"t","columns":[{"name":"block_number","storage":"u64"},{"name":"k","storage":"varchar"}]}]}"#,
    )
    .unwrap();
    std::fs::create_dir_all(d.join("views")).unwrap();
    std::fs::write(
        d.join("views/10-h.sql"),
        "CREATE VIEW h AS SELECT k, row_number() OVER (ORDER BY block_number) AS rn FROM t;",
    )
    .unwrap();
    std::fs::create_dir_all(d.join("folds")).unwrap();
    std::fs::write(
        d.join("folds/c.sql"),
        "SELECT CAST(count(*) AS UBIGINT) AS n FROM h",
    )
    .unwrap();
    std::fs::write(
        d.join("folds/folds.toml"),
        "[[fold]]\nname = \"c\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n",
    )
    .unwrap();
    nuthatch::store::Store::open(&d.join("nuthatch.redb"))
        .unwrap()
        .set_meta("sealed_through", "0")
        .unwrap();

    let out = Command::new(env!("CARGO_BIN_EXE_nuthatch"))
        .args(["fold", "read", "--dir"])
        .arg(d)
        .args(["--fold", "c", "--at", "1"])
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(out.status.success(), "{stderr}");
    assert!(
        stderr.contains("view `h` uses a window function"),
        "{stderr}"
    );
    assert!(!stdout.is_empty());
    for line in stdout.lines() {
        serde_json::from_str::<serde_json::Value>(line)
            .unwrap_or_else(|e| panic!("stdout line is not JSON ({e}): {line}"));
    }
}
