//! RFC-0059 S0 operator check over the independently ingested Arbitrum corpus.
//! It is ignored in CI because the corpus is not part of the repository.
use nuthatch::analytics;
use serde_json::Value;
use std::path::Path;

const INDEXER: &str = "0xeeeee689aa442c607105f29f06d00d2f748776b2";

#[derive(Clone)]
struct Event {
    block: u64,
    log: u64,
    timestamp: u64,
    delta: String,
    reward: String,
    shares: String,
    thawing: String,
    cut: String,
}

fn decimal(row: &Value, name: &str, signed: bool) -> String {
    let value = row[name]
        .as_str()
        .map(str::to_owned)
        .or_else(|| row[name].as_u64().map(|n| n.to_string()))
        .unwrap_or_else(|| panic!("missing {name}: {row}"));
    assert!(
        !value.is_empty()
            && value
                .strip_prefix('-')
                .unwrap_or(&value)
                .bytes()
                .all(|b| b.is_ascii_digit())
            && (signed || !value.starts_with('-')),
        "bad {name}: {value}"
    );
    value
}

fn events(corpus: &Path) -> Vec<Event> {
    let sql = format!(
        "SELECT block_number, log_index, block_timestamp, CAST(delta AS VARCHAR) AS delta, \
         reward, CAST(shares AS VARCHAR) AS shares, CAST(thawing AS VARCHAR) AS thawing, cut \
         FROM delegation_ordered WHERE indexer = '{INDEXER}' ORDER BY block_number, log_index"
    );
    let rows = analytics::query(corpus, &sql).expect("read recorded delegation events");
    let events: Vec<Event> = rows
        .iter()
        .map(|row| Event {
            block: row["block_number"].as_u64().expect("block number"),
            log: row["log_index"].as_u64().expect("log index"),
            timestamp: row["block_timestamp"].as_u64().expect("timestamp"),
            delta: decimal(row, "delta", true),
            reward: decimal(row, "reward", false),
            shares: decimal(row, "shares", true),
            thawing: decimal(row, "thawing", true),
            cut: decimal(row, "cut", false),
        })
        .collect();
    assert_eq!(events.len(), 181, "the pinned corpus changed");
    assert_eq!(events.first().unwrap().block, 155_504_998);
    assert_eq!(events.last().unwrap().block, 394_174_357);
    assert_eq!(events.iter().filter(|e| e.reward != "0").count(), 2);
    assert!(events.iter().any(|e| e.reward != "0" && e.cut != "1000000"));
    assert!(events
        .windows(2)
        .all(|w| (w[0].block, w[0].log) < (w[1].block, w[1].log)));
    events
}

