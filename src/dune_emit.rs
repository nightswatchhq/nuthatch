//! RFC-0055 S1: `nuthatch emit dune`. One DuneSQL `SELECT` per event table, casting every column from
//! its physical type (`docs/reading-segments.md`) to the type §3.2 names. Offline and deterministic:
//! it reads `nuthatch.toml`, `schema.json` and `semantic.toml`, and writes nothing into the nest.
//!
//! Deliberately not `port_emit.rs` (RFC-0055 §7).

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::Path;

use crate::cli::EmitDuneArgs;
use crate::config::Config;
use crate::registry::{ColumnSchema, StorageKind, TableKind, TableSchema};
use crate::semantic::Semantic;

/// Every `StorageKind`, so a `schema.json` storage string can be read back through `as_str`.
pub const KINDS: [StorageKind; 11] = [
    StorageKind::Address,
    StorageKind::U64,
    StorageKind::I64,
    StorageKind::Word16,
    StorageKind::Word32,
    StorageKind::Bool,
    StorageKind::FixedBytes,
    StorageKind::Bytes,
    StorageKind::Str,
    StorageKind::Json,
    StorageKind::Hash32,
];

/// The implicit columns, in the order they are emitted: `(source, storage, emitted name)`. An empty
/// emitted name is not emitted.
const IMPLICIT: &[(&str, &str, &str)] = &[
    ("address", "address", "contract_address"),
    ("tx_hash", "bytes32", "evt_tx_hash"),
    ("log_index", "u64", "evt_index"),
    ("block_timestamp", "u64", "evt_block_time"),
    ("block_number", "u64", "evt_block_number"),
    ("block_hash", "bytes32", "evt_block_hash"),
    ("_seq", "u64", ""),
];

const VIEW_REASON: &str = "authored DuckDB SQL over the nest's `_dec` companions, which do not \
                           exist on the Dune side (RFC-0055 §5)";

#[derive(Deserialize)]
struct SchemaFile {
    tables: Vec<TableSchema>,
}

/// One emitted column.
struct OutCol {
    /// The name as written after `AS`: bare for implicit renames, double-quoted for ABI names.
    sql_name: String,
    name: String,
    ty: &'static str,
    expr: String,
    comment: String,
}

pub fn run(args: EmitDuneArgs) -> Result<()> {
    let files = emit(Path::new(&args.dir), &args.source)?;
    let out = Path::new(&args.out);
    std::fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    for (name, body) in &files {
        std::fs::write(out.join(name), body)
            .with_context(|| format!("write {}", out.join(name).display()))?;
    }
    println!(
        "✓ emitted {} DuneSQL quer{} and README.md into {}",
        files.len() - 1,
        if files.len() == 2 { "y" } else { "ies" },
        out.display()
    );
    Ok(())
}

/// Every output file, keyed by file name, `README.md` included.
pub fn emit(dir: &Path, source: &str) -> Result<BTreeMap<String, String>> {
    let config = Config::load_for_diagnostics(dir)?;
    let schema_path = dir.join("schema.json");
    let raw = std::fs::read_to_string(&schema_path)
        .with_context(|| format!("read {}", schema_path.display()))?;
    let schema: SchemaFile =
        serde_json::from_str(&raw).with_context(|| format!("parse {}", schema_path.display()))?;
    let Some(sem) = crate::semantic::load(dir)? else {
        bail!(
            "{} has no semantic.toml, whose footguns the emitter cross-checks against schema.json; \
             run `nuthatch schema` to generate it",
            dir.display()
        );
    };
    render(
        &config.nest.name,
        &config.nest.chain,
        source,
        &schema.tables,
        &sem,
        &authored_views(dir)?,
    )
}

fn authored_views(dir: &Path) -> Result<Vec<String>> {
    let views = dir.join("views");
    if !views.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(&views).with_context(|| format!("read {}", views.display()))? {
        let path = entry?.path();
        if path.is_file() && path.extension().and_then(|e| e.to_str()) == Some("sql") {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                out.push(format!("views/{name}"));
            }
        }
    }
    out.sort();
    Ok(out)
}

