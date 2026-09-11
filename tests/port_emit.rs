//! RFC-0044 S2: a mapping with one `eth_call` must emit a `[[calls]]` whose signature traces to
//! that line; an Exact field must appear in a view; dropping the call must drop the stanza.

use std::path::{Path, PathBuf};

fn one_call_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("skills/nuthatch-subgraph-port/fixtures/one-call")
}

fn four_classes_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("skills/nuthatch-subgraph-port/fixtures/four-classes")
}

const FACTORY_ABI: &str = r#"[{"type":"event","name":"PoolCreated","anonymous":false,"inputs":[
    {"name":"token0","type":"address","indexed":true},
    {"name":"token1","type":"address","indexed":true},
    {"name":"fee","type":"uint24","indexed":true},
    {"name":"tickSpacing","type":"int24","indexed":false},
    {"name":"pool","type":"address","indexed":false}]}]"#;

const POOL_ABI: &str = r#"[{"type":"event","name":"Swap","anonymous":false,"inputs":[
    {"name":"sender","type":"address","indexed":true},
    {"name":"recipient","type":"address","indexed":true},
    {"name":"amount0","type":"int256","indexed":false},
    {"name":"amount1","type":"int256","indexed":false},
    {"name":"sqrtPriceX96","type":"uint160","indexed":false},
    {"name":"liquidity","type":"uint128","indexed":false},
    {"name":"tick","type":"int24","indexed":false}]}]"#;

fn write_imported_nest(dir: &Path, with_pool: bool) {
    std::fs::create_dir_all(dir.join("abis")).unwrap();
    std::fs::write(dir.join("abis/factory.json"), FACTORY_ABI).unwrap();
    let mut toml = String::from(
        r#"[nest]
name = "port-emit-test"
chain = "arbitrum-one"
chain_id = 42161
rpc_urls = ["http://127.0.0.1:1"]

[[contracts]]
alias = "factory"
address = "0x1f98431c8ad98523631ae4a59f267346ea31f984"
start_block = 1
abi = "abis/factory.json"
events = ["PoolCreated"]
"#,
    );
    if with_pool {
        std::fs::write(dir.join("abis/pool.json"), POOL_ABI).unwrap();
        toml.push_str(
            r#"
[[templates]]
name = "pool"
abi = "abis/pool.json"
events = ["Swap"]

[[factories]]
watch = "factory"
event = "PoolCreated"
child_param = "pool"
template = "pool"
"#,
        );
    }
    std::fs::write(dir.join("nuthatch.toml"), toml).unwrap();
    nuthatch::project::regen(nuthatch::cli::SchemaArgs {
        dir: dir.display().to_string(),
    })
    .expect("regen nest artifacts");
}

