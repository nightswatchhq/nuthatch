//! RFC-0059: checkpointed folds. Built only with the `folds` feature (which `graph` enables).
//!
//! A fold is one SELECT in `folds/<name>.sql`, declared in `folds/folds.toml`. Loading binds every
//! fold against the nest's surface without reading a row, and refuses anything a checkpoint could not
//! carry exactly: a volatile call, an input it cannot enumerate, or an output that is not its carry.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::analytics;
use crate::registry::TableSchema;

pub const FOLDS_DIR: &str = "folds";
pub const FOLDS_TOML: &str = "folds.toml";

/// What a checkpoint may hold: types that round-trip exactly through Parquet and sort
/// deterministically. Big integers are carried as VARCHAR and cast in the step.
const CARRY_TYPES: &[&str] = &[
    "VARCHAR", "BOOLEAN", "BIGINT", "UBIGINT", "INTEGER", "UINTEGER", "SMALLINT", "BLOB",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FoldKey {
    Columns(Vec<String>),
    Singleton,
    Unkeyed,
}

#[derive(Debug, Clone)]
pub struct Fold {
    pub name: String,
    pub file: String,
    pub sql: String,
    pub key: FoldKey,
    /// `(column, DuckDB type)` in output order, types as DuckDB spells them.
    pub carry: Vec<(String, String)>,
    pub max_rows: u64,
    /// Every fact table and view the fold reads, through views. Other folds and carries excluded.
    pub reaches: BTreeSet<String>,
    /// Earlier folds this one reads, at `hi` or through their carry.
    pub deps: BTreeSet<String>,
}

#[derive(Debug, Clone, Default)]
pub struct FoldSet {
    /// In load order, which is dependency order.
    pub folds: Vec<Fold>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FoldsToml {
    #[serde(default)]
    fold: Vec<FoldDecl>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FoldDecl {
    name: String,
    key: KeyDecl,
    carry: Vec<String>,
    max_rows: u64,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum KeyDecl {
    Columns(Vec<String>),
    Word(String),
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

/// `10-deployment_signal.sql` is the fold `deployment_signal`; the prefix only orders files, as in
/// `views/`.
fn fold_name_of(file: &str) -> Option<&str> {
    let stem = file.strip_suffix(".sql")?;
    Some(match stem.split_once('-') {
        Some((prefix, rest))
            if !prefix.is_empty() && prefix.bytes().all(|b| b.is_ascii_digit()) =>
        {
            rest
        }
        _ => stem,
    })
}

impl FoldSet {
    /// Load and validate `folds/`. An absent directory is an empty set. Every problem is a refusal.
    pub fn load(dir: &Path, schema: &[TableSchema]) -> Result<FoldSet> {
        let root = dir.join(FOLDS_DIR);
        if !root.exists() {
            return Ok(FoldSet::default());
        }
        let raw = std::fs::read_to_string(root.join(FOLDS_TOML))
            .with_context(|| format!("{FOLDS_DIR}/ needs {FOLDS_DIR}/{FOLDS_TOML}"))?;
        let decls: FoldsToml =
            toml::from_str(&raw).with_context(|| format!("parsing {FOLDS_DIR}/{FOLDS_TOML}"))?;

        let mut files: Vec<String> = std::fs::read_dir(&root)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|f| f.ends_with(".sql"))
            .collect();
        files.sort();

        let mut by_name: BTreeMap<String, String> = BTreeMap::new();
        for file in &files {
            let name = fold_name_of(file).unwrap_or_default();
            if !is_identifier(name) {
                bail!("{FOLDS_DIR}/{file}: `{name}` is not a fold name (lowercase letters, digits, _)");
            }
            if let Some(other) = by_name.insert(name.to_string(), file.clone()) {
                bail!("{FOLDS_DIR}/{file} and {FOLDS_DIR}/{other} both define the fold `{name}`");
            }
        }
        let mut decl_by_name: BTreeMap<String, FoldDecl> = BTreeMap::new();
        for d in decls.fold {
            if !by_name.contains_key(&d.name) {
                bail!(
                    "{FOLDS_DIR}/{FOLDS_TOML} declares `{}`, which has no .sql file in {FOLDS_DIR}/",
                    d.name
                );
            }
            let name = d.name.clone();
            if decl_by_name.insert(name.clone(), d).is_some() {
                bail!("{FOLDS_DIR}/{FOLDS_TOML} declares `{name}` twice");
            }
        }

        let binder = analytics::FoldBinder::new(dir, schema)?;
        let surface = binder.relations()?;
        let view_bodies = analytics::nest_view_bodies(dir);

        let mut set = FoldSet::default();
        for file in &files {
            let name = fold_name_of(file).unwrap_or_default().to_string();
            let at = format!("{FOLDS_DIR}/{file}");
            let decl = decl_by_name
                .remove(&name)
                .with_context(|| format!("{at} has no [[fold]] in {FOLDS_DIR}/{FOLDS_TOML}"))?;
            let sql = std::fs::read_to_string(root.join(file))?;
            let fold = load_one(
                &binder,
                dir,
                &surface,
                &view_bodies,
                &set,
                &at,
                name,
                decl,
                sql,
            )?;
            // Later folds bind against this one at `hi`, and against its carry.
            binder.execute(&format!(
                "CREATE TABLE \"{0}\" AS {1}; CREATE TABLE \"{0}__carry\" AS {1};",
                fold.name,
                empty_relation(&fold.carry)
            ))?;
            set.folds.push(Fold {
                file: file.clone(),
                ..fold
            });
        }
        Ok(set)
    }
}

/// Walks a fold set forward one window at a time: facts in `(lo, hi]`, each fold's output at `lo` as
/// its carry. The first step starts from genesis with empty carries.
pub struct Stepper<'a> {
    set: &'a FoldSet,
    eval: analytics::FoldEvaluator,
    wanted: BTreeSet<String>,
    at: Option<u64>,
}

impl FoldSet {
    pub fn stepper<'a>(&'a self, dir: &Path, schema: &[TableSchema]) -> Result<Stepper<'a>> {
        Ok(Stepper {
            set: self,
            eval: analytics::FoldEvaluator::new(dir, schema)?,
            wanted: self.folds.iter().flat_map(|f| f.reaches.clone()).collect(),
            at: None,
        })
    }
}

impl Stepper<'_> {
    /// The block the folds were last evaluated at, or `None` before the first step.
    pub fn at(&self) -> Option<u64> {
        self.at
    }

    /// Evaluate every fold at `hi` from its state at the previous step. All or nothing: a refused
    /// step leaves every fold as it was at the previous step, so a retry cannot fold a window twice.
    pub fn step_to(
        &mut self,
        hot: &analytics::HotRows,
        sealed_through: u64,
        hi: u64,
    ) -> Result<()> {
        if let Some(lo) = self.at {
            if hi <= lo {
                bail!("a fold steps forward: {hi} is not after {lo}");
            }
        }
        self.eval.execute("BEGIN TRANSACTION")?;
        match self.advance(hot, sealed_through, hi) {
            Ok(()) => {
                self.eval.execute("COMMIT")?;
                self.at = Some(hi);
                Ok(())
            }
            Err(e) => {
                self.eval.execute("ROLLBACK")?;
                Err(e)
            }
        }
    }

    fn advance(&mut self, hot: &analytics::HotRows, sealed_through: u64, hi: u64) -> Result<()> {
        for f in &self.set.folds {
            let carry = match self.at {
                Some(_) => format!("SELECT * FROM \"{}\"", f.name),
                None => empty_relation(&f.carry),
            };
            self.eval.execute(&format!(
                "CREATE OR REPLACE TABLE \"{}__carry\" AS {carry}",
                f.name
            ))?;
        }
        self.eval
            .bind_window(hot, sealed_through, self.at, hi, &self.wanted)?;
        for f in &self.set.folds {
            let step = format!("__step_{}", f.name);
            self.eval
                .execute(&format!("CREATE OR REPLACE TABLE \"{step}\" AS {}", f.sql))
                .with_context(|| format!("evaluating fold `{}` at {hi}", f.name))?;
            // A keyed fold emits the keys its window touched; every other key passes through as it was.
            let output = match &f.key {
                FoldKey::Columns(cols) => {
                    let same: Vec<String> = cols
                        .iter()
                        .map(|c| format!("s.\"{c}\" IS NOT DISTINCT FROM c.\"{c}\""))
                        .collect();
                    format!(
                        "SELECT * FROM \"{step}\" UNION ALL SELECT * FROM \"{0}__carry\" c \
                         WHERE NOT EXISTS (SELECT 1 FROM \"{step}\" s WHERE {1})",
                        f.name,
                        same.join(" AND ")
                    )
                }
                FoldKey::Singleton | FoldKey::Unkeyed => format!("SELECT * FROM \"{step}\""),
            };
            self.eval.execute(&format!(
                "CREATE OR REPLACE TABLE \"{}\" AS {output}",
                f.name
            ))?;
            let rows = self.eval.count(&f.name)?;
            if rows > f.max_rows {
                bail!(
                    "fold `{}` holds {rows} rows at {hi}, over its declared max_rows {}",
                    f.name,
                    f.max_rows
                );
            }
        }
        Ok(())
    }

    /// A fold's state at the last step, ordered by every column so it compares deterministically.
    pub fn rows(&self, fold: &str) -> Result<Vec<serde_json::Value>> {
        let f = self
            .set
            .folds
            .iter()
            .find(|f| f.name == fold)
            .with_context(|| format!("no fold `{fold}`"))?;
        let order: Vec<String> = (1..=f.carry.len()).map(|i| i.to_string()).collect();
        self.eval.rows(&format!(
            "SELECT * FROM \"{fold}\" ORDER BY {}",
            order.join(", ")
        ))
    }
}

