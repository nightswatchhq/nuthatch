// Prints the allocations nest's query set: every statement Lodestar's dashboard (through kittiwake's
// read API) and kittiwake's own jobs send to that nest's `/sql`. Built by collect-queries.sh, which
// compiles this against a copy of kittiwake's crates/read/src/sql.rs; nothing here runs in nuthatch.
//
// Statements built in sql.rs are called with representative literals and so refresh themselves.
// Statements written inline at a call site are copied here by hand and say so (`inline`): on a
// refresh, re-read those call sites. Literals are pinned to 2026-10-03 so the set is deterministic.

#![allow(dead_code)]
mod sql;

const NOW: i64 = 1_790_985_600; // 2026-10-03T00:00:00Z
const DAY: i64 = 86_400;
// The largest indexer on every axis (allocations, delegators, escrow receipts); kittiwake's own
// tests use it as the worst case.
const IX: &str = "0xf92f430dd8567b0d466358c79594ab58d919a6d4";
const DELEGATOR: &str = "0x15f72b52cd79928b66fe9887acd20057203a97b1";
const CURATOR: &str = "0xec9a7fb6cbc2e41926127929c2dce6e9c5d33bec";
const DEP: &str = "0x5e8b5a9244a5c3bfe1396fec44031cbb975418d8ac94587e85b6bc0dfafb076b";
const SUBGRAPH_SERVICE: &str = "0xb2bb92d0de618878e438b55d5846cfecd9301105";
const BEFORE_BLOCK: i64 = 511_198_017; // the copy's sealed_through on 2026-10-03
// The 24 deployments with the most query fees in the 30 days to 2026-10-03, standing in for the
// fee query's answer that subgraph-fees-30d passes on.
const FEE_DEPLOYMENTS: &[&str] = &[
    "0xc915fae50b8dc6976b454869031c52761538b1354ff5587a207dbdbbe66e73fa",
    "0x75548d70e3585bec8a5766a0debbc78b8cb94d50cc51be2f29f8d7d5e1a12cfd",
    "0xa97329b8da365c46407488297ba55fd43421a59702bd8aac0dabccd41947a400",
    "0xc20fc0a2025756999f2494e4b0d795e91aded151e718348c5823d0968e50c481",
    "0xba36802122f0bd11ee974592fc543efc499cf2947200488659e0379a66aed656",
    "0x4d7cff900a9ea5d7882817e38736382243a409e68b627d01442922d19b39f54a",
    "0x2434cca99690681ea2e52334de39179ca1ea8f4e8927657adb605be3f1fe7e15",
    "0xab635bff4497cb9acd54a80651626590caac9dbeaa149e839c2aa648e866d4c8",
    "0x9ae0e05024684bde7cf967df54edbfc79942f66dc048f9ea9975fbfec968ed49",
    "0xce57e4bc7b885a6255edd3e9d1617bb8819559f3903b84c18bb5db31afe17d06",
    "0x2976b26882acdce242f2c40d8096e16f0ac0b1bab842ca9c1431a0c91ba68544",
    "0x7e5eb975046732c597705650708feda3b7c0db695b56438590a597dc3545b33c",
    "0x9497798951d9dffe9d7e5b41a045d6a49465798d168a6c631c726ecc21cd226e",
    "0xbf2ef2fc80928849fd6aac3f99d56e76a6e2e65356577eeabe059c6bf873058a",
    "0xdb0c77a3184bf017abafb7606794d77ff53d9a59a0bc1dc23f1a762bd1e631ff",
    "0xbd0432957274a4e52f59197e03778f7e291cc7e6b42dfca6ce61f68ee44e6c06",
    "0xc425fa8659016a13a58bc4f74983fbd3c50ba8ad900c6077d88261114d43118a",
    "0xb1bb6f22826a21a64a306c5c6e2a4108cb39ec9cccf82a88884ee3d4a9f887a7",
    "0x88e7a9e5882046a6f3cda49b8a08f31e7e0939cd34934f27709822fd77d3e5b7",
    "0xff15810951d58793e9b3160addf310058af42ece5b5991adf684e126de5335f4",
    "0x7868174e4707eba68070e2154e7eb6109ddaeda9c85b6219a8bec06cc6269f67",
    "0x2390f64b1c508990f72b42e64d517e2bf79b608c6c86c1a8e493b1c300cfe2f4",
    "0x73cc1d5ace50e5e18679083919ace9728535ac5fa1515a8ebeda10389b6bafa5",
    "0x90dbb1e44b12f0acbdbb494b7f84928b4f25c0c27904e2052ac38b6024fad315",
];