fn select_sql(sql: &str) -> String {
    sql.lines()
        .filter(|l| !l.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn copy_dir(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let to = dst.join(entry.file_name());
        if entry.path().is_dir() {
            copy_dir(&entry.path(), &to);
        } else {
            std::fs::copy(entry.path(), to).unwrap();
        }
    }
}

#[test]
fn one_eth_call_emits_calls_whose_signature_traces_to_that_line() {
    let nest = tempfile::tempdir().unwrap();
    write_imported_nest(nest.path(), false);
    let result = nuthatch::port_emit::emit(&one_call_dir(), nest.path()).expect("emit");

    assert_eq!(
        result.calls.len(),
        1,
        "one try_symbol must produce one [[calls]], got {:?}",
        result
            .calls
            .iter()
            .map(|c| &c.decl.name)
            .collect::<Vec<_>>()
    );
    let call = &result.calls[0];
    assert_eq!(
        call.decl.signature.as_deref(),
        Some("symbol()"),
        "signature must be the method try_symbol called, not a guess"
    );
    assert_eq!(call.decl.on.as_deref(), Some("factory__pool_created"));
    assert_eq!(call.decl.contract_column.as_deref(), Some("{token0}"));
    assert!(
        call.citation.file.ends_with("core.ts"),
        "citation file {}",
        call.citation.file
    );
    assert_eq!(
        call.citation.line, 8,
        "must cite the try_symbol line in fixtures/one-call/src/mappings/core.ts, got {}",
        call.citation.line
    );

    let toml = std::fs::read_to_string(nest.path().join("nuthatch.toml")).unwrap();
    assert!(toml.contains("[[calls]]"), "{toml}");
    assert!(toml.contains("signature = \"symbol()\""), "{toml}");
    assert!(toml.contains("on = \"factory__pool_created\""), "{toml}");
}

#[test]
fn exact_field_appears_in_a_view() {
    let nest = tempfile::tempdir().unwrap();
    write_imported_nest(nest.path(), false);
    let result = nuthatch::port_emit::emit(&one_call_dir(), nest.path()).unwrap();
    let token = result
        .views
        .iter()
        .find(|v| v.entity == "Token")
        .expect("Token view");
    assert!(
        token.exact_fields.iter().any(|f| f == "id"),
        "Token.id is exact: {:?}",
        token.exact_fields
    );
    let select = select_sql(&token.sql);
    assert!(
        select.contains("AS \"id\""),
        "Token.id must be a SELECT column, not a comment:\n{}",
        token.sql
    );
    assert!(
        select.contains("\"token0\""),
        "constructor id is event.params.token0:\n{}",
        token.sql
    );
    assert!(
        !select.contains("port_placeholder"),
        "Token.id was omitted from the projection:\n{}",
        token.sql
    );
    let on_disk = std::fs::read_to_string(nest.path().join("views").join(&token.file)).unwrap();
    assert!(
        select_sql(&on_disk).contains("AS \"id\""),
        "on-disk view must project id:\n{on_disk}"
    );
    assert!(
        !on_disk.to_ascii_lowercase().contains("symbol")
            || on_disk.contains("Call-derived")
            || on_disk.contains("call-derived"),
        "Token.symbol is call-derived and must not be selected as an exact column:\n{on_disk}"
    );
}

/// The invariant the coverage figure rests on: a view projects exactly the fields it lists.
///
/// `EmittedView::exact_fields` feeds two things - the generated check's projection and the coverage
/// figure - so a field listed but not projected reads as verified *and* as answered while DuckDB
/// cannot answer it. That divergence is #1248. It is now impossible by construction, because the list
/// is the key set of the same map the SQL is rendered from, and this test is what holds that true if
/// anyone reintroduces a parallel list.
#[test]
fn every_emitted_view_projects_exactly_the_fields_it_lists() {
    /// Names the outer projection exposes, for both shapes `exact_select_sql` emits: the grouped fold
    /// (`last(...) AS "x"` over a union, with a bare `"id"`) and the single-arm form (`"col" AS "x"`).
    /// `__present__` markers are internal and not fields.
    fn projected(sql: &str) -> std::collections::BTreeSet<String> {
        let body = sql
            .split_once(" AS\n")
            .map(|(_, rest)| rest)
            .unwrap_or(sql)
            .trim_start();
        let head = body
            .split_once("\nFROM")
            .map(|(h, _)| h)
            .unwrap_or(body)
            .trim_start()
            .trim_start_matches("SELECT");
        let mut out = std::collections::BTreeSet::new();
        for part in head.split(",\n") {
            let part = part.trim().trim_end_matches(';');
            let name = match part.rsplit_once(" AS ") {
                Some((_, alias)) => alias.trim(),
                // a bare projected column, which is how `id` is emitted in the fold
                None => part,
            };
            let name = name.trim().trim_matches('"');
            if name.is_empty()
                || name.starts_with("__present__")
                || !name.contains(char::is_alphabetic)
            {
                continue;
            }
            out.insert(name.to_string());
        }
        out
    }

    // Three shapes: one field and one table, a fold over several fields, and an overlay with a
    // running total split out to an incremental entity.
    for (label, schema, mapping) in [
        ("one field", None, None),
        ("accumulator split", Some(ACCUM_SCHEMA), Some(ACCUM_MAPPING)),
        ("operations", Some(OPS_SCHEMA), Some(OPS_MAPPING)),
    ] {
        let (_nest, result) = match (schema, mapping) {
            (Some(s), Some(m)) => emitted_nest(s, m),
            _ => {
                let nest = tempfile::tempdir().unwrap();
                write_imported_nest(nest.path(), false);
                let r = nuthatch::port_emit::emit(&one_call_dir(), nest.path()).unwrap();
                (nest, r)
            }
        };
        assert!(
            !result.views.is_empty(),
            "{label}: nothing was emitted, so this case proves nothing"
        );
        for v in &result.views {
            let listed: std::collections::BTreeSet<String> =
                v.exact_fields.iter().cloned().collect();
            let select = v
                .sql
                .split_once("CREATE VIEW")
                .map(|(_, rest)| rest)
                .unwrap_or_else(|| panic!("{label}/{}: no CREATE VIEW:\n{}", v.entity, v.sql));
            assert_eq!(
                projected(select),
                listed,
                "{label}/{}: the projection and `exact_fields` disagree\n{}",
                v.entity,
                v.sql
            );
            assert!(
                !listed.is_empty(),
                "{label}/{}: an emitted view with no listed field",
                v.entity
            );
        }
    }
}

/// `README.md` is the report **plus what this overlay actually answers**.
///
/// It used to be the report verbatim, which is what this test asserted. The report's summary table
/// describes the mapping - a field is `exact` when it is a pure function of decoded events, a claim
/// about reproducibility in principle. On the RFC-0044 S3 acceptance port it said `exact 205` while
/// the overlay answered **27**, and nothing on disk or on stdout said so, so the artefacts read as a
/// finished port (#1277).
#[test]
fn readme_carries_the_report_and_the_overlay_coverage() {
    let nest = tempfile::tempdir().unwrap();
    write_imported_nest(nest.path(), false);
    let result = nuthatch::port_emit::emit(&one_call_dir(), nest.path()).unwrap();
    let readme = std::fs::read_to_string(nest.path().join("README.md")).unwrap();

    assert!(
        readme.starts_with(&result.report),
        "the report must still be there, whole and first:\n{readme}"
    );
    assert!(readme.contains("# Port report"));
    assert!(readme.contains("call-derived"));
    assert!(readme.contains("`Token.symbol`"));

    // This fixture has one exact field (`Token.id`) and the view answers it, so the honest figure is
    // 1 of 1. Asserted as the rendered sentence rather than as `contains("coverage")`, because the
    // sentence is what a porter reads and a count that drifts from the artefacts is the whole defect.
    assert_eq!(
        result.coverage,
        nuthatch::port_emit::Coverage {
            classified_exact: 1,
            in_views: 1,
            incremental: 0,
        },
        "coverage must be counted from the artefacts"
    );
    assert!(
        readme.contains(
            "1 of 1 fields the report calls exact are answered (100%): 1 in views, 0 maintained \
             incrementally, 0 not answered at all"
        ),
        "README must carry the figure as a sentence:\n{readme}"
    );
    assert!(
        readme.contains("## Overlay coverage"),
        "and under a heading a reader can find:\n{readme}"
    );
}

/// The figure has to move when the overlay gets worse, or it is decoration.
///
/// `Token.tally` is written by the mapping from an arithmetic expression over an event parameter, so
/// the classifier calls it exact - a pure function of decoded events - and the emitter cannot render
/// it into a column. That is the exact shape of the 178 fields the S3 acceptance port promised and
/// could not answer (#1277), and the coverage line must stop reading 100%.
#[test]
fn an_unanswered_exact_field_moves_the_coverage_figure() {
    let subgraph = tempfile::tempdir().unwrap();
    copy_dir(&one_call_dir(), subgraph.path());

    let schema = subgraph.path().join("schema.graphql");
    let text = std::fs::read_to_string(&schema).unwrap();
    let text = text.replace("  symbol: String!", "  symbol: String!\n  tally: BigInt!");
    assert!(text.contains("tally"), "schema edit did not apply");
    std::fs::write(&schema, &text).unwrap();

    let mapping = subgraph.path().join("src/mappings/core.ts");
    let m = std::fs::read_to_string(&mapping).unwrap();
    // Asserted rather than assumed: the fixture binds `token0`, not `token`, and an edit that matches
    // nothing leaves a field the classifier calls *unreachable* instead of exact - which moves no
    // coverage and makes this test vacuous. It happened while writing it.
    let m2 = m.replace(
        "  token0.save()",
        "  token0.tally = event.params.fee.plus(event.params.fee)\n  token0.save()",
    );
    assert_ne!(m2, m, "mapping edit did not apply:\n{m}");
    std::fs::write(&mapping, &m2).unwrap();

    let nest = tempfile::tempdir().unwrap();
    write_imported_nest(nest.path(), false);
    let result = nuthatch::port_emit::emit(subgraph.path(), nest.path()).unwrap();
    let readme = std::fs::read_to_string(nest.path().join("README.md")).unwrap();

    assert_eq!(
        result.coverage.classified_exact,
        2,
        "the fixture must promise two exact fields, got {}",
        result.coverage.summary()
    );
    assert_eq!(
        result.coverage.unanswered(),
        1,
        "and exactly one must be unanswered, got {}",
        result.coverage.summary()
    );
    assert!(
        readme.contains("1 of 2 fields the report calls exact are answered (50%)"),
        "README must carry the moved figure:\n{}",
        result.coverage.summary()
    );
    assert!(
        !readme.contains("(100%)"),
        "an overlay with an unanswered exact field must not read 100%:\n{}",
        result.coverage.summary()
    );
}

#[test]
fn dropping_the_call_from_the_mapping_drops_the_calls_stanza() {
    let subgraph = tempfile::tempdir().unwrap();
    copy_dir(&one_call_dir(), subgraph.path());
    let mapping = subgraph.path().join("src/mappings/core.ts");
    let original = std::fs::read_to_string(&mapping).unwrap();
    assert!(
        original.contains("try_symbol"),
        "fixture must contain the call this test drops"
    );
    let mutated = original.replace(
        "  let contract = ERC20.bind(tokenAddress)\n  let result = contract.try_symbol()\n  if (!result.reverted) {\n    return result.value\n  }\n  return 'unknown'\n",
        "  return 'unknown'\n",
    );
    assert!(!mutated.contains("try_symbol"), "{mutated}");
    std::fs::write(&mapping, mutated).unwrap();

    let nest = tempfile::tempdir().unwrap();
    write_imported_nest(nest.path(), false);
    let result = nuthatch::port_emit::emit(subgraph.path(), nest.path()).unwrap();
    assert!(
        result.calls.is_empty(),
        "dropping try_symbol must vanish the [[calls]], got {:?}",
        result
            .calls
            .iter()
            .map(|c| (c.decl.signature.clone(), c.citation.display()))
            .collect::<Vec<_>>()
    );
    let toml = std::fs::read_to_string(nest.path().join("nuthatch.toml")).unwrap();
    assert!(
        !toml.contains("[[calls]]"),
        "nuthatch.toml must not invent a [[calls]] after the mapping call was dropped:\n{toml}"
    );
}

#[test]
fn parameterized_call_is_refused_but_the_rest_of_the_overlay_still_lands() {
    let subgraph = tempfile::tempdir().unwrap();
    copy_dir(&one_call_dir(), subgraph.path());
    let mapping = subgraph.path().join("src/mappings/core.ts");
    let original = std::fs::read_to_string(&mapping).unwrap();
    std::fs::write(
        &mapping,
        original.replace(
            "contract.try_symbol()",
            "contract.try_balanceOf(event.params.tokenId)",
        ),
    )
    .unwrap();

    let nest = tempfile::tempdir().unwrap();
    write_imported_nest(nest.path(), false);
    let result = nuthatch::port_emit::emit(subgraph.path(), nest.path())
        .expect("one unpinnable read must not cost the author the whole overlay");

    // Still refused, and still not guessed.
    assert!(
        result.calls.is_empty(),
        "a parameterized read has no ABI parameter types, so no signature may be emitted: {:?}",
        result
            .calls
            .iter()
            .map(|c| &c.decl.name)
            .collect::<Vec<_>>()
    );
    let skipped = result
        .skipped_calls
        .iter()
        .find(|s| s.signature.contains("balanceOf"))
        .expect("the skipped read must be reported by name, not dropped silently");
    assert!(skipped.why.contains("refusing to guess"), "{}", skipped.why);
    assert!(
        skipped.citation.file.contains("core.ts"),
        "{:?}",
        skipped.citation
    );

    // And the rest of the port is there, which is what aborting used to destroy.
    assert!(!result.views.is_empty(), "views must still be emitted");
    assert!(
        nest.path().join("README.md").exists(),
        "the README carrying the port report must still be written"
    );
    assert!(
        nest.path().join("checks/port_views.sql").exists(),
        "the checks must still be written"
    );
}

#[test]
fn four_classes_exact_sqrt_price_lands_in_a_view() {
    let nest = tempfile::tempdir().unwrap();
    write_imported_nest(nest.path(), true);
    let result = nuthatch::port_emit::emit(&four_classes_dir(), nest.path()).unwrap();
    let pool = result
        .views
        .iter()
        .find(|v| v.entity == "Pool")
        .expect("Pool view");
    assert!(
        pool.exact_fields.iter().any(|f| f == "sqrtPrice"),
        "Pool.sqrtPrice is exact: {:?}",
        pool.exact_fields
    );
    assert!(
        pool.sql.contains("sqrtPrice") || pool.sql.contains("sqrt_price"),
        "Pool.sqrtPrice must appear in a view:\n{}",
        pool.sql
    );
    assert!(
        result
            .calls
            .iter()
            .any(|c| c.decl.signature.as_deref() == Some("symbol()")),
        "four-classes still has try_symbol: {:?}",
        result
            .calls
            .iter()
            .map(|c| &c.decl.signature)
            .collect::<Vec<_>>()
    );
    assert!(
        result
            .calls
            .iter()
            .any(|c| c.decl.signature.as_deref() == Some("decimals()")),
        "four-classes still has try_decimals"
    );
}

#[test]
fn two_triggering_tables_keep_every_exact_field() {
    let subgraph = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(subgraph.path().join("src/mappings")).unwrap();
    std::fs::write(
        subgraph.path().join("schema.graphql"),
        r#"type Token @entity {
  id: ID!
  symbol: String!
  name: String!
}
"#,
    )
    .unwrap();
    std::fs::write(
        subgraph.path().join("subgraph.yaml"),
        r#"specVersion: 0.0.8
dataSources:
  - kind: ethereum/contract
    name: Factory
    network: arbitrum-one
    source:
      address: "0x1F98431c8aD98523631AE4a59f267346ea31F984"
      abi: Factory
      startBlock: 1
    mapping:
      kind: ethereum/events
      apiVersion: 0.0.7
      language: wasm/assemblyscript
      file: ./src/mappings/core.ts
      entities: [Token]
      abis:
        - name: Factory
          file: ./abis/factory.json
      eventHandlers:
        - event: PoolCreated(indexed address,indexed address,indexed uint24,int24,address)
          handler: handlePoolCreated
        - event: TokenUpdated(indexed address,string)
          handler: handleTokenUpdated
"#,
    )
    .unwrap();
    std::fs::write(
        subgraph.path().join("src/mappings/core.ts"),
        r#"import { PoolCreated, TokenUpdated } from '../../generated/Factory/Factory'
import { Token } from '../../generated/schema'

export function handlePoolCreated(event: PoolCreated): void {
  let token = new Token(event.params.token0.toHex())
  token.symbol = event.params.token0.toHex()
  token.save()
}

export function handleTokenUpdated(event: TokenUpdated): void {
  let token = Token.load(event.params.token.toHex())!
  token.name = event.params.name
  token.save()
}
"#,
    )
    .unwrap();

    let nest = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(nest.path().join("abis")).unwrap();
    let abi = r#"[{"type":"event","name":"PoolCreated","anonymous":false,"inputs":[
    {"name":"token0","type":"address","indexed":true},
    {"name":"token1","type":"address","indexed":true},
    {"name":"fee","type":"uint24","indexed":true},
    {"name":"tickSpacing","type":"int24","indexed":false},
    {"name":"pool","type":"address","indexed":false}]},
{"type":"event","name":"TokenUpdated","anonymous":false,"inputs":[
    {"name":"token","type":"address","indexed":true},
    {"name":"name","type":"string","indexed":false}]}]"#;
    std::fs::write(nest.path().join("abis/factory.json"), abi).unwrap();
    std::fs::write(
        nest.path().join("nuthatch.toml"),
        r#"[nest]
name = "port-emit-two-tables"
chain = "arbitrum-one"
chain_id = 42161
rpc_urls = ["http://127.0.0.1:1"]

[[contracts]]
alias = "factory"
address = "0x1f98431c8ad98523631ae4a59f267346ea31f984"
start_block = 1
abi = "abis/factory.json"
events = ["PoolCreated", "TokenUpdated"]
"#,
    )
    .unwrap();
    nuthatch::project::regen(nuthatch::cli::SchemaArgs {
        dir: nest.path().display().to_string(),
    })
    .expect("regen nest artifacts");

    let result = nuthatch::port_emit::emit(subgraph.path(), nest.path()).unwrap();
    let token = result
        .views
        .iter()
        .find(|v| v.entity == "Token")
        .expect("Token view");
    assert!(
        token.exact_fields.iter().any(|f| f == "symbol"),
        "Token.symbol is exact: {:?}",
        token.exact_fields
    );
    assert!(
        token.exact_fields.iter().any(|f| f == "name"),
        "Token.name is exact: {:?}",
        token.exact_fields
    );
    let select = select_sql(&token.sql);
    assert!(
        select.contains("AS \"symbol\""),
        "Token.symbol must be a SELECT column:\n{}",
        token.sql
    );
    assert!(
        select.contains("AS \"name\""),
        "Token.name must be a SELECT column:\n{}",
        token.sql
    );
    assert!(
        select.contains("factory__pool_created"),
        "PoolCreated table must appear:\n{}",
        token.sql
    );
    assert!(
        select.contains("factory__token_updated"),
        "TokenUpdated table must appear:\n{}",
        token.sql
    );
    assert!(
        select.contains("UNION ALL"),
        "two tables overlay with UNION ALL, not a guessed JOIN:\n{}",
        token.sql
    );
    assert!(
        select.contains("GROUP BY") && (select.contains("last(") || select.contains("arg_max")),
        "entity view must fold to last-per-id, not raw event rows:\n{}",
        token.sql
    );
    assert!(
        !select.to_ascii_lowercase().contains(" join "),
        "must not invent a JOIN:\n{}",
        token.sql
    );
}