fn empty_relation(cols: &[(String, String)]) -> String {
    let select: Vec<String> = cols
        .iter()
        .map(|(c, t)| format!("CAST(NULL AS {t}) AS \"{c}\""))
        .collect();
    format!("SELECT {} WHERE false", select.join(", "))
}

#[allow(clippy::too_many_arguments)]
fn load_one(
    binder: &analytics::FoldBinder,
    dir: &Path,
    surface: &BTreeSet<String>,
    view_bodies: &BTreeMap<String, String>,
    earlier: &FoldSet,
    at: &str,
    name: String,
    decl: FoldDecl,
    sql: String,
) -> Result<Fold> {
    let carry_name = format!("{name}__carry");
    for taken in [&name, &carry_name] {
        if surface.contains(taken) {
            bail!("{at}: `{taken}` is already a table or view in this nest");
        }
    }

    // Exactly one SELECT, parsed. SQL that will not parse cannot be checked for volatility, so it is
    // refused rather than waved through.
    let kinds = binder
        .statement_kinds(&sql)
        .with_context(|| at.to_string())?;
    if !matches!(
        kinds.as_slice(),
        [k] if k == "SELECT_NODE" || k == "SET_OPERATION_NODE"
    ) {
        bail!("{at}: a fold is exactly one SELECT");
    }
    analytics::reject_file_access(&sql).with_context(|| at.to_string())?;
    analytics::reject_replacement_scan(&sql).with_context(|| at.to_string())?;

    // What it reads. A fold whose inputs cannot be enumerated cannot be content-addressed.
    let referenced = binder
        .base_tables(&sql)
        .with_context(|| format!("{at}: cannot enumerate the tables it reads"))?;
    let mut deps = BTreeSet::new();
    let mut direct = BTreeSet::new();
    for t in &referenced {
        let base = t.strip_suffix("__carry").unwrap_or(t);
        if base == name {
            if t == &name {
                bail!("{at}: reads itself; its previous state is `{carry_name}`");
            }
            continue;
        }
        if earlier.folds.iter().any(|f| f.name == base) {
            deps.insert(base.to_string());
            continue;
        }
        direct.insert(t.clone());
    }
    let reaches = binder.reachable(dir, &direct).with_context(|| {
        format!(
            "{at}: cannot enumerate what it reads through views \
             (factory `__children` views are not bound inside a fold)"
        )
    })?;
    for t in &reaches {
        if t.starts_with("offchain__") || t == "labels" {
            bail!("{at}: reads `{t}`, which is not bound inside a fold");
        }
        if !surface.contains(t) {
            bail!("{at}: reads `{t}`, which is neither a table, a view nor an earlier fold");
        }
    }

    // Volatility, in the fold and in every view it reaches: a fold over a volatile view is volatile.
    let mut sources = vec![(String::from("it"), sql.clone())];
    for t in &reaches {
        if let Some(body) = view_bodies.get(t) {
            sources.push((format!("view `{t}`"), body.clone()));
        }
    }
    for (what, text) in &sources {
        if let Some(r) = binder.refusals(text).first() {
            bail!("{at}: {what} {r}; a checkpoint must be reproducible");
        }
    }

    let carry = declared_carry(binder, at, &decl)?;
    let key = match decl.key {
        KeyDecl::Word(w) if w == "singleton" => FoldKey::Singleton,
        KeyDecl::Word(w) if w == "unkeyed" => FoldKey::Unkeyed,
        KeyDecl::Word(w) => {
            bail!("{at}: key is a list of columns, \"singleton\" or \"unkeyed\", not \"{w}\"")
        }
        KeyDecl::Columns(cols) => {
            if cols.is_empty() {
                bail!("{at}: an empty key; say \"singleton\" or \"unkeyed\"");
            }
            for c in &cols {
                if !carry.iter().any(|(n, _)| n == c) {
                    bail!("{at}: key column `{c}` is not in the carry");
                }
            }
            FoldKey::Columns(cols)
        }
    };
    if decl.max_rows == 0 {
        bail!("{at}: max_rows must be at least 1");
    }

    // The output is the carry: same names, same types, same order. Bound with its own carry in place.
    binder.execute(&format!(
        "CREATE OR REPLACE TEMP VIEW \"{carry_name}\" AS {}",
        empty_relation(&carry)
    ))?;
    let output = binder
        .describe(&sql)
        .with_context(|| format!("{at}: does not bind"))?;
    binder.execute(&format!("DROP VIEW \"{carry_name}\""))?;
    if output != carry {
        bail!("{at}: {}", schema_mismatch(&carry, &output));
    }

    Ok(Fold {
        name,
        file: String::new(),
        sql,
        key,
        carry,
        max_rows: decl.max_rows,
        reaches,
        deps,
    })
}