fn values(events: &[Event]) -> String {
    events
        .iter()
        .enumerate()
        .map(|(i, e)| {
            format!(
                "('{INDEXER}', {}, {}, {}, CAST('{}' AS BIGNUM), '{}', \
                 CAST('{}' AS BIGNUM), CAST('{}' AS BIGNUM), {}, '{}')",
                e.block,
                e.log,
                e.timestamp,
                e.delta,
                e.reward,
                e.shares,
                e.thawing,
                i + 1,
                e.cut
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn state(row: &Value) -> [String; 5] {
    [
        decimal(row, "delegated", true),
        decimal(row, "shares", true),
        decimal(row, "thawing", true),
        decimal(row, "delegator_reward", true),
        decimal(row, "indexer_reward", true),
    ]
}

fn reference(scratch: &Path, events: &[Event]) -> Vec<[String; 5]> {
    let source = include_str!("fixtures/network-nest/views/41-delegation.sql");
    let body = source
        .split_once("CREATE VIEW delegation_ledger AS")
        .unwrap()
        .1
        .split_once("CREATE VIEW indexer_delegation AS")
        .unwrap()
        .0
        .trim()
        .trim_end_matches(';');
    let body = body.replacen(
        "WITH RECURSIVE ordered AS",
        &format!(
            "WITH RECURSIVE delegation_ordered(indexer, block_number, log_index, \
             block_timestamp, delta, reward, shares, thawing, seq, cut) AS \
             (VALUES {}), ordered AS",
            values(events)
        ),
        1,
    );
    assert!(body.contains("delegation_ordered(indexer"));
    let sql = format!(
        "SELECT CAST(delegated AS VARCHAR) AS delegated, \
         CAST(shares AS VARCHAR) AS shares, CAST(thawing AS VARCHAR) AS thawing, \
         CAST(delegator_reward AS VARCHAR) AS delegator_reward, \
         CAST(indexer_reward AS VARCHAR) AS indexer_reward \
         FROM ({body}) AS original ORDER BY block_number, log_index"
    );
    analytics::query(scratch, &sql)
        .expect("original recursive ledger on the recorded events")
        .iter()
        .map(state)
        .collect()
}

fn step(scratch: &Path, events: &[Event], carry: &[String; 3]) -> Vec<[String; 5]> {
    assert!(!events.is_empty());
    let rows = events
        .iter()
        .enumerate()
        .map(|(i, e)| {
            format!(
                "({}, CAST('{}' AS BIGNUM), '{}', CAST('{}' AS BIGNUM), \
                 CAST('{}' AS BIGNUM), '{}')",
                i + 1,
                e.delta,
                e.reward,
                e.shares,
                e.thawing,
                e.cut
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let reward = "CASE WHEN e.reward <> '0' AND w.delegated <> CAST(0 AS BIGNUM) \
                  THEN CAST(e.reward AS BIGNUM) - \
                       CAST(nuthatch_mul_div(e.reward, e.cut, '1000000') AS BIGNUM) \
                  ELSE CAST(0 AS BIGNUM) END";
    let sql = format!(
        "WITH RECURSIVE events(rn, delta, reward, shares, thawing, cut) AS (VALUES {rows}), \
         walk(rn, delegated, shares, thawing, delegator_reward, indexer_reward) AS ( \
           SELECT 0, CAST('{}' AS BIGNUM), CAST('{}' AS BIGNUM), \
                  CAST('{}' AS BIGNUM), CAST(0 AS BIGNUM), CAST(0 AS BIGNUM) \
           UNION ALL \
           SELECT e.rn, w.delegated + e.delta + {reward}, \
                  w.shares + e.shares, w.thawing + e.thawing, {reward}, \
                  CAST(e.reward AS BIGNUM) - {reward} \
           FROM walk w JOIN events e ON e.rn = w.rn + 1 \
         ) \
         SELECT CAST(delegated AS VARCHAR) AS delegated, \
                CAST(shares AS VARCHAR) AS shares, CAST(thawing AS VARCHAR) AS thawing, \
                CAST(delegator_reward AS VARCHAR) AS delegator_reward, \
                CAST(indexer_reward AS VARCHAR) AS indexer_reward \
         FROM walk WHERE rn > 0 ORDER BY rn",
        carry[0], carry[1], carry[2]
    );
    analytics::query(scratch, &sql)
        .expect("windowed delegation step with Nuthatch's exact scalar")
        .iter()
        .map(state)
        .collect()
}

#[test]
fn windowed_reward_arithmetic_matches_the_original_with_large_values() {
    let amount = "123456789012345678901234567890123456789";
    let events = vec![
        Event {
            block: 1,
            log: 0,
            timestamp: 1,
            delta: amount.into(),
            reward: "0".into(),
            shares: amount.into(),
            thawing: "0".into(),
            cut: "333333".into(),
        },
        Event {
            block: 2,
            log: 0,
            timestamp: 2,
            delta: "0".into(),
            reward: "999999999999999999999999999999999999999".into(),
            shares: "0".into(),
            thawing: "0".into(),
            cut: "333333".into(),
        },
        Event {
            block: 3,
            log: 0,
            timestamp: 3,
            delta: "0".into(),
            reward: "7".into(),
            shares: "0".into(),
            thawing: "0".into(),
            cut: "1000000".into(),
        },
    ];
    let scratch = tempfile::tempdir().unwrap();
    let expected = reference(scratch.path(), &events);
    let first = step(
        scratch.path(),
        &events[..1],
        &["0".into(), "0".into(), "0".into()],
    );
    let carry = [
        first[0][0].clone(),
        first[0][1].clone(),
        first[0][2].clone(),
    ];
    let second = step(scratch.path(), &events[1..], &carry);
    assert_eq!([first, second].concat(), expected);
    assert_ne!(expected[1][3], "0");
}

#[test]
#[ignore = "operator check: needs NETWORK_COLD_REPLAY_DIR with the independently ingested Arbitrum corpus"]
fn recorded_delegation_is_partition_invariant_with_exact_reward_arithmetic() {
    let corpus = std::env::var("NETWORK_COLD_REPLAY_DIR").expect("NETWORK_COLD_REPLAY_DIR");
    let events = events(Path::new(&corpus));
    let scratch = tempfile::tempdir().unwrap();
    let expected = reference(scratch.path(), &events);
    assert_eq!(expected.len(), events.len());
    assert!(expected.iter().any(|row| row[3] != "0"));

    for ends in [[31, 60, 100, 181], [1, 13, 89, 181], [70, 90, 150, 181]] {
        let mut carry = ["0".to_owned(), "0".to_owned(), "0".to_owned()];
        let mut start = 0;
        for end in ends {
            let got = step(scratch.path(), &events[start..end], &carry);
            assert_eq!(got, expected[start..end], "window [{start}, {end})");
            let last = got.last().unwrap();
            carry = [last[0].clone(), last[1].clone(), last[2].clone()];
            start = end;
        }
    }

    let carry = [
        "0".to_owned(),
        expected[30][1].clone(),
        expected[30][2].clone(),
    ];
    let mutated = step(scratch.path(), &events[31..60], &carry);
    assert_ne!(
        mutated,
        expected[31..60],
        "dropping delegated carry must be detected"
    );
}