#[test]
fn emitted_nest_loads_and_views_validate() {
    let nest = tempfile::tempdir().unwrap();
    write_imported_nest(nest.path(), false);
    nuthatch::port_emit::emit(&one_call_dir(), nest.path()).unwrap();
    nuthatch::config::Config::load(nest.path())
        .expect("emitted nest must load without hand editing");
    let result = nuthatch::check::check(nuthatch::cli::CheckArgs {
        name: None,
        dir: nest.path().display().to_string(),
        update: false,
    });
    assert!(
        result.is_ok(),
        "emitted nest must pass nuthatch check: {result:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// #1248: an Exact field must be answered correctly or named, never answered wrongly.
// ---------------------------------------------------------------------------------------------

/// A throwaway subgraph whose mappings are supplied inline. The fixtures on disk are the curated
/// ports; these cases are about right-hand sides the emitter must refuse, so they belong with the
/// test rather than in the fixture set the drift gate watches.
fn subgraph_with(schema: &str, mapping: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src/mappings")).unwrap();
    std::fs::copy(
        one_call_dir().join("subgraph.yaml"),
        dir.path().join("subgraph.yaml"),
    )
    .unwrap();
    std::fs::write(dir.path().join("schema.graphql"), schema).unwrap();
    std::fs::write(dir.path().join("src/mappings/core.ts"), mapping).unwrap();
    dir
}

fn emitted_nest(
    schema: &str,
    mapping: &str,
) -> (tempfile::TempDir, nuthatch::port_emit::EmitResult) {
    let subgraph = subgraph_with(schema, mapping);
    let nest = tempfile::tempdir().unwrap();
    write_imported_nest(nest.path(), false);
    let result = nuthatch::port_emit::emit(subgraph.path(), nest.path()).expect("emit");
    (nest, result)
}

const OPS_SCHEMA: &str = r#"
type Pool @entity {
  id: ID!
  plain: BigInt!
  negated: BigInt!
  scaled: BigInt!
  summed: BigInt!
}
"#;

/// Every parameter here is one that `snake_case` maps to itself (`fee`, `token1`, `pool`).
/// That is deliberate: a camelCase parameter emits a column name the decoded table does not have,
/// so the view fails to bind for a reason that has nothing to do with what these tests assert.
/// That is #1250, filed separately and fixed separately.
const OPS_MAPPING: &str = r#"
export function handlePoolCreated(event: PoolCreated): void {
  let pool = new Pool(event.params.pool.toHex())
  pool.id = event.params.pool.toHex()
  pool.plain = event.params.fee
  pool.negated = event.params.token1.neg()
  pool.scaled = event.params.token1.times(BigInt.fromI32(1000))
  pool.summed = event.params.token1.plus(event.params.fee)
  pool.save()
}
"#;

#[test]
fn an_operation_on_an_event_param_is_never_answered_by_the_bare_column() {
    let (_nest, result) = emitted_nest(OPS_SCHEMA, OPS_MAPPING);
    let view = result
        .views
        .iter()
        .find(|v| v.entity == "Pool")
        .expect("a Pool view");
    let sql = select_sql(&view.sql);

    // The defect: each of these resolved to `"fee"`, so the view answered with the wrong sign, a
    // factor of 1000 out, or an addend short - under a report promising byte-identical.
    for field in ["negated", "scaled", "summed"] {
        assert!(
            !sql.contains(&format!("AS \"{field}\"")),
            "`{field}` applies an operation the emitter cannot render, so it must not be projected \
             at all; the view answered it anyway:\n{sql}"
        );
        assert!(
            !view.exact_fields.contains(&field.to_string()),
            "`{field}` is not in the view, so exact_fields must not advertise it: {:?}",
            view.exact_fields
        );
        assert!(
            result
                .skipped_fields
                .iter()
                .any(|s| s.entity == "Pool" && s.field == field),
            "`{field}` must be named in skipped_fields, not silently dropped: {:?}",
            result
                .skipped_fields
                .iter()
                .map(|s| s.name())
                .collect::<Vec<_>>()
        );
    }

    // And the field that genuinely is a bare parameter still lands, so this is a refusal of
    // unrenderable expressions and not a refusal of everything.
    assert!(
        sql.contains("AS \"plain\""),
        "a bare event param must still be projected:\n{sql}"
    );
    assert!(view.exact_fields.contains(&"plain".to_string()));
}

#[test]
fn an_exact_field_that_reaches_no_column_is_named_rather_than_dropped() {
    let schema = r#"
type Pool @entity {
  id: ID!
  accumulated: BigInt!
}
"#;
    // The operand is itself an expression, so this is neither a renderable view column nor an
    // accumulation #1214 can turn into `sum(col)`: `accumulation` takes one flat argument only,
    // precisely so an operand it cannot render does not become a sum over the wrong thing.
    let mapping = r#"
export function handlePoolCreated(event: PoolCreated): void {
  let pool = new Pool(event.params.pool.toHex())
  pool.id = event.params.pool.toHex()
  pool.accumulated = pool.accumulated.plus(event.params.fee.times(BigInt.fromI32(2)))
  pool.save()
}
"#;
    let (_nest, result) = emitted_nest(schema, mapping);
    assert!(
        result
            .skipped_fields
            .iter()
            .any(|s| s.entity == "Pool" && s.field == "accumulated"),
        "an accumulation reaches no column and must be named: {:?}",
        result
            .skipped_fields
            .iter()
            .map(|s| s.name())
            .collect::<Vec<_>>()
    );
    let view = result.views.iter().find(|v| v.entity == "Pool").unwrap();
    assert!(
        !view.exact_fields.contains(&"accumulated".to_string()),
        "exact_fields must not advertise a field the SQL does not contain: {:?}",
        view.exact_fields
    );
}

#[test]
fn the_generated_check_names_the_promised_columns_rather_than_star() {
    let (nest, result) = emitted_nest(OPS_SCHEMA, OPS_MAPPING);
    let check = std::fs::read_to_string(nest.path().join("checks/port_views.sql")).unwrap();
    let view = result.views.iter().find(|v| v.entity == "Pool").unwrap();

    assert!(
        !check.contains("SELECT * FROM"),
        "`SELECT *` binds whatever the view happens to contain and cannot see a missing promised \
         column, which is how #1248 passed every generated check:\n{check}"
    );
    for field in &view.exact_fields {
        assert!(
            check.contains(&format!("\"{field}\"")),
            "the check must name `{field}` so DuckDB refuses to bind if the view ever stops \
             projecting it:\n{check}"
        );
    }
}

/// Pins the *mechanism* the fix depends on: DuckDB refuses to bind a projection naming a column the
/// view does not have, so a check written that way can see a missing promised column.
///
/// **Read this together with its sibling above, and do not mistake one for the other.** Reverting
/// the projection to `SELECT *` kills `the_generated_check_names_the_promised_columns_rather_than_star`
/// and leaves this one green, because breaking the check by hand still names an absent column
/// whichever way the generator wrote it. So the sibling is the regression test for the change, and
/// this is the test that says why naming columns is worth doing at all. Neither is sufficient alone:
/// without the sibling nothing notices the generator regressing, and without this one the sibling is
/// asserting on a substring whose consequence nobody has demonstrated.
#[test]
fn the_generated_check_fails_if_a_promised_column_is_missing() {
    let (nest, _result) = emitted_nest(OPS_SCHEMA, OPS_MAPPING);
    let clean = nuthatch::check::check(nuthatch::cli::CheckArgs {
        name: None,
        dir: nest.path().display().to_string(),
        update: false,
    });
    assert!(
        clean.is_ok(),
        "the emitted nest must pass its own check: {clean:?}"
    );

    // Now break exactly the invariant #1248 broke: promise a column the view does not project.
    let path = nest.path().join("checks/port_views.sql");
    let sql = std::fs::read_to_string(&path).unwrap();
    let broken = sql.replace("FROM (SELECT ", "FROM (SELECT \"negated\", ");
    assert_ne!(
        broken, sql,
        "the check must have a projection to break:\n{sql}"
    );
    std::fs::write(&path, &broken).unwrap();

    let result = nuthatch::check::check(nuthatch::cli::CheckArgs {
        name: None,
        dir: nest.path().display().to_string(),
        update: false,
    });
    assert!(
        result.is_err(),
        "a check naming a column the view does not project must fail to bind, and did not:\n{broken}"
    );
}

// ---------------------------------------------------------------------------------------------
// #1250: a column name is resolved against the decoded schema, never snake_cased into existence.
// ---------------------------------------------------------------------------------------------

/// The regression test the fixture choice hid. `tickSpacing` is a camelCase ABI parameter, so the
/// decoded column is `tickSpacing`; the emitter used to ask for `tick_spacing` and the view did not
/// bind at all. **This binds rather than greps**, because the whole class of defect was invisible to
/// a test that only inspects SQL text: `four_classes_exact_sqrt_price_lands_in_a_view` exercises
/// `sqrtPriceX96` and passes on the broken code.
#[test]
fn a_camel_case_parameter_yields_a_view_that_binds() {
    let schema = r#"
type Pool @entity {
  id: ID!
  spacing: BigInt!
}
"#;
    let mapping = r#"
export function handlePoolCreated(event: PoolCreated): void {
  let pool = new Pool(event.params.pool.toHex())
  pool.id = event.params.pool.toHex()
  pool.spacing = event.params.tickSpacing
  pool.save()
}
"#;
    let (nest, result) = emitted_nest(schema, mapping);
    let view = result.views.iter().find(|v| v.entity == "Pool").unwrap();
    let sql = select_sql(&view.sql);

    // **What each assertion below actually guards, because they are not interchangeable.**
    //
    // The bind proves the emitted view is loadable, which is what the old code failed outright with
    // `Binder Error: Referenced column "tick_spacing" not found in FROM clause!`. It does *not*
    // catch a return to snake-casing on its own: `resolve_column` refuses a name the table does not
    // have, so the field would simply be skipped and the smaller view would bind perfectly well.
    // Measured, not assumed - restoring `snake_case` leaves this assertion green and prints
    // `✓ port_views: 1 row(s) match`.
    //
    // So the two below it are the ones that catch that regression: the field has to *land*, named
    // as the ABI names it, and nothing may be skipped. Read together, the three say the view loads
    // and carries the field, which is the whole claim.
    let check = nuthatch::check::check(nuthatch::cli::CheckArgs {
        name: None,
        dir: nest.path().display().to_string(),
        update: false,
    });
    assert!(
        check.is_ok(),
        "a camelCase parameter must produce a view that binds: {check:?}\n{sql}"
    );

    assert!(
        sql.contains("\"tickSpacing\""),
        "the decoded column is the ABI parameter verbatim:\n{sql}"
    );
    assert!(
        !sql.contains("tick_spacing"),
        "`tick_spacing` is not a column of this table and never was:\n{sql}"
    );
    assert!(
        result.skipped_fields.is_empty(),
        "`spacing` resolves to a real column, so nothing should be skipped: {:?}",
        result
            .skipped_fields
            .iter()
            .map(|s| s.name())
            .collect::<Vec<_>>()
    );
}

/// A name matching no column of the table is skipped by name, not written into a view that cannot
/// bind. `resolve_column` has no fuzzy fallback, so this also pins that a near-miss is refused
/// rather than resolved to something adjacent.
#[test]
fn a_name_that_matches_no_column_is_skipped_rather_than_emitted() {
    // `PoolCreated` has no `amount0`; the Swap ABI does, and this nest does not import it.
    let schema = r#"
type Pool @entity {
  id: ID!
  missing: BigInt!
}
"#;
    let mapping = r#"
export function handlePoolCreated(event: PoolCreated): void {
  let pool = new Pool(event.params.pool.toHex())
  pool.id = event.params.pool.toHex()
  pool.missing = event.params.amount0
  pool.save()
}
"#;
    let (nest, result) = emitted_nest(schema, mapping);
    assert!(
        result
            .skipped_fields
            .iter()
            .any(|s| s.entity == "Pool" && s.field == "missing"),
        "a parameter this event does not have must be named as skipped: {:?}",
        result
            .skipped_fields
            .iter()
            .map(|s| s.name())
            .collect::<Vec<_>>()
    );
    let check = nuthatch::check::check(nuthatch::cli::CheckArgs {
        name: None,
        dir: nest.path().display().to_string(),
        update: false,
    });
    assert!(
        check.is_ok(),
        "skipping the field must leave a nest that still checks: {check:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// #1214 / RFC-0044 S5: a running total is maintained incrementally, not folded in a view.
// ---------------------------------------------------------------------------------------------

const ACCUM_SCHEMA: &str = r#"
type Pool @entity {
  id: ID!
  totalFees: BigInt!
  latest: BigInt!
}
"#;

const ACCUM_MAPPING: &str = r#"
export function handlePoolCreated(event: PoolCreated): void {
  let pool = new Pool(event.params.pool.toHex())
  pool.id = event.params.pool.toHex()
  pool.totalFees = pool.totalFees.plus(event.params.fee)
  pool.latest = event.params.tickSpacing
  pool.save()
}
"#;

/// A field maintained incrementally is **answered**, and the figure has to say so separately.
///
/// Counting it as unanswered would under-report a port that did the right thing with its running
/// totals, and folding it into the view count would hide where the field actually lives. A mutation
/// setting `incremental: 0` survived this file before this test existed, which is how the gap showed
/// up: the two fixtures the coverage tests used had no materialised field between them (#1277).
#[test]
fn the_coverage_figure_counts_an_incremental_field_as_answered() {
    let (nest, result) = emitted_nest(ACCUM_SCHEMA, ACCUM_MAPPING);

    // `id` and `latest` are latest-value and land in the view; `totalFees` is a running total and is
    // maintained in `entities/pool.sql`. All three are classified exact.
    assert_eq!(
        result.coverage,
        nuthatch::port_emit::Coverage {
            classified_exact: 3,
            in_views: 2,
            incremental: 1,
        },
        "got {}",
        result.coverage.summary()
    );
    assert_eq!(
        result.coverage.answered(),
        3,
        "{}",
        result.coverage.summary()
    );
    assert_eq!(
        result.coverage.unanswered(),
        0,
        "nothing here is unanswered: {}",
        result.coverage.summary()
    );

    let readme = std::fs::read_to_string(nest.path().join("README.md")).unwrap();
    assert!(
        readme.contains(
            "3 of 3 fields the report calls exact are answered (100%): 2 in views, 1 maintained \
             incrementally, 0 not answered at all"
        ),
        "the README must say where the third field went:\n{readme}"
    );
}

/// The gate that decides the slice: the emitted entity has to satisfy RFC-0041's v1 shape rules and
/// bind. `nuthatch check` runs both, so this is the real validator rather than a substring.
#[test]
fn an_accumulated_field_is_emitted_as_an_incremental_entity_that_validates() {
    let (nest, result) = emitted_nest(ACCUM_SCHEMA, ACCUM_MAPPING);

    let entity = result
        .entities
        .iter()
        .find(|e| e.entity == "Pool")
        .unwrap_or_else(|| {
            panic!(
                "a running total must become an entity: {:?}",
                result.entities
            )
        });
    assert_eq!(entity.fields, vec!["totalFees".to_string()]);
    assert!(
        entity
            .sql
            .contains("sum(TRY_CAST(\"fee\" AS DECIMAL(38,0)))"),
        "the total is the sum of the deltas, through the checked cast RFC-0047 §2 C1 names:\n{}",
        entity.sql
    );
    // The half that keeps the cast honest. `TRY_CAST` yields NULL past 38 digits and `sum` skips
    // NULLs, so without this a real uint256 would leave a total silently short.
    assert!(
        entity.sql.contains("AS \"totalFees_overflow\""),
        "a checked cast must report the values it could not represent:\n{}",
        entity.sql
    );

    // Declared, and declared consistently: `entities.toml` requires the name to match the file stem
    // and the path to be exactly `entities/<name>.sql`.
    let toml = std::fs::read_to_string(nest.path().join("entities.toml")).unwrap();
    assert!(toml.contains("name = \"pool\""), "{toml}");
    assert!(toml.contains("sql = \"entities/pool.sql\""), "{toml}");
    assert!(nest.path().join("entities/pool.sql").is_file());

    // RFC-0041's shape gate and the binder, via the real validator.
    let issues = nuthatch::entities::validate(nest.path());
    assert!(
        issues.is_empty(),
        "the emitted entity must satisfy RFC-0041 v1: {:?}",
        issues
            .iter()
            .map(|i| format!("{}: {}", i.name, i.error))
            .collect::<Vec<_>>()
    );
    let check = nuthatch::check::check(nuthatch::cli::CheckArgs {
        name: None,
        dir: nest.path().display().to_string(),
        update: false,
    });
    assert!(check.is_ok(), "the ported nest must still check: {check:?}");
}

/// The accumulation must not *also* appear in the view, where `last()` would answer with the most
/// recent delta instead of the total - a wrong number rather than a missing one. The view keeps the
/// latest-value field, and the file says where the other one went.
#[test]
fn the_running_total_is_absent_from_the_view_and_the_view_says_where_it_went() {
    let (_nest, result) = emitted_nest(ACCUM_SCHEMA, ACCUM_MAPPING);
    let view = result.views.iter().find(|v| v.entity == "Pool").unwrap();

    assert!(
        !view.exact_fields.contains(&"totalFees".to_string()),
        "a running total is not a view field: {:?}",
        view.exact_fields
    );
    assert!(
        view.exact_fields.contains(&"latest".to_string()),
        "the latest-value field still belongs in the view: {:?}",
        view.exact_fields
    );
    assert!(
        view.sql
            .contains("maintained incrementally in entities/pool.sql"),
        "RFC-0044 §6 wants the artefact to say which of the two a field landed in:\n{}",
        view.sql
    );
    assert!(
        !result.skipped_fields.iter().any(|s| s.field == "totalFees"),
        "a field that landed as an entity is not skipped: {:?}",
        result
            .skipped_fields
            .iter()
            .map(|s| s.name())
            .collect::<Vec<_>>()
    );
}

/// The absence case, and the sprint's theme. A port with no running totals must leave **no**
/// `entities.toml` behind: one that declares nothing is itself a `check` error, so emitting an empty
/// file would turn a nest that is merely view-shaped into a nest that fails its own validation.
#[test]
fn a_port_with_no_running_totals_leaves_no_entities_file() {
    let (nest, result) = emitted_nest(OPS_SCHEMA, OPS_MAPPING);
    assert!(
        result.entities.is_empty(),
        "nothing here accumulates: {:?}",
        result.entities
    );
    assert!(
        !nest.path().join("entities.toml").exists(),
        "an entities.toml declaring nothing fails `nuthatch check`"
    );
    let check = nuthatch::check::check(nuthatch::cli::CheckArgs {
        name: None,
        dir: nest.path().display().to_string(),
        update: false,
    });
    assert!(
        check.is_ok(),
        "a nest with no entities is still a nest: {check:?}"
    );
}

/// `port-emit` is deliberately re-runnable into the same nest. When a mapping that once had an
/// accumulation loses it, the old generated declaration and SQL must disappear together; otherwise
/// `has_declarations` still starts the entity runtime from a stale file.
#[test]
fn rerunning_without_a_running_total_removes_the_prior_generated_entity() {
    let subgraph = subgraph_with(ACCUM_SCHEMA, ACCUM_MAPPING);
    let nest = tempfile::tempdir().unwrap();
    write_imported_nest(nest.path(), false);
    nuthatch::port_emit::emit(subgraph.path(), nest.path()).expect("initial emitting port");
    assert!(nest.path().join("entities.toml").is_file());
    assert!(nest.path().join("entities/pool.sql").is_file());

    std::fs::write(subgraph.path().join("schema.graphql"), OPS_SCHEMA).unwrap();
    std::fs::write(subgraph.path().join("src/mappings/core.ts"), OPS_MAPPING).unwrap();
    let result = nuthatch::port_emit::emit(subgraph.path(), nest.path()).expect("re-emitting port");

    assert!(
        result.entities.is_empty(),
        "nothing here accumulates: {:?}",
        result.entities
    );
    assert!(
        !nest.path().join("entities.toml").exists(),
        "the former declaration must not survive an empty re-emission"
    );
    assert!(
        !nest.path().join("entities").exists(),
        "the former generated SQL directory must not keep the nest entity-backed"
    );
}

/// A field accumulated by two event tables cannot be silently narrowed to the first table the
/// mapper happens to visit. v1 emits one relation per entity, so the second contribution is named
/// for the operator instead of claiming a total that omits it.
#[test]
fn an_accumulation_from_a_second_table_is_named_rather_than_discarded() {
    let subgraph = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(subgraph.path().join("src/mappings")).unwrap();
    std::fs::write(
        subgraph.path().join("schema.graphql"),
        r#"type Pool @entity {
  id: ID!
  totalFees: BigInt!
}

"#,
    )
    .unwrap();
    std::fs::write(
        subgraph.path().join("subgraph.yaml"),
        r#"specVersion: 0.0.8
dataSources:
  - kind: ethereum/contract
    name: Factory
    mapping:
      file: ./src/mappings/core.ts
      eventHandlers:
        - event: PoolCreated(indexed address,indexed address,indexed uint24,int24,address)
          handler: handlePoolCreated
templates:
  - kind: ethereum/contract
    name: Pool
    mapping:
      file: ./src/mappings/core.ts
      eventHandlers:
        - event: Swap(indexed address,indexed address,int256,int256,uint160,uint128,int24)
          handler: handlePoolSwap
"#,
    )
    .unwrap();
    std::fs::write(
        subgraph.path().join("src/mappings/core.ts"),
        r#"export function handlePoolCreated(event: PoolCreated): void {
  let pool = new Pool(event.params.pool.toHex())
  pool.id = event.params.pool.toHex()
  pool.totalFees = pool.totalFees.plus(event.params.fee)
  pool.save()
}

export function handlePoolSwap(event: Swap): void {
  let pool = new Pool(event.address.toHex())
  pool.id = event.address.toHex()
  pool.totalFees = pool.totalFees.plus(event.params.amount0)
  pool.save()
}
"#,
    )
    .unwrap();

    let nest = tempfile::tempdir().unwrap();
    write_imported_nest(nest.path(), true);
    let result = nuthatch::port_emit::emit(subgraph.path(), nest.path()).expect("emit");

    assert!(
        result.entities.iter().any(|entity| entity.entity == "Pool"),
        "the first relation is emitted: {:?}",
        result.entities
    );
    assert!(
        result.skipped_fields.iter().any(|field| {
            field.entity == "Pool" && field.field == "totalFees" && field.why.contains("pool__swap")
        }),
        "the second relation must be reported, not silently dropped: {:?}",
        result.skipped_fields
    );
}

#[test]
fn an_accumulation_from_another_entity_receiver_is_not_emitted_for_this_entity() {
    let mapping = r#"export function handlePoolCreated(event: PoolCreated): void {
  let pool = new Pool(event.params.pool.toHex())
  let other = new Pool(event.params.token0.toHex())
  pool.id = event.params.pool.toHex()
  other.id = event.params.token0.toHex()
  pool.totalFees = other.totalFees.plus(event.params.fee)
  pool.save()
  other.save()
}
"#;
    let (_nest, result) = emitted_nest(ACCUM_SCHEMA, mapping);
    assert!(
        result.entities.is_empty(),
        "a different receiver's value is not this entity's running total: {:?}",
        result.entities
    );
}

#[test]
fn a_nonempty_reemission_removes_entities_no_longer_generated() {
    let initial_schema = r#"type Pool @entity {
  id: ID!
  totalFees: BigInt!
}

type Token @entity {
  id: ID!
  totalSupply: BigInt!
}
"#;
    let initial_mapping = r#"export function handlePoolCreated(event: PoolCreated): void {
  let pool = new Pool(event.params.pool.toHex())
  let token = new Token(event.params.token0.toHex())
  pool.id = event.params.pool.toHex()
  token.id = event.params.token0.toHex()
  pool.totalFees = pool.totalFees.plus(event.params.fee)
  token.totalSupply = token.totalSupply.plus(event.params.fee)
  pool.save()
  token.save()
}
"#;
    let subgraph = subgraph_with(initial_schema, initial_mapping);
    let nest = tempfile::tempdir().unwrap();
    write_imported_nest(nest.path(), false);
    nuthatch::port_emit::emit(subgraph.path(), nest.path()).expect("initial emitting port");
    assert!(nest.path().join("entities/pool.sql").is_file());
    assert!(nest.path().join("entities/token.sql").is_file());
    std::fs::write(
        nest.path().join("entities/manual.sql"),
        "SELECT id FROM factory__pool_created",
    )
    .unwrap();
    let mut manifest = std::fs::read_to_string(nest.path().join("entities.toml")).unwrap();
    manifest.push_str(
        "\n[[entities]]\nname = \"manual\"\nsql = \"entities/manual.sql\"\nkey = [\"id\"]\nmax_rows = 100\n",
    );
    std::fs::write(nest.path().join("entities.toml"), manifest).unwrap();

    std::fs::write(subgraph.path().join("schema.graphql"), ACCUM_SCHEMA).unwrap();
    std::fs::write(subgraph.path().join("src/mappings/core.ts"), ACCUM_MAPPING).unwrap();
    nuthatch::port_emit::emit(subgraph.path(), nest.path()).expect("re-emitting port");

    assert!(nest.path().join("entities/pool.sql").is_file());
    assert!(
        !nest.path().join("entities/token.sql").exists(),
        "a generator-owned entity omitted by a non-empty re-emission must disappear"
    );
    let manifest = std::fs::read_to_string(nest.path().join("entities.toml")).unwrap();
    assert!(manifest.contains("name = \"manual\""));
    assert!(nest.path().join("entities/manual.sql").is_file());
}