fn declared_carry(
    binder: &analytics::FoldBinder,
    at: &str,
    decl: &FoldDecl,
) -> Result<Vec<(String, String)>> {
    let mut out: Vec<(String, String)> = Vec::new();
    for spec in &decl.carry {
        let (col, ty) = spec
            .trim()
            .split_once(char::is_whitespace)
            .with_context(|| format!("{at}: carry entry `{spec}` is not `<column> <TYPE>`"))?;
        if !is_identifier(col) {
            bail!("{at}: carry column `{col}` is not a plain lowercase name");
        }
        if out.iter().any(|(c, _)| c == col) {
            bail!("{at}: carry column `{col}` is declared twice");
        }
        // DuckDB's own spelling, so `INT` and `INTEGER` compare equal.
        let canonical = binder
            .describe(&format!("SELECT CAST(NULL AS {}) AS c", ty.trim()))
            .with_context(|| format!("{at}: carry column `{col}` has an unknown type `{ty}`"))?
            .remove(0)
            .1;
        let allowed = CARRY_TYPES.contains(&canonical.as_str())
            || canonical
                .strip_prefix("DECIMAL(")
                .and_then(|r| r.split(',').next())
                .and_then(|p| p.parse::<u32>().ok())
                .is_some_and(|p| p <= 38);
        if !allowed {
            bail!(
                "{at}: carry column `{col}` is {canonical}, which a checkpoint cannot hold exactly; \
                 carry big integers as VARCHAR and cast them in the step"
            );
        }
        out.push((col.to_string(), canonical));
    }
    if out.is_empty() {
        bail!("{at}: an empty carry");
    }
    Ok(out)
}