// kittiwake_rewards::pnl::window_start
fn window_start(now: i64, days: i64) -> i64 {
    (now.div_euclid(DAY) - (days - 1)) * DAY
}

// kittiwake_read::routes::stake_history_cutoffs
fn stake_history_cutoffs(end: i64, days: i64) -> Vec<i64> {
    let start = end - days * DAY;
    (0..)
        .map(|i| start + i * DAY * 7)
        .take_while(|t| *t <= end)
        .collect()
}

struct Set {
    rows: Vec<(String, String, String, String)>,
}

impl Set {
    fn add(&mut self, id: &str, consumer: &str, site: &str, sql: impl Into<Option<String>>) {
        let sql = sql
            .into()
            .unwrap_or_else(|| panic!("{id}: the builder refused its representative input"));
        let sql = sql.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(!sql.contains('\t'), "{id}: a tab in the statement");
        // One statement is asked once: the nest memoises by text, so a duplicate would time a cache.
        if let Some(row) = self.rows.iter_mut().find(|r| r.3 == sql) {
            if !row.1.split(',').any(|c| c == consumer) {
                row.1 = format!("{},{consumer}", row.1);
            }
            row.2 = format!("{}; {site}", row.2);
            return;
        }
        assert!(self.rows.iter().all(|r| r.0 != id), "{id}: duplicate id");
        self.rows
            .push((id.into(), consumer.into(), site.into(), sql));
    }
}

fn s(v: &str) -> Option<String> {
    Some(v.to_string())
}