/// #1277. A local that only aliases an event parameter is the shape every `Pool.id` takes in the
/// real Uniswap V4 subgraph:
///
/// ```text
/// const poolId = event.params.id.toHexString()
/// const pool = new Pool(poolId)
/// ```
///
/// The emitter resolved the constructor argument with `event_column` alone, which sees a bare
/// identifier and refuses, so the primary key of all 132,765 pools was reported as *"no decoded
/// column corresponds to it"* while the column sat in `pool_manager__initialize` already
/// hex-encoded. Without an id in the view, every other field the view answered was unalignable
/// against a reference, which is why this one field decided the acceptance port.
#[test]
fn an_id_from_a_local_aliasing_an_event_param_reaches_its_column() {
    let schema = r#"
type Pool @entity {
  id: ID!
  plain: BigInt!
}
"#;
    let mapping = r#"
export function handlePoolCreated(event: PoolCreated): void {
  const poolId = event.params.pool.toHexString()
  const pool = new Pool(poolId)
  pool.plain = event.params.fee
  pool.save()
}
"#;
    let (nest, result) = emitted_nest(schema, mapping);
    let view = result.views.iter().find(|v| v.entity == "Pool").unwrap();
    let sql = select_sql(&view.sql);

    assert!(
        sql.contains("AS \"id\""),
        "`Pool.id` must bind to the column its local aliases (#1277):\n{sql}"
    );
    assert!(
        sql.contains("\"pool\""),
        "and the column is `pool`, as the ABI names it:\n{sql}"
    );
    assert!(
        !result
            .skipped_fields
            .iter()
            .any(|s| s.name().contains("id")),
        "`Pool.id` must not be skipped: {:?}",
        result
            .skipped_fields
            .iter()
            .map(|s| s.name())
            .collect::<Vec<_>>()
    );

    // The view has to load, not merely mention the column.
    let check = nuthatch::check::check(nuthatch::cli::CheckArgs {
        name: None,
        dir: nest.path().display().to_string(),
        update: false,
    });
    assert!(
        check.is_ok(),
        "the emitted view must bind: {check:?}\n{sql}"
    );
}

