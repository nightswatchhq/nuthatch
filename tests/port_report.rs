//! RFC-0044 S1: the committed four-class fixture report is the drift gate. A class change or a
//! deleted citation is a red test, the same shape `tests/skill_refs.rs` is for the builder skill.

use std::path::PathBuf;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("skills/nuthatch-subgraph-port/fixtures/four-classes")
}

#[test]
fn four_classes_fixture_matches_committed_report() {
    let dir = fixture_dir();
    let report = nuthatch::port_report::classify_dir(&dir).expect("classify fixture");
    let got = nuthatch::port_report::render_report(&report);
    let expected_path = dir.join("expected.md");
    let expected = std::fs::read_to_string(&expected_path).unwrap_or_else(|e| {
        panic!(
            "expected.md must be committed next to the fixture ({e}). Re-run this test after \
             writing the classifier's output to {}",
            expected_path.display()
        )
    });
    assert_eq!(
        got, expected,
        "four-classes expected.md drifted - if the classes are right, commit the new report; \
         if a class changed, that is the bug"
    );
}

#[test]
fn four_classes_fixture_hits_each_class_on_the_known_fields() {
    let report = nuthatch::port_report::classify_dir(&fixture_dir()).unwrap();
    let class = |entity: &str, field: &str| {
        report
            .fields
            .iter()
            .find(|r| r.entity == entity && r.field == field)
            .unwrap_or_else(|| panic!("missing {entity}.{field}"))
            .class
    };
    use nuthatch::port_report::Class;
    assert_eq!(class("Pool", "sqrtPrice"), Class::Exact);
    // Flipped by #1274. `getEthPriceInUSD()` returns a price read back off a stored `Pool`, and
    // `classes.md` defines fixed point as "reads back own or another entity's prior output". The
    // fixture asserted `Exact` because the classifier only ever seeded fixed point from a load
    // inside a *loop*, so this point-load sibling escaped - the identical shape one field over.
    assert_eq!(class("Bundle", "ethPriceUSD"), Class::FixedPoint);
    assert_eq!(class("Pool", "swaps"), Class::Exact);
    assert_eq!(class("Token", "symbol"), Class::CallDerived);
    assert_eq!(class("Token", "decimals"), Class::CallDerived);
    assert_eq!(class("Token", "derivedETH"), Class::FixedPoint);
    assert_eq!(class("_Schema_", "tokenSearch"), Class::Unreachable);
    assert_eq!(class("Token", "name"), Class::Unreachable);
    assert_eq!(class("BlockStat", "blockNumber"), Class::Unreachable);

    let derived = report
        .fields
        .iter()
        .find(|r| r.entity == "Token" && r.field == "derivedETH")
        .unwrap();
    assert!(
        derived.reason.contains("will not reproduce"),
        "fixed-point must say it will not reproduce: {}",
        derived.reason
    );
    assert!(
        derived.citation.file.contains("pricing.ts") || derived.citation.file.contains("core.ts"),
        "derivedETH citation should name the mapping, got {}",
        derived.citation.file
    );
}

#[test]
fn missing_schema_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let err = nuthatch::port_report::classify_dir(dir.path()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("schema.graphql"),
        "must name the missing file: {msg}"
    );
}

/// A subgraph directory holding a manifest with one `Factory.PoolCreated` handler, a schema and one
/// mapping file, so a case is classified through `load_mappings` exactly as `port-report` does.
fn subgraph(schema: &str, mapping: Option<&str>) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("subgraph.yaml"),
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
      file: ./src/mapping.ts
      entities: []
      abis:
        - name: Factory
          file: ./abis/factory.json
      eventHandlers:
        - event: PoolCreated(indexed address,indexed address,indexed uint24,int24,address)
          handler: handlePoolCreated
"#,
    )
    .unwrap();
    std::fs::write(dir.path().join("schema.graphql"), schema).unwrap();
    if let Some(m) = mapping {
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/mapping.ts"), m).unwrap();
    }
    dir
}

fn row<'a>(
    report: &'a nuthatch::port_report::Report,
    entity: &str,
    field: &str,
) -> &'a nuthatch::port_report::FieldRow {
    report
        .fields
        .iter()
        .find(|r| r.entity == entity && r.field == field)
        .unwrap_or_else(|| panic!("missing {entity}.{field}"))
}

/// #1947: a deployment CID gives the manifest and the schema, never the `.ts`. The report must say it
/// read no mapping rather than claim no mapping writes each field.
#[test]
fn a_subgraph_with_no_mapping_source_says_nothing_was_classified() {
    let dir = subgraph("type Pool @entity {\n  id: ID!\n  fee: BigInt!\n}\n", None);
    let report = nuthatch::port_report::classify_dir(dir.path()).unwrap();
    assert!(!report.mapping_source, "{:?}", report.missing_handlers);
    assert_eq!(
        report.missing_handlers,
        vec!["handlePoolCreated".to_string()]
    );
    let fee = row(&report, "Pool", "fee");
    assert!(
        !fee.reason.contains("no mapping writes this field"),
        "nothing was read, so nothing can be said about writes: {}",
        fee.reason
    );
    let text = nuthatch::port_report::render_report(&report);
    assert!(text.contains("Nothing was classified"), "{text}");
}