fn main() {
    let mut q = Set { rows: Vec::new() };
    let l = "lodestar";
    let k = "kittiwake";
    let routes = "kittiwake crates/read/src/routes.rs";
    let since30 = NOW - 30 * DAY;
    let fee_ids: Vec<String> = FEE_DEPLOYMENTS.iter().map(|s| s.to_string()).collect();

    // The directory and the network. Parameters are the edge handlers' defaults and the warmer's.
    q.add("indexers", l, &format!("{routes} indexers (/api/indexers; warmer)"), sql::indexers_sql(100, 0, "stakedTokens", true));
    q.add("curators", l, &format!("{routes} curators (/api/curators)"), sql::curators_sql(100, 0));
    q.add("epochs", l, &format!("{routes} epochs, token_metrics (/api/epochs, /api/token-metrics; warmer)"), sql::epochs_sql(20));
    q.add("network", l, &format!("{routes} network_stats, grt_flow (/api/network-stats, /api/grt-flow)"), s(sql::network_sql()));
    q.add("network_params", l, &format!("{routes} network_stats, grt_flow"), s(sql::network_params_sql()));

    // One indexer: the page the warmer keeps hot for every directory indexer.
    q.add("indexer.detail", l, &format!("{routes} indexer, indexer_status, apr_provenance"), sql::indexer_detail_sql(IX));
    q.add("indexer.operators", l, &format!("{routes} indexer"), sql::indexer_operators_sql(IX));
    q.add("indexer.delegators", l, &format!("{routes} indexer"), sql::indexer_delegators_sql(IX, 100));
    q.add("indexer.active_allocations", l, &format!("{routes} indexer, indexer_status, apr_provenance"), sql::indexer_active_allocations_sql(IX));
    q.add("indexer.closed_allocations", l, &format!("{routes} indexer"), sql::indexer_closed_allocations_sql(IX, 500));
    q.add("delegation_ratio", l, &format!("{routes} indexer, apr_provenance, delegator_portfolio"), s(sql::DELEGATION_RATIO_SQL));
    q.add("indexer.delegators_page", l, &format!("{routes} indexer_delegators (/api/indexer/[a]/delegators)"), sql::indexer_delegators_page_sql(IX, 25, 0, "", true));
    q.add("indexer.stake_history", l, &format!("{routes} indexer_stake_history (182 days; warmer)"), sql::indexer_stake_history_sql(IX, &stake_history_cutoffs(NOW, 182)));
    q.add("indexer.daily", l, &format!("{routes} indexer_trends (90 days; warmer) -> lodestar_indexer_daily"), sql::indexer_daily_sql(IX, window_start(NOW, 90)));
    q.add("indexer.revenue_daily", l, &format!("{routes} indexer_revenue (30 days; warmer)"), sql::indexer_revenue_daily_sql(IX, window_start(NOW, 30)));
    q.add("indexer.revenue_by_deployment", l, &format!("{routes} indexer_revenue by_deployment (/api/indexer/[a]/pnl; warmer)"), sql::indexer_revenue_by_deployment_sql(IX, window_start(NOW, 30)));
    q.add("indexer.allocation_shares", l, "kittiwake crates/read/src/qos.rs allocation_shares (warmer)", sql::indexer_allocation_shares_sql(IX));

    // Portfolios.
    q.add("delegator", l, &format!("{routes} delegator_portfolio (/api/portfolio)"), sql::delegator_sql(DELEGATOR));
    q.add("delegator.stakes", l, &format!("{routes} delegator_portfolio"), sql::delegator_stakes_sql(DELEGATOR, 200, false));
    q.add("delegator.active_stakes", l, &format!("{routes} rewards_history (/api/rewards-history)"), sql::delegator_stakes_sql(DELEGATOR, 100, true));
    q.add("curator", l, &format!("{routes} curator_portfolio"), sql::curator_sql(CURATOR));
    q.add("curator.signals", l, &format!("{routes} curator_portfolio"), sql::curator_signals_sql(CURATOR, 200));

    // Provisions, payments, proofs.
    q.add("provisions.by_indexer", l, &format!("{routes} provisions_by_indexer, provision_detail (warmer)"), sql::provisions_by_indexer_sql(IX));
    q.add("provisions.max_thawing", l, &format!("{routes} provisions_by_indexer"), s(sql::MAX_THAWING_PERIOD_SQL));
    q.add("provisions.service_totals", l, &format!("{routes} provisions_by_indexer"), sql::data_service_totals_sql(&[SUBGRAPH_SERVICE.to_string()]));
    q.add("provisions.thaw_requests", l, &format!("{routes} provision_detail"), sql::provision_thaw_requests_sql(IX));
    q.add("provisions.horizon_activity", l, &format!("{routes} provision_detail"), sql::indexer_horizon_activity_sql(IX, 100));
    q.add("provisions.by_service", l, &format!("{routes} provisions_by_service"), sql::provisions_by_service_sql(SUBGRAPH_SERVICE, 100, 0));
    q.add("payments.accounts", l, &format!("{routes} payments (/api/payments)"), sql::escrow_accounts_sql(None, 100));
    q.add("payments.transactions", l, &format!("{routes} payments"), sql::escrow_transactions_sql(None, 50));
    q.add("payments.tally", l, &format!("{routes} payments"), sql::tally_collected_sql(None, 50));
    q.add("payments.by_payer", l, &format!("{routes} payments"), sql::tally_by_payer_sql(None));
    q.add("payments.receiver_accounts", l, &format!("{routes} payments ?receiver="), sql::escrow_accounts_sql(Some(IX), 100));
    q.add("payments.receiver_transactions", l, &format!("{routes} payments ?receiver="), sql::escrow_transactions_sql(Some(IX), 100));
    q.add("payments.receiver_tally", l, &format!("{routes} payments ?receiver="), sql::tally_collected_sql(Some(IX), 50));
    q.add("payments.receiver_by_payer", l, &format!("{routes} payments ?receiver="), sql::tally_by_payer_sql(Some(IX)));
    q.add("poi.overview", l, &format!("{routes} poi (/api/poi)"), sql::poi_allocations_sql(None, 1000));
    q.add("poi.deployment", l, &format!("{routes} poi ?deployment="), sql::poi_allocations_sql(Some(DEP), 1000));

    // Delegation activity.
    q.add("delegation_events", l, &format!("{routes} delegation_events (/api/delegation-events, 30 days)"), sql::delegation_events_sql(None, 100, since30));
    q.add("delegation_flows", l, &format!("{routes} delegation_flows (live half, 30 days)"), s(&sql::daily_flow_sql(since30.max(sql::LEGACY_HISTORY_END_EXCLUSIVE), i64::MAX, false)));

    // Subgraphs and deployments.
    q.add("deployments.list", l, &format!("{routes} subgraph_deployments"), s(&sql::deployments_list_sql(100, 0, "signalledTokens", true)));
    q.add("deployments.one", l, &format!("{routes} subgraph_deployment (/api/subgraph-deployment/[hash])"), sql::deployments_by_id_sql(&[DEP.to_string()]));
    q.add("deployments.fees_30d", l, &format!("{routes} subgraph_fees_30d"), s(&sql::deployment_fees_since_sql(since30, 200)));
    q.add("deployments.by_fee_ids", l, &format!("{routes} subgraph_fees_30d (second fold)"), sql::deployments_by_id_sql(&fee_ids));
    q.add("deployments.directory", l, &format!("{routes} subgraph_directory"), s(&sql::directory_sql()));
    q.add("deployments.fees_window", l, &format!("{routes} subgraph_directory"), s(&sql::deployment_fees_window_sql(since30)));
    q.add("deployment.signals", l, &format!("{routes} subgraph_curation"), sql::deployment_signals_sql(DEP, 100));
    q.add("deployment.signal_transactions", l, &format!("{routes} subgraph_history"), sql::deployment_signal_transactions_sql(DEP, 1000));
    q.add("deployment.allocation_history", l, &format!("{routes} subgraph_history"), sql::deployment_allocations_history_sql(DEP, 1000));
    q.add("deployment.status", l, &format!("{routes} deployment_status (/api/indexing-status/[hash])"), sql::deployment_sql(DEP));
    q.add("deployment.allocations", l, &format!("{routes} deployment_status"), sql::allocations_by_deployment_sql(DEP, 100));
    q.add("deployments.signal_stake", l, &format!("{routes} subgraph_versions_fetcher"), sql::deployments_signal_stake_sql(&fee_ids[..3]));
    q.add("deployment.disassembly_signal", l, "kittiwake crates/read/src/disassembly.rs deployment_signals_sql (inline)", s(&format!(
        "SELECT CAST(deployment_signalled_tokens AS VARCHAR) AS deployment_signalled_tokens, \
         CAST(deployment_query_fees_amount AS VARCHAR) AS deployment_query_fees_amount \
         FROM lodestar_curator_signals s WHERE LOWER(s.subgraph_deployment) = '{DEP}' \
         AND s.signal > 0 ORDER BY s.signalled_tokens DESC, s.curator LIMIT 100"
    )));
    q.add("search.figures", l, "kittiwake crates/read/src/search.rs load (inline)", s(
        "SELECT id, CAST(signalled_tokens AS VARCHAR) AS signalled_tokens, \
         CAST(staked_tokens AS VARCHAR) AS staked_tokens FROM lodestar_deployments \
         WHERE signalled_tokens > 0 OR staked_tokens > 0"));
    q.add("search.deployment_ids", l, "kittiwake crates/read/src/search.rs warm (inline)", s(
        "SELECT id FROM lodestar_deployments WHERE signalled_tokens > 0 OR staked_tokens > 0"));

    // The public SQL page: the staking dataset's sample and its named queries (inline in sqlplay).
    let sqlplay = "kittiwake crates/sqlplay/src";
    q.add("sqlplay.sample", l, &format!("{sqlplay}/lib.rs DATASETS staking sample (/sql page)"), s(
        "SELECT block_number, serviceProvider, delegator, tokens FROM staking__tokens_delegated \
         ORDER BY block_number DESC LIMIT 20"));
    q.add("sqlplay.delegations_to_indexer", l, &format!("{sqlplay}/named.rs delegations_to_indexer (inline)"), s(&format!(
        "SELECT block_number, block_timestamp, delegator, tokens, shares, tx_hash FROM staking__tokens_delegated \
         WHERE serviceProvider = '{IX}' AND block_number <= {BEFORE_BLOCK} ORDER BY block_number DESC LIMIT 500")));
    q.add("sqlplay.delegator_activity", l, &format!("{sqlplay}/named.rs delegator_activity (inline)"), s(&format!(
        "SELECT block_number, block_timestamp, 'delegated' AS action, serviceProvider, tokens FROM staking__tokens_delegated \
         WHERE delegator = '{DELEGATOR}' AND block_number <= {BEFORE_BLOCK} UNION ALL \
         SELECT block_number, block_timestamp, 'undelegated' AS action, serviceProvider, tokens FROM staking__tokens_undelegated \
         WHERE delegator = '{DELEGATOR}' AND block_number <= {BEFORE_BLOCK} ORDER BY block_number DESC LIMIT 500")));
    q.add("sqlplay.net_delegation_to_indexer", l, &format!("{sqlplay}/named.rs net_delegation_to_indexer (inline)"), s(&format!(
        "SELECT CAST(SUM(CAST(tokens AS HUGEINT)) AS VARCHAR) AS delegated, CAST(COUNT(*) AS VARCHAR) AS events \
         FROM staking__tokens_delegated WHERE serviceProvider = '{IX}' AND block_number <= {BEFORE_BLOCK}")));

    // kittiwake's own jobs: the directory refresh, the ingest crons, the live feed, RAVs and QoS.
    let ingest = "kittiwake crates/ingest/src";
    q.add("refresh.network", k, &format!("{ingest}/refresh.rs refresh; lib.rs network totals"), s(sql::network_sql()));
    q.add("refresh.params", k, &format!("{ingest}/refresh.rs refresh"), s(sql::network_params_sql()));
    q.add("refresh.indexers_all", k, &format!("{ingest}/refresh.rs refresh; live.rs"), s(sql::INDEXERS_ALL_SQL));
    q.add("refresh.active_allocations_all", k, &format!("{ingest}/refresh.rs refresh"), s(sql::ACTIVE_ALLOCATIONS_ALL_SQL));
    q.add("refresh.delegation_flow_7d", k, &format!("{ingest}/refresh.rs refresh"), s(&sql::delegation_events_since_sql(NOW - 7 * DAY)));
    q.add("refresh.closed_allocations_90d", k, &format!("{ingest}/refresh.rs refresh"), s(&sql::closed_allocations_since_sql(NOW - 90 * DAY)));
    q.add("refresh.data_service_counts", k, &format!("{ingest}/refresh.rs refresh"), s(sql::DATA_SERVICE_COUNTS_SQL));
    q.add("refresh.exchange_rates_30d", k, &format!("{ingest}/refresh.rs refresh"), s(&sql::exchange_rates_as_of_sql(NOW - 30 * DAY)));
    q.add("refresh.exchange_rates_90d", k, &format!("{ingest}/refresh.rs refresh"), s(&sql::exchange_rates_as_of_sql(NOW - 90 * DAY)));
    q.add("ingest.epochs", k, &format!("{ingest}/lib.rs epochs (inline, cursor 0)"), s(
        "SELECT id, start_block, end_block, CAST(signalled_tokens AS VARCHAR) AS signalled_tokens, \
         CAST(stake_deposited AS VARCHAR) AS stake_deposited, CAST(total_rewards AS VARCHAR) AS total_rewards, \
         CAST(total_indexer_rewards AS VARCHAR) AS total_indexer_rewards, \
         CAST(total_delegator_rewards AS VARCHAR) AS total_delegator_rewards, \
         CAST(query_fees_collected AS VARCHAR) AS query_fees_collected, \
         CAST(curator_query_fees AS VARCHAR) AS curator_query_fees, \
         CAST(taxed_query_fees AS VARCHAR) AS taxed_query_fees \
         FROM lodestar_epochs WHERE id > 0 ORDER BY id ASC LIMIT 500"));
    q.add("ingest.delegation_events", k, &format!("{ingest}/lib.rs delegation_events (inline, cursor a day back)"), s(&format!(
        "SELECT id, event_type, indexer, delegator, CAST(tokens AS VARCHAR) AS tokens, timestamp \
         FROM lodestar_delegations WHERE timestamp > {} ORDER BY timestamp ASC, id ASC LIMIT 1000", NOW - DAY)));
    q.add("ingest.allocations", k, &format!("{ingest}/lib.rs allocations (inline, first page)"), s(
        "SELECT id, indexer, subgraph_deployment, CAST(signalled_tokens AS VARCHAR) AS signalled_tokens, \
         CAST(allocated_tokens AS VARCHAR) AS allocated_tokens, created_at_epoch, closed_at_epoch, \
         created_at, closed_at, poi, CAST(indexing_rewards AS VARCHAR) AS indexing_rewards, \
         CAST(query_fees_collected AS VARCHAR) AS query_fees_collected, status \
         FROM lodestar_allocations ORDER BY id ASC LIMIT 2000"));
    q.add("ingest.disputes", k, &format!("{ingest}/lib.rs disputes (inline)"), s(
        "SELECT d.id, d.kind, d.indexer, d.fisherman, d.allocation_id, \
         a.subgraph_deployment, d.status, d.created_at, d.resolved_at \
         FROM lodestar_disputes d LEFT JOIN lodestar_allocations a ON a.id = d.allocation_id \
         ORDER BY d.created_at"));
    q.add("live.delegation_events", k, &format!("{ingest}/live.rs"), sql::delegation_events_sql(None, 20, 0));
    q.add("live.newest_provisions", k, &format!("{ingest}/live.rs"), s(&sql::newest_provisions_sql(10)));
    q.add("rav.collections", k, &format!("{ingest}/rav.rs build_sql (inline, cursor a day back, first page)"), s(&format!(
        "WITH fees AS (\
         SELECT tx_hash, LOWER(payer) AS payer, LOWER(\"serviceProvider\") AS receiver, \
         CAST(\"tokensCollected\" AS VARCHAR) AS tokens, \"allocationId\" AS allocation_id, \
         CAST(\"tokensCollected\" AS VARCHAR) AS fee_tokens, \
         ROW_NUMBER() OVER (PARTITION BY tx_hash, LOWER(payer), LOWER(\"serviceProvider\"), \
         CAST(\"tokensCollected\" AS VARCHAR) ORDER BY log_index) AS rn \
         FROM subgraph_service__query_fees_collected\
         ), esc AS (\
         SELECT tx_hash, log_index, payer, receiver, tokens, block_timestamp, \
         ROW_NUMBER() OVER (PARTITION BY tx_hash, LOWER(payer), LOWER(receiver), \
         CAST(tokens AS VARCHAR) ORDER BY log_index) AS rn \
         FROM escrow__escrow_collected WHERE LOWER(collector) = '0x8f69f5c07477ac46fbc491b1e6d91e2bb0111a9e'\
         ) \
         SELECT c.tx_hash, c.log_index, c.payer, c.receiver, CAST(c.tokens AS VARCHAR) AS tokens, \
         c.block_timestamp, f.allocation_id, f.fee_tokens \
         FROM escrow__escrow_collected c \
         LEFT JOIN esc e ON e.tx_hash = c.tx_hash AND e.log_index = c.log_index \
         LEFT JOIN fees f ON f.tx_hash = e.tx_hash AND f.payer = LOWER(e.payer) \
         AND f.receiver = LOWER(e.receiver) AND f.tokens = CAST(e.tokens AS VARCHAR) AND f.rn = e.rn \
         WHERE c.block_timestamp >= {} \
         ORDER BY c.block_timestamp, c.tx_hash, c.log_index LIMIT 10000", NOW - DAY)));
    q.add("qos.allocation_shares_all", k, &format!("{ingest}/qos_score.rs"), s(sql::ALLOCATION_SHARES_ALL_SQL));

    // Statements release-gate.sh holds to their row count, because their answer moves without the
    // binary changing (#1772). Found by gating production twice on one copy a minute apart; re-run
    // that after a refresh. Both page with ORDER BY ... LIMIT on a key with ties, so which tied
    // rows make the page is the engine's choice; a tiebreaker in kittiwake would make them exact.
    let volatile: &[(&str, &str)] = &[
        ("payments.accounts", "ORDER BY SUM(mv.d) DESC LIMIT 100: equal balances tie across the limit"),
        ("delegation_events", "ORDER BY ts DESC LIMIT 100: events in one block share a timestamp"),
    ];
    for (id, why) in volatile {
        assert!(q.rows.iter().any(|r| r.0 == *id), "{id}: tagged volatile but not in the set");
        println!("# volatile: {id} {why}");
    }
    for (id, consumer, site, sql) in &q.rows {
        println!("{id}\t{consumer}\t{site}\t{sql}");
    }
}