/// #1277. An entity whose every exact field reaches no column used to get
/// `CREATE VIEW "<entity>" AS SELECT 1 AS port_placeholder`, so that `nuthatch check` had a table to
/// bind. On the RFC-0044 S3 acceptance port that was nine of nineteen entities, `Bundle` and
/// `PoolManager` among them, and `SELECT count(*) FROM "bundle"` answered `1` with a full provenance
/// block. The reference has exactly one `Bundle`, so a comparison on row counts agreed with it.
///
/// `Bundle` here is the real shape: a literal id and a constant, neither of which is a column.
const NO_COLUMN_SCHEMA: &str = r#"
type Pool @entity {
  id: ID!
  plain: BigInt!
}
type Bundle @entity {
  id: ID!
  ethPriceUSD: BigDecimal!
}
"#;

const NO_COLUMN_MAPPING: &str = r#"
export function handlePoolCreated(event: PoolCreated): void {
  let pool = new Pool(event.params.pool.toHex())
  pool.id = event.params.pool.toHex()
  pool.plain = event.params.fee
  pool.save()
  let bundle = new Bundle('1')
  bundle.ethPriceUSD = ZERO_BD
  bundle.save()
}
"#;

#[test]
fn an_entity_whose_fields_reach_no_column_gets_no_view_at_all() {
    let (nest, result) = emitted_nest(NO_COLUMN_SCHEMA, NO_COLUMN_MAPPING);

    assert!(
        result.views.iter().all(|v| v.entity != "Bundle"),
        "no view may be emitted for Bundle: {:?}",
        result.views.iter().map(|v| &v.entity).collect::<Vec<_>>()
    );
    assert!(
        result
            .entities_without_views
            .contains(&"Bundle".to_string()),
        "and it must be named rather than silently absent: {:?}",
        result.entities_without_views
    );
    assert!(
        !nest.path().join("views/20-bundle.sql").exists(),
        "no file either - a view returning 1 is worse than no table"
    );

    // Pool still lands. The change must not cost an entity that does resolve.
    assert!(
        result.views.iter().any(|v| v.entity == "Pool"),
        "Pool resolves `plain` and must still be emitted"
    );

    // The check must not name Bundle. Its projection for such a view was `*`, which binds whatever
    // the view holds and so asserted nothing while reading `binds: true`.
    let check = std::fs::read_to_string(nest.path().join("checks/port_views.sql")).unwrap();
    assert!(
        !check.contains("'Bundle'"),
        "the check may not claim an entity that has no view:\n{check}"
    );
    assert!(
        !check.contains("SELECT * FROM"),
        "and no row of it may project `*`:\n{check}"
    );
    let expected =
        std::fs::read_to_string(nest.path().join("checks/expected/port_views.json")).unwrap();
    assert!(
        !expected.contains("Bundle"),
        "nor may the committed expectation:\n{expected}"
    );
}