fn schema_mismatch(carry: &[(String, String)], output: &[(String, String)]) -> String {
    let mut parts = Vec::new();
    for (c, t) in carry {
        match output.iter().find(|(o, _)| o == c) {
            None => parts.push(format!("missing `{c}`")),
            Some((_, ot)) if ot != t => parts.push(format!("`{c}` is {ot}, the carry says {t}")),
            _ => {}
        }
    }
    for (o, _) in output {
        if !carry.iter().any(|(c, _)| c == o) {
            parts.push(format!("extra `{o}`"));
        }
    }
    if parts.is_empty() {
        let order: Vec<&str> = output.iter().map(|(o, _)| o.as_str()).collect();
        parts.push(format!("columns are in the order {}", order.join(", ")));
    }
    format!("output is not its carry: {}", parts.join("; "))
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(super) const SCHEMA: &str = r#"{"tables":[{"table":"t","columns":[{"name":"block_number","storage":"u64"},{"name":"k","storage":"varchar"},{"name":"v","storage":"varchar"}]}]}"#;

    fn nest(folds: &[(&str, &str)], decls: &str, views: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("schema.json"), SCHEMA).unwrap();
        std::fs::create_dir_all(dir.path().join("folds")).unwrap();
        for (file, sql) in folds {
            std::fs::write(dir.path().join("folds").join(file), sql).unwrap();
        }
        std::fs::write(dir.path().join("folds/folds.toml"), decls).unwrap();
        if !views.is_empty() {
            std::fs::create_dir_all(dir.path().join("views")).unwrap();
            for (file, sql) in views {
                std::fs::write(dir.path().join("views").join(file), sql).unwrap();
            }
        }
        dir
    }

    fn refusal(folds: &[(&str, &str)], decls: &str, views: &[(&str, &str)]) -> String {
        let dir = nest(folds, decls, views);
        format!(
            "{:#}",
            FoldSet::load(dir.path(), &[]).expect_err("must be refused")
        )
    }

    const COUNT: &str = "SELECT CAST(count(*) AS UBIGINT) AS n FROM t";
    const COUNT_DECL: &str =
        "[[fold]]\nname = \"c\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n";

    #[test]
    fn a_valid_set_loads_in_name_order_with_its_dependencies() {
        let dir = nest(
            &[
                ("20-latest.sql", "SELECT k, v FROM t UNION ALL SELECT k, v FROM latest__carry"),
                (
                    "10-running.sql",
                    "SELECT CAST(count(*) + coalesce((SELECT max(n) FROM running__carry), 0) AS UBIGINT) AS n FROM t",
                ),
                ("30-copy.sql", "SELECT n FROM running"),
            ],
            "[[fold]]\nname = \"latest\"\nkey = [\"k\"]\ncarry = [\"k VARCHAR\", \"v VARCHAR\"]\nmax_rows = 10\n\
             [[fold]]\nname = \"running\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n\
             [[fold]]\nname = \"copy\"\nkey = \"unkeyed\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n",
            &[],
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        let names: Vec<&str> = set.folds.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["running", "latest", "copy"]);
        assert_eq!(set.folds[0].file, "10-running.sql");
        assert_eq!(set.folds[1].key, FoldKey::Columns(vec!["k".into()]));
        assert_eq!(set.folds[2].deps, BTreeSet::from(["running".to_string()]));
        assert_eq!(set.folds[0].reaches, BTreeSet::from(["t".to_string()]));
    }

    #[test]
    fn a_volatile_fold_is_refused_and_so_is_one_over_a_volatile_view() {
        for (sql, function) in [
            (
                "SELECT CAST(count(*) AS UBIGINT) AS n FROM t WHERE now() IS NOT NULL",
                "now",
            ),
            (
                "SELECT CAST(count(*) AS UBIGINT) AS n FROM t WHERE random() < 2",
                "random",
            ),
            (
                "SELECT CAST(count(*) AS UBIGINT) AS n FROM t WHERE current_date IS NOT NULL",
                "current_date",
            ),
        ] {
            let msg = refusal(&[("c.sql", sql)], COUNT_DECL, &[]);
            assert!(
                msg.contains(function) && msg.contains("reproducible"),
                "{msg}"
            );
        }
        let msg = refusal(
            &[("c.sql", "SELECT CAST(count(*) AS UBIGINT) AS n FROM noisy")],
            COUNT_DECL,
            &[(
                "10-noisy.sql",
                "CREATE VIEW noisy AS SELECT * FROM t WHERE random() < 2;",
            )],
        );
        assert!(
            msg.contains("view `noisy`") && msg.contains("random"),
            "{msg}"
        );
    }

    #[test]
    fn an_output_that_is_not_its_carry_is_refused_by_what_differs() {
        let decl = "[[fold]]\nname = \"c\"\nkey = [\"k\"]\ncarry = [\"k VARCHAR\", \"n UBIGINT\"]\nmax_rows = 9\n";
        for (sql, expect) in [
            (
                "SELECT k, CAST(count(*) AS BIGINT) AS n FROM t GROUP BY k",
                "`n` is BIGINT, the carry says UBIGINT",
            ),
            ("SELECT k FROM t", "missing `n`"),
            (
                "SELECT k, CAST(count(*) AS UBIGINT) AS n, 1 AS x FROM t GROUP BY k",
                "extra `x`",
            ),
            (
                "SELECT CAST(count(*) AS UBIGINT) AS n, k FROM t GROUP BY k",
                "in the order n, k",
            ),
        ] {
            let msg = refusal(&[("c.sql", sql)], decl, &[]);
            assert!(msg.contains(expect), "{sql}: {msg}");
        }
    }

    #[test]
    fn a_carry_type_a_checkpoint_cannot_hold_exactly_is_refused() {
        for ty in ["HUGEINT", "DOUBLE", "VARCHAR[]"] {
            let decl = format!(
                "[[fold]]\nname = \"c\"\nkey = \"singleton\"\ncarry = [\"n {ty}\"]\nmax_rows = 1\n"
            );
            let msg = refusal(&[("c.sql", COUNT)], &decl, &[]);
            assert!(msg.contains("cannot hold exactly"), "{ty}: {msg}");
        }
    }

    #[test]
    fn names_order_and_declarations_are_checked() {
        let fwd = "[[fold]]\nname = \"a\"\nkey = \"unkeyed\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n\
                   [[fold]]\nname = \"b\"\nkey = \"unkeyed\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n";
        let msg = refusal(
            &[("10-a.sql", "SELECT n FROM b"), ("20-b.sql", COUNT)],
            fwd,
            &[],
        );
        assert!(msg.contains("`b`") && msg.contains("earlier fold"), "{msg}");

        let clash = COUNT_DECL.replace("\"c\"", "\"t\"");
        let msg = refusal(&[("t.sql", COUNT)], &clash, &[]);
        assert!(msg.contains("already a table or view"), "{msg}");

        let msg = refusal(&[("c.sql", COUNT), ("d.sql", COUNT)], COUNT_DECL, &[]);
        assert!(msg.contains("d.sql has no [[fold]]"), "{msg}");

        let msg = refusal(
            &[("c.sql", COUNT)],
            &format!("{COUNT_DECL}{}", COUNT_DECL.replace("\"c\"", "\"e\"")),
            &[],
        );
        assert!(msg.contains("`e`, which has no .sql file"), "{msg}");

        let msg = refusal(
            &[("c.sql", "SELECT 1 AS n; SELECT 2 AS n")],
            COUNT_DECL,
            &[],
        );
        assert!(
            msg.contains("exactly one SELECT") || msg.contains("parse"),
            "{msg}"
        );
    }
}