pub fn render(
    nest: &str,
    chain: &str,
    source: &str,
    tables: &[TableSchema],
    sem: &Semantic,
    views: &[String],
) -> Result<BTreeMap<String, String>> {
    if source.is_empty() {
        bail!("--source must name the Dune namespace the rows were uploaded into");
    }
    let mut files = BTreeMap::new();
    let mut emitted: BTreeMap<String, String> = BTreeMap::new();
    let mut skipped: BTreeMap<String, String> = BTreeMap::new();
    for view in views {
        skipped.insert(format!("`{view}`"), VIEW_REASON.to_string());
    }
    for t in tables {
        let kind = match t.kind {
            TableKind::Event => {
                let model = model_name(nest, chain, t)?;
                files.insert(format!("{model}.sql"), table_query(&model, source, t, sem)?);
                emitted.insert(format!("{model}.sql"), format!("dune.{source}.{}", t.table));
                continue;
            }
            TableKind::Block => "block",
            TableKind::Call => "call",
            TableKind::State => "state",
        };
        skipped.insert(
            format!("`{}`", t.table),
            format!("a {kind} table; RFC-0055 §3.3 names only event tables"),
        );
    }
    files.insert(
        "README.md".to_string(),
        readme(nest, chain, source, &emitted, &skipped),
    );
    Ok(files)
}

fn model_name(nest: &str, chain: &str, t: &TableSchema) -> Result<String> {
    let Some(event) = t.table.strip_prefix(&format!("{}__", t.alias)) else {
        bail!(
            "table `{}` is not named `{}__<event>`, so it has no event name to emit",
            t.table,
            t.alias
        );
    };
    Ok(format!("{nest}_{chain}_{}_evt_{event}", t.alias).to_lowercase())
}

fn table_query(model: &str, source: &str, t: &TableSchema, sem: &Semantic) -> Result<String> {
    let Some(ts) = sem.tables.get(&t.table) else {
        bail!(
            "semantic.toml has no [table.{}], so it is older than schema.json; run `nuthatch schema`",
            t.table
        );
    };
    check_footguns(t, &ts.footguns.big_ints)?;

    let mut cols: Vec<OutCol> = Vec::new();
    // Trino identifiers are case-insensitive, quoted or not, so collisions compare lowercased.
    let mut taken: BTreeMap<String, String> = BTreeMap::new();
    let mut claim = |name: &str, what: String| -> Result<()> {
        if let Some(prior) = taken.insert(name.to_lowercase(), what.clone()) {
            bail!(
                "table `{}`: {what} and {prior} would both be emitted as `{name}`; renaming either \
                 would make the query disagree with its ABI (RFC-0055 §5)",
                t.table
            );
        }
        Ok(())
    };

    let implicit: Vec<&ColumnSchema> = t
        .columns
        .iter()
        .filter(|c| c.sol_type == "implicit")
        .collect();
    for c in &implicit {
        if !IMPLICIT
            .iter()
            .any(|(n, s, _)| *n == c.name && *s == c.storage)
        {
            bail!(
                "table `{}` column `{}`: implicit column with storage `{}` is outside RFC-0055 §3.1",
                t.table,
                c.name,
                c.storage
            );
        }
    }
    for (src, storage, out) in IMPLICIT {
        if out.is_empty() || !implicit.iter().any(|c| c.name == *src) {
            continue;
        }
        let what = format!("`{out}`, the renamed `{src}`");
        claim(out, what)?;
        let comment = format!("renamed from `{src}`");
        let (ty, expr) = match *storage {
            "u64" if *src == "block_timestamp" => (
                "timestamp",
                format!("cast(from_unixtime({src}, 'UTC') as timestamp)"),
            ),
            "u64" => ("bigint", cast(src, "bigint")),
            _ => ("varbinary", from_hex(src)),
        };
        cols.push(OutCol {
            sql_name: out.to_string(),
            name: out.to_string(),
            ty,
            expr,
            comment,
        });
        if *src == "block_timestamp" {
            claim("evt_block_date", "`evt_block_date`".to_string())?;
            cols.push(OutCol {
                sql_name: "evt_block_date".to_string(),
                name: "evt_block_date".to_string(),
                ty: "date",
                expr: format!("cast(from_unixtime({src}, 'UTC') as date)"),
                comment: format!("the UTC date of `{src}`"),
            });
        }
    }

    for c in t.columns.iter().filter(|c| c.sol_type != "implicit") {
        let Some(kind) = KINDS.into_iter().find(|k| k.as_str() == c.storage) else {
            bail!(
                "table `{}` column `{}`: storage `{}` is outside RFC-0055 §3.1",
                t.table,
                c.name,
                c.storage
            );
        };
        let src = quote(&c.name);
        let mapped = param_type(kind, &c.sol_type, &src)
            .with_context(|| format!("table `{}` column `{}`", t.table, c.name))?;
        claim(&c.name, format!("parameter `{}`", c.name))?;
        let mut comment = ts
            .columns
            .get(&c.name)
            .map(|d| one_line(d))
            .unwrap_or_default();
        if kind == StorageKind::Hash32 {
            if !comment.is_empty() {
                comment.push(' ');
            }
            comment.push_str("Holds the keccak-256 hash of the indexed value, not the value.");
        }
        cols.push(OutCol {
            sql_name: src.clone(),
            name: c.name.clone(),
            ty: mapped.ty,
            expr: mapped.expr,
            comment,
        });
        if mapped.raw {
            let raw = format!("{}_raw", c.name);
            claim(&raw, format!("`{raw}`, the exact text of `{}`", c.name))?;
            cols.push(OutCol {
                sql_name: quote(&raw),
                name: raw,
                ty: "varchar",
                expr: src,
                comment: format!("the exact decimal text of `{}`", c.name),
            });
        }
    }

    let mut out = String::new();
    out.push_str(&format!("-- {model}\n"));
    out.push_str(&format!(
        "-- Generated by `nuthatch emit dune` from `{}` (RFC-0055). Regenerate; do not edit.\n",
        t.table
    ));
    let description = one_line(&ts.description);
    if !description.is_empty() {
        out.push_str(&format!("--\n-- {description}\n"));
    }
    let grain = one_line(&ts.grain);
    if !grain.is_empty() {
        out.push_str(&format!("-- Grain: {grain}\n"));
    }
    out.push_str("--\n-- Columns:\n");
    for c in &cols {
        if c.comment.is_empty() {
            out.push_str(&format!("--   {} {}\n", c.name, c.ty));
        } else {
            out.push_str(&format!("--   {} {}: {}\n", c.name, c.ty, c.comment));
        }
    }
    out.push_str("SELECT\n");
    let last = cols.len().saturating_sub(1);
    for (i, c) in cols.iter().enumerate() {
        let sep = if i == last { "" } else { "," };
        out.push_str(&format!("    {} AS {}{sep}\n", c.expr, c.sql_name));
    }
    out.push_str(&format!("FROM dune.{source}.{}\n", t.table));
    Ok(out)
}