/// A nest emitted before that change carries nine placeholder views. Re-emitting has to remove the
/// one for an entity that now emits none, or the stale table survives and nothing re-examines it.
#[test]
fn reemission_removes_a_stale_generated_view() {
    let subgraph = subgraph_with(NO_COLUMN_SCHEMA, NO_COLUMN_MAPPING);
    let nest = tempfile::tempdir().unwrap();
    write_imported_nest(nest.path(), false);
    let views = nest.path().join("views");
    std::fs::create_dir_all(&views).unwrap();

    // Exactly what the previous emitter wrote, header and all.
    let stale = views.join("20-bundle.sql");
    std::fs::write(
        &stale,
        "-- Exact fields of `Bundle`. Call-derived, fixed-point and unreachable fields are not \
         here; see README.md.\n\nCREATE VIEW \"bundle\" AS SELECT 1 AS port_placeholder;\n",
    )
    .unwrap();
    // And an operator's own view, which must survive.
    let mine = views.join("30-mine.sql");
    std::fs::write(&mine, "CREATE VIEW \"mine\" AS SELECT 1 AS ok;\n").unwrap();

    nuthatch::port_emit::emit(subgraph.path(), nest.path()).expect("emit");

    assert!(
        !stale.exists(),
        "the stale generated view for Bundle must be removed"
    );
    assert!(
        mine.exists(),
        "a view the operator wrote must not be removed"
    );
}