#[cfg(test)]
mod stepping_support {
    use super::*;
    use serde_json::json;

    /// Blocks 1..=30 sealed as three segments, 31..=35 hot. Key `k` cycles a, b, c; `v` is the block.
    pub(super) fn corpus() -> (tempfile::TempDir, analytics::HotRows) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("schema.json"), super::tests::SCHEMA).unwrap();
        crate::seal::test_set_table_floor(dir.path(), 0);
        let row = |b: u64| {
            let k = ["a", "b", "c"][(b % 3) as usize];
            json!({"table": "t", "block_number": b, "k": k, "v": b.to_string()})
        };
        for (from, to) in [(1, 10), (11, 20), (21, 30)] {
            let rows: Vec<String> = (from..=to).map(|b| row(b).to_string()).collect();
            crate::seal::seal_range(dir.path(), &rows, from, to).unwrap();
        }
        let mut hot = analytics::HotRows::new();
        hot.insert("t".into(), (31..=35).map(row).collect());
        (dir, hot)
    }

    pub(super) fn fold_files(dir: &Path, folds: &[(&str, &str)], decls: &str) {
        std::fs::create_dir_all(dir.join("folds")).unwrap();
        for (file, sql) in folds {
            std::fs::write(dir.join("folds").join(file), sql).unwrap();
        }
        std::fs::write(dir.join("folds/folds.toml"), decls).unwrap();
    }
}