/// Both directions: one of the two files is stale, and the output would be wrong either way.
fn check_footguns(t: &TableSchema, big_ints: &[String]) -> Result<()> {
    let is_wide = |c: &ColumnSchema| c.storage == "word16" || c.storage == "word32";
    for c in &t.columns {
        if is_wide(c) && !big_ints.contains(&c.name) {
            bail!(
                "table `{}` column `{}`: storage `{}` but not in [table.{}.footguns].big_ints; \
                 schema.json and semantic.toml disagree, run `nuthatch schema`",
                t.table,
                c.name,
                c.storage,
                t.table
            );
        }
    }
    for name in big_ints {
        match t.columns.iter().find(|c| &c.name == name) {
            Some(c) if is_wide(c) => {}
            found => bail!(
                "table `{}` column `{name}`: listed in [table.{}.footguns].big_ints but its storage \
                 is {}; schema.json and semantic.toml disagree, run `nuthatch schema`",
                t.table,
                t.table,
                found.map_or("absent".to_string(), |c| format!("`{}`", c.storage))
            ),
        }
    }
    Ok(())
}

struct Mapped {
    ty: &'static str,
    expr: String,
    raw: bool,
}

/// RFC-0055 §3.2. No wildcard: a new `StorageKind` does not compile until it has a row.
fn param_type(kind: StorageKind, sol_type: &str, src: &str) -> Result<Mapped> {
    let (ty, expr, raw) = match kind {
        StorageKind::Address
        | StorageKind::FixedBytes
        | StorageKind::Bytes
        | StorageKind::Hash32 => ("varbinary", from_hex(src), false),
        StorageKind::U64 => {
            match sol_type
                .strip_prefix("uint")
                .and_then(|b| b.parse::<u32>().ok())
            {
                Some(bits) if bits < 64 => ("bigint", cast(src, "bigint"), false),
                // A uint64 can exceed bigint's maximum.
                Some(64) => ("uint256", cast(src, "uint256"), false),
                _ => bail!("storage `u64` disagrees with sol_type `{sol_type}`"),
            }
        }
        StorageKind::I64 => ("bigint", cast(src, "bigint"), false),
        StorageKind::Word16 | StorageKind::Word32 => {
            if sol_type.starts_with("int") {
                ("int256", cast(src, "int256"), true)
            } else {
                ("uint256", cast(src, "uint256"), true)
            }
        }
        StorageKind::Bool => (
            "boolean",
            format!("case {src} when 'true' then true when 'false' then false end"),
            false,
        ),
        StorageKind::Str | StorageKind::Json => ("varchar", src.to_string(), false),
    };
    Ok(Mapped { ty, expr, raw })
}