/// #1277. A field assigned inside a helper that is itself called by a helper reaches no column,
/// because `event_handler_for` looks only one hop: it finds an event handler that calls the function
/// directly. The Uniswap V4 subgraph routes every handler through that indirection -
/// `handleSwap` -> `handleSwapHelper` -> `loadTransaction` - so everything `loadTransaction` writes
/// is lost, `Transaction.blockNumber` and `Transaction.timestamp` among them, even though
/// `event.block.number` and `event.block.timestamp` are both mapped to implicit columns already.
#[test]
fn a_field_written_two_helper_hops_from_the_handler_still_reaches_its_column() {
    let schema = r#"
type Pool @entity {
  id: ID!
  createdAt: BigInt!
}
"#;
    let mapping = r#"
export function stamp(event: PoolCreated): void {
  let pool = new Pool(event.params.pool.toHex())
  pool.id = event.params.pool.toHex()
  pool.createdAt = event.block.timestamp
  pool.save()
}
export function handlePoolCreatedHelper(event: PoolCreated): void {
  stamp(event)
}
export function handlePoolCreated(event: PoolCreated): void {
  handlePoolCreatedHelper(event)
}
"#;
    let (_nest, result) = emitted_nest(schema, mapping);
    let view = result
        .views
        .iter()
        .find(|v| v.entity == "Pool")
        .expect("a Pool view");
    let sql = select_sql(&view.sql);
    assert!(
        sql.contains("\"block_timestamp\" AS \"createdAt\""),
        "`event.block.timestamp` two hops from the handler must still bind (#1277):\n{sql}"
    );
}