/// The Messari SDK writes from class methods through `this._market` and with `+=` (S0 defect 7,
/// morpho). Both were invisible, so the field read "no mapping writes this field".
#[test]
fn a_compound_write_in_a_class_method_is_a_write() {
    let schema =
        "type Market @entity {\n  id: ID!\n  depositCount: BigInt!\n  volume: BigInt!\n}\n";
    let mapping = r#"
export class DataManager {
  private _market: Market;

  constructor(id: Bytes) {
    this._market = Market.load(id)!;
  }

  addDeposit(amount: BigInt): void {
    this._market.depositCount += INT_ONE;
    this._market.save();
  }
}

export function handlePoolCreated(event: PoolCreated): void {
  let market = Market.load(event.params.pool)!
  market.volume += event.params.fee
  market.save()
  new DataManager(event.params.pool).addDeposit(event.params.fee)
}
"#;
    let dir = subgraph(schema, Some(mapping));
    let report = nuthatch::port_report::classify_dir(dir.path()).unwrap();
    use nuthatch::port_report::Class;

    let count = row(&report, "Market", "depositCount");
    assert_eq!(count.class, Class::Exact, "{}", count.reason);
    assert_eq!(
        count.citation.display(),
        "src/mapping.ts:10",
        "{}",
        count.reason
    );

    let volume = row(&report, "Market", "volume");
    assert_eq!(volume.class, Class::Exact, "{}", volume.reason);
    assert_eq!(
        volume.citation.display(),
        "src/mapping.ts:17",
        "{}",
        volume.reason
    );
    assert!(
        volume.reason.contains("event.params.fee"),
        "{}",
        volume.reason
    );
}

/// Bunni's `Bribe.amount` (S0 defect 7): the entity comes back from a `getOrCreate` helper that
/// initialises it to zero, and the handler then sets it from the event. The report cited the zero.
#[test]
fn a_field_set_on_an_entity_from_a_get_or_create_helper_cites_the_handler() {
    let schema = "type Bribe @entity {\n  id: ID!\n  amount: BigInt!\n  token: Bytes!\n}\n\
                  type Quest @entity {\n  id: ID!\n  amount: BigInt!\n}\n";
    let mapping = r#"
export function getBribe(id: Bytes, index: i32): Bribe {
  let bribe = Bribe.load(id.toHex() + '-' + index.toString());
  if (bribe == null) {
    bribe = new Bribe(id.toHex() + '-' + index.toString());
    bribe.amount = ZERO_INT;
    bribe.token = ZERO_ADDR;
    bribe.save();
  }
  return bribe as Bribe;
}

export function handlePoolCreated(event: PoolCreated): void {
  let bribe = getBribe(event.params.pool, 0);
  bribe.amount = event.params.fee;
  bribe.save();
}
"#;
    let dir = subgraph(schema, Some(mapping));
    let report = nuthatch::port_report::classify_dir(dir.path()).unwrap();
    let amount = row(&report, "Bribe", "amount");
    assert_eq!(amount.class, nuthatch::port_report::Class::Exact);
    assert_eq!(
        amount.citation.display(),
        "src/mapping.ts:15",
        "{}",
        amount.reason
    );
    assert!(
        amount.reason.contains("event.params.fee"),
        "{}",
        amount.reason
    );
}

/// Peeranha's `Achievement.achievementURI` (S0 defect 7): a read through a helper that returns a bound
/// contract was classed exact. It is a contract read at the row's block.
#[test]
fn a_read_through_a_helper_returning_a_bound_contract_is_call_derived() {
    let schema = "type Achievement @entity {\n  id: ID!\n  achievementURI: String!\n}\n\
                  type Token @entity {\n  id: ID!\n  symbol: String!\n}\n\
                  type Swap @entity {\n  id: ID!\n  token: Token!\n}\n";
    let mapping = r#"
export function getPeeranhaNFT(): PeeranhaNFT {
  return PeeranhaNFT.bind(Address.fromString(NFT_ADDRESS));
}

export function getOrCreateToken(address: Address): Token {
  let token = Token.load(address.toHex())
  if (token == null) {
    token = new Token(address.toHex())
    token.symbol = ERC20.bind(address).symbol()
    token.save()
  }
  return token as Token
}

export function handlePoolCreated(event: PoolCreated): void {
  let achievement = new Achievement(event.params.pool.toHex());
  let config = getPeeranhaNFT().getAchievementsNFTConfig(event.params.fee);
  achievement.achievementURI = config.achievementURI;
  achievement.save();

  let token = getOrCreateToken(event.params.token0)
  let swap = new Swap(event.params.pool.toHex())
  swap.token = token.id
  swap.save()
}
"#;
    let dir = subgraph(schema, Some(mapping));
    let report = nuthatch::port_report::classify_dir(dir.path()).unwrap();
    use nuthatch::port_report::Class;
    let uri = row(&report, "Achievement", "achievementURI");
    assert_eq!(uri.class, Class::CallDerived, "{}", uri.reason);
    assert_eq!(
        uri.citation.display(),
        "src/mapping.ts:3",
        "cite the bind that makes it a contract read: {}",
        uri.reason
    );
    // The id of an entity a call-derived helper returns is still its argument (#1294).
    let token = row(&report, "Swap", "token");
    assert_eq!(token.class, Class::Exact, "{}", token.reason);
}