/// `cast`, never `try_cast`: a silent NULL undercounts every sum over the column (RFC-0055 §3.2).
fn cast(src: &str, ty: &str) -> String {
    format!("cast({src} as {ty})")
}

/// The source is `0x`-prefixed; `cast(c as varbinary)` would encode the string's own bytes.
fn from_hex(src: &str) -> String {
    format!("from_hex(substr({src}, 3))")
}

fn quote(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

/// A newline in a description would end the `--` comment and put prose into the query.
fn one_line(s: &str) -> String {
    s.split(['\n', '\r'])
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn readme(
    nest: &str,
    chain: &str,
    source: &str,
    emitted: &BTreeMap<String, String>,
    skipped: &BTreeMap<String, String>,
) -> String {
    let mut out = format!(
        "# DuneSQL queries for `{nest}` on {chain}\n\n\
         Generated by `nuthatch emit dune` (RFC-0055). Each file is one DuneSQL `SELECT` over \
         `dune.{source}.<table>`, which must hold the nest's rows with every column `varchar` except \
         `block_number`, `log_index`, `_seq` and `block_timestamp`. Regenerate rather than edit.\n\n\
         ## Emitted\n\n"
    );
    if emitted.is_empty() {
        out.push_str("Nothing.\n");
    } else {
        out.push_str("| query | reads |\n| --- | --- |\n");
        for (file, reads) in emitted {
            out.push_str(&format!("| `{file}` | `{reads}` |\n"));
        }
    }
    out.push_str("\n## Not emitted\n\n");
    if skipped.is_empty() {
        out.push_str("Nothing.\n");
    } else {
        out.push_str("| name | reason |\n| --- | --- |\n");
        for (name, why) in skipped {
            out.push_str(&format!("| {name} | {why} |\n"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, sol_type: &str, indexed: bool) -> ColumnSchema {
        ColumnSchema {
            name: name.to_string(),
            sol_type: sol_type.to_string(),
            storage: StorageKind::from_sol(sol_type, indexed)
                .as_str()
                .to_string(),
            indexed,
            components: Vec::new(),
        }
    }

    fn table(params: Vec<ColumnSchema>) -> TableSchema {
        let mut columns = crate::registry::implicit_columns(true);
        columns.extend(params);
        TableSchema {
            table: "vault__deposit".to_string(),
            alias: "vault".to_string(),
            kind: TableKind::Event,
            event: "Deposit".to_string(),
            topic0: String::new(),
            function: String::new(),
            selector: String::new(),
            columns,
        }
    }

    fn render_one(t: TableSchema, sem: &Semantic) -> Result<String> {
        let files = render("n", "mainnet", "src", &[t], sem, &[])?;
        Ok(files["n_mainnet_vault_evt_deposit.sql"].clone())
    }

    fn generated(t: &TableSchema) -> Semantic {
        crate::semantic::generate(std::slice::from_ref(t), "n", "mainnet")
    }

    #[test]
    fn every_storage_kind_has_a_dune_type() {
        for kind in KINDS {
            let sol = match kind {
                StorageKind::U64 => "uint64",
                StorageKind::I64 => "int64",
                StorageKind::Word16 => "uint128",
                StorageKind::Word32 => "int256",
                _ => "whatever",
            };
            let m = param_type(kind, sol, "\"c\"").unwrap();
            assert!(!m.ty.is_empty() && m.expr.contains("\"c\""), "{kind:?}");
            assert_eq!(
                KINDS.iter().filter(|k| k.as_str() == kind.as_str()).count(),
                1,
                "{kind:?} is listed once and reads back from its storage string"
            );
        }
        // The sol types that produce each variant: a variant `from_sol` can reach but KINDS omits
        // would be refused as unknown storage.
        let reached: std::collections::BTreeSet<&str> = [
            ("address", false),
            ("uint8", false),
            ("int8", false),
            ("uint128", false),
            ("uint256", false),
            ("bool", false),
            ("bytes4", false),
            ("bytes", false),
            ("string", false),
            ("tuple", false),
            ("string", true),
        ]
        .into_iter()
        .map(|(ty, idx)| StorageKind::from_sol(ty, idx).as_str())
        .collect();
        let listed: std::collections::BTreeSet<&str> = KINDS.iter().map(|k| k.as_str()).collect();
        assert_eq!(reached, listed);
    }

    #[test]
    fn a_freshly_generated_semantic_never_trips_the_cross_check() {
        let t = table(vec![
            col("from", "address", true),
            col("amount", "uint256", false),
            col("delta", "int128", false),
            col("fee", "uint24", false),
            col("deadline", "uint64", false),
            col("tick", "int24", false),
            col("ok", "bool", false),
            col("label", "string", true),
            col("meta", "tuple", false),
            col("node", "bytes32", false),
            col("data", "bytes", false),
            col("name", "string", false),
        ]);
        render_one(t.clone(), &generated(&t)).unwrap();
    }

    #[test]
    fn unknown_storage_is_refused_by_column() {
        let mut t = table(vec![col("amount", "uint256", false)]);
        let sem = generated(&t);
        t.columns.push(ColumnSchema {
            storage: "decimal".to_string(),
            ..col("price", "uint256", false)
        });
        let err = format!("{:#}", render_one(t, &sem).unwrap_err());
        assert!(
            err.contains("column `price`") && err.contains("`decimal`"),
            "{err}"
        );
    }

    #[test]
    fn a_wide_column_missing_from_big_ints_is_refused() {
        let t = table(vec![
            col("amount", "uint256", false),
            col("fee", "uint128", false),
        ]);
        let mut sem = generated(&t);
        let ft = &mut sem.tables.get_mut("vault__deposit").unwrap().footguns;
        ft.big_ints.retain(|c| c != "fee");
        let err = format!("{:#}", render_one(t, &sem).unwrap_err());
        assert!(
            err.contains("column `fee`") && err.contains("big_ints"),
            "{err}"
        );
    }

    #[test]
    fn a_big_ints_entry_that_is_not_wide_is_refused() {
        let t = table(vec![
            col("amount", "uint256", false),
            col("fee", "uint24", false),
        ]);
        let mut sem = generated(&t);
        let ft = &mut sem.tables.get_mut("vault__deposit").unwrap().footguns;
        ft.big_ints.push("fee".to_string());
        let err = format!("{:#}", render_one(t, &sem).unwrap_err());
        assert!(
            err.contains("column `fee`") && err.contains("`u64`"),
            "{err}"
        );
    }

    #[test]
    fn a_parameter_named_like_an_emitted_column_is_refused() {
        let t = table(vec![col("evt_index", "uint256", false)]);
        let err = format!("{:#}", render_one(t.clone(), &generated(&t)).unwrap_err());
        assert!(
            err.contains("parameter `evt_index`") && err.contains("the renamed `log_index`"),
            "{err}"
        );
    }

    #[test]
    fn a_raw_column_colliding_with_a_parameter_is_refused() {
        let t = table(vec![
            col("amount", "uint256", false),
            col("Amount_raw", "string", false),
        ]);
        let err = format!("{:#}", render_one(t.clone(), &generated(&t)).unwrap_err());
        assert!(
            err.contains("parameter `Amount_raw`") && err.contains("the exact text of `amount`"),
            "{err}"
        );
    }

    #[test]
    fn a_u64_whose_sol_type_is_not_a_narrow_uint_is_refused() {
        let mut t = table(vec![]);
        t.columns.push(ColumnSchema {
            storage: "u64".to_string(),
            ..col("n", "uint256", false)
        });
        let err = format!(
            "{:#}",
            render_one(t.clone(), &Semantic::default()).unwrap_err()
        );
        assert!(
            err.contains("semantic.toml has no [table.vault__deposit]"),
            "{err}"
        );
        let err = format!("{:#}", render_one(t.clone(), &generated(&t)).unwrap_err());
        assert!(
            err.contains("column `n`") && err.contains("`uint256`"),
            "{err}"
        );
    }

    #[test]
    fn a_multiline_description_stays_inside_the_comment() {
        let t = table(vec![col("amount", "uint256", false)]);
        let mut sem = generated(&t);
        let ts = sem.tables.get_mut("vault__deposit").unwrap();
        ts.description = "first\nselect 1; --".to_string();
        ts.columns
            .insert("amount".to_string(), "a\r\nb".to_string());
        let sql = render_one(t, &sem).unwrap();
        for line in sql.lines().take_while(|l| !l.starts_with("SELECT")) {
            assert!(line.starts_with("--"), "{line}");
        }
        assert!(sql.contains("--   amount uint256: a b\n"), "{sql}");
    }
}
