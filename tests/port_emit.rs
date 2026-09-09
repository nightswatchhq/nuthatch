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

#[test]
fn readme_is_the_port_report_verbatim() {
    let nest = tempfile::tempdir().unwrap();
    write_imported_nest(nest.path(), false);
    let result = nuthatch::port_emit::emit(&one_call_dir(), nest.path()).unwrap();
    let readme = std::fs::read_to_string(nest.path().join("README.md")).unwrap();
    assert_eq!(readme, result.report);
    assert!(readme.contains("# Port report"));
    assert!(readme.contains("call-derived"));
    assert!(readme.contains("`Token.symbol`"));
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

fn emitted_nest(schema: &str, mapping: &str) -> (tempfile::TempDir, nuthatch::port_emit::EmitResult) {
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

const OPS_MAPPING: &str = r#"
export function handlePoolCreated(event: PoolCreated): void {
  let pool = new Pool(event.params.pool.toHex())
  pool.id = event.params.pool.toHex()
  pool.plain = event.params.tickSpacing
  pool.negated = event.params.fee.neg()
  pool.scaled = event.params.fee.times(BigInt.fromI32(1000))
  pool.summed = event.params.fee.plus(event.params.tickSpacing)
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
    let mapping = r#"
export function handlePoolCreated(event: PoolCreated): void {
  let pool = new Pool(event.params.pool.toHex())
  pool.id = event.params.pool.toHex()
  pool.accumulated = pool.accumulated.plus(event.params.fee)
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
