//! **A GraphQL query answered over genuinely indexed data.** RFC-0053's definition of done is that a
//! client changes nothing but the URL, and until now every test of the dialect ran views over literal
//! `SELECT`s: the compiler was proved and the thing it compiles against was a fixture. That is the
//! largest gap between "the lane works" and "a nest answers", and it is the one a reader would assume
//! was already closed.
//!
//! So this drives the real `indexer::spawn_nest` loop against a scripted [`TapeSource`], lands blocks,
//! seals some past finality, serves the real router over real HTTP, and POSTs GraphQL at it. Every
//! asserted value traces to a log the tape emitted, so a wrong number is a wrong number rather than a
//! fixture agreeing with itself.

use std::sync::Arc;
use std::time::Duration;

use nuthatch::{indexer, serve};

mod common;

use common::tape::{account, scaffold_nest, transfer_table, transfers_block, TapeSource, USDC};

const POLL_TIMEOUT: Duration = Duration::from_secs(30);

/// The overlay a port writes: a Graph schema and a view per entity. Hand-written here rather than
/// generated, because what is under test is the serving path, not `port-emit`.
fn write_graph_overlay(dir: &std::path::Path) {
    std::fs::create_dir_all(dir.join("graph")).unwrap();
    std::fs::write(
        dir.join("graph/schema.graphql"),
        r#"
type Transfer @entity {
  id: ID!
  from: Bytes!
  to: Bytes!
  value: BigInt!
  blockNumber: BigInt!
}
"#,
    )
    .unwrap();
    std::fs::create_dir_all(dir.join("views")).unwrap();
    // `id` is the entity key the store already uses - block and log index - so a GraphQL `id` is a real
    // identity rather than something invented for this test.
    std::fs::write(
        dir.join("views/20-transfer.sql"),
        // **Numeric columns, as a generated view writes them.** Casting `value` to VARCHAR here made
        // `orderBy: value` lexicographic and ranked 9000351 above 60000353; the string the wire needs is
        // the query lane's job, not the view's.
        "CREATE VIEW transfer AS SELECT \
           CAST(block_number AS VARCHAR) || '-' || CAST(log_index AS VARCHAR) AS \"id\", \
           \"from\" AS \"from\", \"to\" AS \"to\", \
           \"value\" AS \"value\", \
           block_number AS \"blockNumber\" \
         FROM usdc__transfer;",
    )
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_graphql_query_is_answered_over_genuinely_indexed_data() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = scaffold_nest(dir.path(), "usdc", USDC);
    write_graph_overlay(dir.path());

    let tape = Arc::new(TapeSource::new());
    let a1 = account(1);
    let a2 = account(2);
    // **Enough rows to actually seal.** The row cut is `SEAL_DIRECT_BATCH` (20,000) and the span cut is
    // 10,800 blocks, so a five-block fixture never leaves the hot store - the first version of this test
    // passed while proving nothing about Parquet at all. Sixty blocks of ~350 transfers crosses the row
    // cut once; finality short of the tip leaves the last blocks hot, so the query has to span both.
    for b in 1..=60u64 {
        let n = 350 + (b % 7);
        let transfers: Vec<(&str, &str, u128)> = (0..n)
            .map(|i| {
                (
                    a1.as_str(),
                    a2.as_str(),
                    (100_000_000 + 1_000_000 * b + i) as u128,
                )
            })
            .collect();
        tape.insert_block(
            b,
            transfers_block(b, 0, 1_700_000_000 + b, USDC, &transfers),
        );
    }
    let total_rows: u64 = (1..=60u64).map(|b| 350 + (b % 7)).sum();
    // Every value is nine digits. **Deliberate**: a `BigInt` is stored as canonical text
    // (`analytics.rs:2253`), so `orderBy` and numeric `where` compare text, which agrees with numeric
    // order only while the digit counts match. Mixed magnitudes here would make this test depend on that
    // defect, either by passing because of it or by failing for a reason that is not about serving over
    // indexed data. It is filed separately; this fixture stays out of its way.
    let value_of = |b: u64, i: u64| 100_000_000 + 1_000_000 * b + i;
    tape.advance_tip_to(60);
    tape.advance_finalized_to(58);

    let rt = indexer::spawn_nest(
        tape.clone(),
        dir.path().to_path_buf(),
        cfg,
        None,
        false,
        1,
        Some(2),
        false,
        None,
    )
    .await
    .expect("spawn_nest");
    let ingest = rt.ingest;
    let alert_worker = rt.alert_worker;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let app = serve::router(serve::SharedNest::new(rt.state));
    let server = tokio::spawn(async move { axum::serve(listener, app).await });
    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // Wait on the indexer, not on a sleep. Counted through `/sql`, which spans hot and sealed, because
    // `/` reports the hot store alone - on this fixture it settles at about a thousand of the twenty-one
    // thousand rows, the rest having been sealed out of it.
    let count_all = || {
        let client = client.clone();
        let base = base.clone();
        async move {
            client
                .get(format!("{base}/sql"))
                .query(&[("q", "SELECT count(*) AS n FROM usdc__transfer")])
                .send()
                .await
                .ok()?
                .json::<serde_json::Value>()
                .await
                .ok()?["rows"][0]["n"]
                .as_u64()
        }
    };
    let start = std::time::Instant::now();
    let mut rows = 0u64;
    while start.elapsed() < POLL_TIMEOUT {
        rows = count_all().await.unwrap_or(0);
        if rows >= total_rows {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert_eq!(
        rows, total_rows,
        "every transfer must be indexed before anything is asked of the query lane"
    );

    // The hot store holds a fraction of them, which is the two-layer claim stated from the other side: if
    // the query lane read only the hot store, most of what follows would be missing rather than wrong.
    let root: serde_json::Value = client
        .get(&base)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let hot = root["entities"].as_u64().expect("a hot entity count");
    assert!(
        hot < total_rows,
        "most rows must have left the hot store for Parquet, found {hot} of {total_rows} still hot"
    );

    // **Both layers, proved rather than assumed.** The first version of this test asserted five rows and
    // passed with nothing ever sealed, which made it a hot-store test wearing a hat.
    let start = std::time::Instant::now();
    let mut sealed_through = 0u64;
    while start.elapsed() < POLL_TIMEOUT {
        if let Ok(m) = nuthatch::seal::load_manifest(dir.path()) {
            if let Some(segs) = m.tables.get(&transfer_table("usdc")) {
                sealed_through = segs.iter().map(|s| s.to_block).max().unwrap_or(0);
                if sealed_through > 0 {
                    break;
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(
        sealed_through > 0,
        "the fixture must seal something or the two-layer claim is empty"
    );
    assert!(
        sealed_through < 60,
        "and it must not seal everything, or the hot half of the claim is empty: sealed through \
         {sealed_through}"
    );

    let gql = |q: serde_json::Value| {
        let client = client.clone();
        let base = base.clone();
        async move {
            client
                .post(format!("{base}/graphql"))
                .json(&q)
                .send()
                .await
                .expect("post")
                .json::<serde_json::Value>()
                .await
                .expect("json")
        }
    };

    // **One row that exists only in Parquet, and one that exists only in the hot store**, picked from
    // the boundary the run actually produced rather than one assumed in advance. A lane that reads a
    // single layer answers one of these and not the other.
    let cold_id = format!("{sealed_through}-0");
    let hot_id = "60-0".to_string();
    let cold_value = format!("{}", value_of(sealed_through, 0));
    for (id, want) in [
        (&cold_id, &cold_value),
        (&hot_id, &format!("{}", value_of(60, 0))),
    ] {
        let body = gql(serde_json::json!({
            "query": format!("{{ transfer(id: \"{id}\") {{ id value from }} }}")
        }))
        .await;
        assert_eq!(
            body["data"]["transfer"],
            serde_json::json!({"id": id, "value": want, "from": format!("{a1}")}),
            "the query lane must answer row {id}, which lives {} the seal boundary at \
             {sealed_through}: {body}",
            if id == &cold_id { "below" } else { "above" }
        );
    }

    // A filter and an order over the whole set, sealed and hot together. `value` is `1_000_000 * block +
    // i`, so the three largest are the last three of block 60 - rows that are hot - and the filter has to
    // have seen the sealed ones to rank them out.
    let body = gql(serde_json::json!({
        "query": "{ transfers(orderBy: value, orderDirection: desc, first: 3) { value } }"
    }))
    .await;
    let top: Vec<&str> = body["data"]["transfers"]
        .as_array()
        .expect("an array")
        .iter()
        .map(|r| r["value"].as_str().expect("a value"))
        .collect();
    let last = 350 + (60 % 7) - 1;
    assert_eq!(
        top,
        vec![
            format!("{}", value_of(60, last)),
            format!("{}", value_of(60, last - 1)),
            format!("{}", value_of(60, last - 2)),
        ],
        "descending order over the whole indexed set: {body}"
    );

    // A range that straddles the boundary: every row of the sealed block and the block after it. `first:
    // 1000` because the default page is 100, as graph-node's is - the first version of this assertion got
    // exactly 100 rows back, which is the page size doing its job rather than a missing row.
    let body = gql(serde_json::json!({
        "query": format!(
            "{{ transfers(first: 1000, where: {{ value_gte: \"{}\", value_lt: \"{}\" }}) \
             {{ value }} }}",
            value_of(sealed_through, 0),
            value_of(sealed_through + 2, 0),
        )
    }))
    .await;
    let n = body["data"]["transfers"]
        .as_array()
        .expect("an array")
        .len() as u64;
    let want = (350 + (sealed_through % 7)) + (350 + ((sealed_through + 1) % 7));
    assert_eq!(
        n, want,
        "a range spanning the seal boundary must return every row on both sides of it: {body}"
    );

    // **A `BigInt` arrives as a GraphQL string**, which is what graph-node sends
    // (`graph/src/data/store/mod.rs:568`) and what every generated client parses. The column is
    // `DECIMAL(38,0)` in the view, so without the cast this is a JSON number and a client reading a value
    // above 2^53 loses it silently.
    let body = gql(serde_json::json!({
        "query": "{ transfers(orderBy: value, orderDirection: desc, first: 1) { value blockNumber } }"
    }))
    .await;
    let row = &body["data"]["transfers"][0];
    assert!(
        row["value"].is_string() && row["blockNumber"].is_string(),
        "`BigInt` must arrive as a string, as graph-node sends it: {body}"
    );

    // And `_meta` reports the head this nest actually reached, not a constant.
    let body = gql(serde_json::json!({ "query": "{ _meta { block { number } } }" })).await;
    assert_eq!(
        body["data"]["_meta"]["block"]["number"], 60,
        "`_meta` must report the head the indexer reached: {body}"
    );

    server.abort();
    ingest.abort();
    if let Some(w) = alert_worker {
        w.abort();
    }
}