#[cfg(test)]
mod stepping {
    use super::stepping_support::*;
    use super::*;

    fn n(rows: &[serde_json::Value]) -> u64 {
        rows[0]["n"].as_u64().unwrap()
    }

    /// RFC-0059 S1's first criterion: `count(*)` over a fact table inside a fold counts the window,
    /// not history, while a fold that adds its carry reaches the history total.
    #[test]
    fn a_fold_sees_its_window_and_its_carry_holds_the_rest() {
        let (dir, hot) = corpus();
        fold_files(
            dir.path(),
            &[
                ("10-window.sql", "SELECT CAST(count(*) AS UBIGINT) AS n FROM t"),
                (
                    "20-running.sql",
                    "SELECT CAST(coalesce((SELECT max(n) FROM running__carry), 0) + count(*) AS UBIGINT) AS n FROM t",
                ),
            ],
            "[[fold]]\nname = \"window\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n\
             [[fold]]\nname = \"running\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n",
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        let mut s = set.stepper(dir.path(), &[]).unwrap();
        // Cuts inside a segment (15, 25) and across the sealed/hot boundary (33).
        for (hi, window, running) in [(15, 15, 15), (25, 10, 25), (33, 8, 33), (35, 2, 35)] {
            s.step_to(&hot, 30, hi).unwrap();
            assert_eq!(n(&s.rows("window").unwrap()), window, "window at {hi}");
            assert_eq!(n(&s.rows("running").unwrap()), running, "running at {hi}");
        }
        assert!(
            s.step_to(&hot, 30, 35).is_err(),
            "a fold never steps backwards or in place"
        );
    }

    /// A keyed fold emits only the keys its window touched; untouched keys keep their carried rows.
    #[test]
    fn a_keyed_fold_passes_untouched_keys_through() {
        let (dir, hot) = corpus();
        fold_files(
            dir.path(),
            &[(
                "latest.sql",
                "SELECT k, v FROM t QUALIFY row_number() OVER (PARTITION BY k ORDER BY block_number DESC) = 1",
            )],
            "[[fold]]\nname = \"latest\"\nkey = [\"k\"]\ncarry = [\"k VARCHAR\", \"v VARCHAR\"]\nmax_rows = 3\n",
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        let mut s = set.stepper(dir.path(), &[]).unwrap();
        s.step_to(&hot, 30, 30).unwrap();
        // (30, 31] touches only block 31, key b.
        s.step_to(&hot, 30, 31).unwrap();
        let got: Vec<(String, String)> = s
            .rows("latest")
            .unwrap()
            .iter()
            .map(|r| {
                (
                    r["k"].as_str().unwrap().into(),
                    r["v"].as_str().unwrap().into(),
                )
            })
            .collect();
        let want =
            [("a", "30"), ("b", "31"), ("c", "29")].map(|(k, v)| (k.to_string(), v.to_string()));
        assert_eq!(got, want);
    }

    /// A step refused part-way leaves every fold at the previous step. Otherwise an earlier fold has
    /// already advanced, and a retry folds the same window into it twice.
    #[test]
    fn a_refused_step_changes_nothing_and_a_retry_does_not_double_count() {
        let (dir, hot) = corpus();
        fold_files(
            dir.path(),
            &[
                (
                    "10-running.sql",
                    "SELECT CAST(coalesce((SELECT max(n) FROM running__carry), 0) + count(*) AS UBIGINT) AS n FROM t",
                ),
                // Blocks 1, 2, 3 bring keys b, c, a: the third step breaks max_rows.
                ("20-keys.sql", "SELECT DISTINCT k FROM t"),
            ],
            "[[fold]]\nname = \"running\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n\
             [[fold]]\nname = \"keys\"\nkey = [\"k\"]\ncarry = [\"k VARCHAR\"]\nmax_rows = 2\n",
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        let mut s = set.stepper(dir.path(), &[]).unwrap();
        s.step_to(&hot, 30, 1).unwrap();
        s.step_to(&hot, 30, 2).unwrap();
        for _ in 0..2 {
            assert!(s.step_to(&hot, 30, 3).is_err());
            assert_eq!(s.at(), Some(2));
            assert_eq!(
                n(&s.rows("running").unwrap()),
                2,
                "running advanced on a refused step"
            );
        }
    }

    #[test]
    fn a_fold_over_its_max_rows_is_refused() {
        let (dir, hot) = corpus();
        fold_files(
            dir.path(),
            &[("keys.sql", "SELECT DISTINCT k FROM t")],
            "[[fold]]\nname = \"keys\"\nkey = [\"k\"]\ncarry = [\"k VARCHAR\"]\nmax_rows = 2\n",
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        let mut s = set.stepper(dir.path(), &[]).unwrap();
        let err = s.step_to(&hot, 30, 30).unwrap_err();
        assert!(
            format!("{err:#}").contains("over its declared max_rows 2"),
            "{err:#}"
        );
    }
}

#[cfg(test)]
mod short_windows {
    use super::stepping_support::*;
    use super::*;

    /// `/sql` answers short and flags it; a fold step would checkpoint the short answer as the truth.
    #[test]
    fn a_window_missing_sealed_data_is_refused() {
        let (dir, hot) = corpus();
        fold_files(
            dir.path(),
            &[("c.sql", "SELECT CAST(count(*) AS UBIGINT) AS n FROM t")],
            "[[fold]]\nname = \"c\"\nkey = \"singleton\"\ncarry = [\"n UBIGINT\"]\nmax_rows = 1\n",
        );
        let set = FoldSet::load(dir.path(), &[]).unwrap();
        let manifest = crate::seal::load_manifest_with_hash(dir.path()).unwrap().0;
        let seg = manifest.tables["t"]
            .iter()
            .find(|s| s.from_block == 11)
            .unwrap();
        std::fs::remove_file(crate::seal::segment_path(dir.path(), &seg.file, &seg.hash)).unwrap();
        let mut s = set.stepper(dir.path(), &[]).unwrap();
        let err = s.step_to(&hot, 30, 25).unwrap_err();
        assert!(
            format!("{err:#}").contains("missing sealed data for t"),
            "{err:#}"
        );
    }
}