/// Raised by review of #1279. `plain_local_expr` scanned from the top and returned the first
/// declaration of the name, which is not the one in scope at the use site:
///
/// ```text
/// const poolId = event.params.token1.toHexString()   // outer
/// if (..) {
///   const poolId = event.params.pool.toHexString()   // shadows it
///   const pool = new Pool(poolId)                    // uses this one
/// }
/// ```
///
/// Emitting `Pool.id` from `token1` there would answer the wrong column under a report promising
/// byte-identical - #1248's fault in a new place. Scanning cannot resolve scope, so an ambiguous
/// name resolves to nothing and the field is reported as reaching no column, exactly as it was
/// before the alias fallback existed.
#[test]
fn a_shadowed_local_is_refused_rather_than_resolved_to_the_wrong_one() {
    let schema = r#"
type Pool @entity {
  id: ID!
  plain: BigInt!
}
"#;
    let mapping = r#"
export function handlePoolCreated(event: PoolCreated): void {
  const poolId = event.params.token1.toHexString()
  if (event.params.fee.gt(BigInt.zero())) {
    const poolId = event.params.pool.toHexString()
    const pool = new Pool(poolId)
    pool.plain = event.params.fee
    pool.save()
  }
}
"#;
    let (_nest, result) = emitted_nest(schema, mapping);
    let view = result.views.iter().find(|v| v.entity == "Pool");
    if let Some(view) = view {
        let sql = select_sql(&view.sql);
        assert!(
            !sql.contains("\"token1\" AS \"id\""),
            "the outer `poolId` must not answer `Pool.id`; the inner one shadows it:\n{sql}"
        );
    }
    assert!(
        result
            .skipped_fields
            .iter()
            .any(|s| s.entity == "Pool" && s.field == "id"),
        "an ambiguous local must leave `Pool.id` reported as reaching no column: {:?}",
        result
            .skipped_fields
            .iter()
            .map(|s| s.name())
            .collect::<Vec<_>>()
    );
}
