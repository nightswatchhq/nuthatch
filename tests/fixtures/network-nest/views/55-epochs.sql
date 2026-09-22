-- Additions are assigned to the mapping's observed epoch, not event L2 height.
CREATE VIEW epoch_financial_movement AS
SELECT block_number, log_index,
       CAST(tokens AS BIGNUM) - CAST("curationTax" AS BIGNUM) AS "signalledTokens", CAST(0 AS BIGNUM) AS "stakeDeposited", CAST(0 AS BIGNUM) AS "queryFeeRebates", CAST(0 AS BIGNUM) AS "queryFeesCollected", CAST(0 AS BIGNUM) AS "totalQueryFees", CAST(0 AS BIGNUM) AS "taxedQueryFees", CAST(0 AS BIGNUM) AS "curatorQueryFees", CAST(0 AS BIGNUM) AS "totalRewards", CAST(0 AS BIGNUM) AS "totalIndexerRewards", CAST(0 AS BIGNUM) AS "totalDelegatorRewards"
FROM curation__signalled
UNION ALL
SELECT block_number, log_index,
       CAST(0 AS BIGNUM) AS "signalledTokens", CAST(tokens AS BIGNUM) AS "stakeDeposited", CAST(0 AS BIGNUM) AS "queryFeeRebates", CAST(0 AS BIGNUM) AS "queryFeesCollected", CAST(0 AS BIGNUM) AS "totalQueryFees", CAST(0 AS BIGNUM) AS "taxedQueryFees", CAST(0 AS BIGNUM) AS "curatorQueryFees", CAST(0 AS BIGNUM) AS "totalRewards", CAST(0 AS BIGNUM) AS "totalIndexerRewards", CAST(0 AS BIGNUM) AS "totalDelegatorRewards"
FROM staking_legacy__stake_deposited
UNION ALL
SELECT block_number, log_index,
       CAST(0 AS BIGNUM) AS "signalledTokens", CAST(tokens AS BIGNUM) AS "stakeDeposited", CAST(0 AS BIGNUM) AS "queryFeeRebates", CAST(0 AS BIGNUM) AS "queryFeesCollected", CAST(0 AS BIGNUM) AS "totalQueryFees", CAST(0 AS BIGNUM) AS "taxedQueryFees", CAST(0 AS BIGNUM) AS "curatorQueryFees", CAST(0 AS BIGNUM) AS "totalRewards", CAST(0 AS BIGNUM) AS "totalIndexerRewards", CAST(0 AS BIGNUM) AS "totalDelegatorRewards"
FROM horizon_staking__horizon_stake_deposited
UNION ALL
SELECT block_number, log_index,
       CAST(0 AS BIGNUM) AS "signalledTokens", CAST(0 AS BIGNUM) AS "stakeDeposited", CAST(0 AS BIGNUM) AS "queryFeeRebates", CAST("rebateFees" AS BIGNUM) AS "queryFeesCollected", CAST(tokens AS BIGNUM) AS "totalQueryFees", CAST(tokens AS BIGNUM) - CAST("rebateFees" AS BIGNUM) - CAST("curationFees" AS BIGNUM) AS "taxedQueryFees", CAST("curationFees" AS BIGNUM) AS "curatorQueryFees", CAST(0 AS BIGNUM) AS "totalRewards", CAST(0 AS BIGNUM) AS "totalIndexerRewards", CAST(0 AS BIGNUM) AS "totalDelegatorRewards"
FROM staking_legacy__allocation_collected
UNION ALL
SELECT block_number, log_index,
       CAST(0 AS BIGNUM) AS "signalledTokens", CAST(0 AS BIGNUM) AS "stakeDeposited", CAST(tokens AS BIGNUM) AS "queryFeeRebates", CAST(0 AS BIGNUM) AS "queryFeesCollected", CAST(0 AS BIGNUM) AS "totalQueryFees", CAST(0 AS BIGNUM) AS "taxedQueryFees", CAST(0 AS BIGNUM) AS "curatorQueryFees", CAST(0 AS BIGNUM) AS "totalRewards", CAST(0 AS BIGNUM) AS "totalIndexerRewards", CAST(0 AS BIGNUM) AS "totalDelegatorRewards"
FROM staking_legacy__rebate_claimed
UNION ALL
SELECT block_number, log_index,
       CAST(0 AS BIGNUM) AS "signalledTokens", CAST(0 AS BIGNUM) AS "stakeDeposited", CAST("queryRebates" AS BIGNUM) AS "queryFeeRebates", CAST("queryFees" AS BIGNUM) AS "queryFeesCollected", CAST(tokens AS BIGNUM) AS "totalQueryFees", CAST("protocolTax" AS BIGNUM) AS "taxedQueryFees", CAST("curationFees" AS BIGNUM) AS "curatorQueryFees", CAST(0 AS BIGNUM) AS "totalRewards", CAST(0 AS BIGNUM) AS "totalIndexerRewards", CAST(0 AS BIGNUM) AS "totalDelegatorRewards"
FROM staking_legacy__rebate_collected
UNION ALL
SELECT block_number, log_index,
       CAST(0 AS BIGNUM) AS "signalledTokens", CAST(0 AS BIGNUM) AS "stakeDeposited", CAST(0 AS BIGNUM) AS "queryFeeRebates", CAST(0 AS BIGNUM) AS "queryFeesCollected", CAST(0 AS BIGNUM) AS "totalQueryFees", CAST("tokensProtocol" AS BIGNUM) AS "taxedQueryFees", CAST(0 AS BIGNUM) AS "curatorQueryFees", CAST(0 AS BIGNUM) AS "totalRewards", CAST(0 AS BIGNUM) AS "totalIndexerRewards", CAST(0 AS BIGNUM) AS "totalDelegatorRewards"
FROM graph_payments__graph_payment_collected
UNION ALL
SELECT block_number, log_index,
       CAST(0 AS BIGNUM) AS "signalledTokens", CAST(0 AS BIGNUM) AS "stakeDeposited", CAST(0 AS BIGNUM) AS "queryFeeRebates", CAST(0 AS BIGNUM) AS "queryFeesCollected", CAST(0 AS BIGNUM) AS "totalQueryFees", CAST(0 AS BIGNUM) AS "taxedQueryFees", CAST(0 AS BIGNUM) AS "curatorQueryFees", CAST("tokensRewards" AS BIGNUM) AS "totalRewards", CAST("tokensIndexerRewards" AS BIGNUM) AS "totalIndexerRewards", CAST("tokensDelegationRewards" AS BIGNUM) AS "totalDelegatorRewards"
FROM subgraph_service__indexing_rewards_collected
UNION ALL
SELECT block_number, log_index,
       CAST(0 AS BIGNUM) AS "signalledTokens", CAST(0 AS BIGNUM) AS "stakeDeposited", CAST(0 AS BIGNUM) AS "queryFeeRebates", CAST(0 AS BIGNUM) AS "queryFeesCollected", CAST(0 AS BIGNUM) AS "totalQueryFees", CAST(0 AS BIGNUM) AS "taxedQueryFees", CAST(0 AS BIGNUM) AS "curatorQueryFees", CAST(reward AS BIGNUM) AS "totalRewards", indexer_reward AS "totalIndexerRewards", delegator_reward AS "totalDelegatorRewards"
FROM delegation_ledger
UNION ALL
SELECT block_number, log_index,
       CAST(0 AS BIGNUM) AS "signalledTokens", CAST(0 AS BIGNUM) AS "stakeDeposited", fees AS "queryFeeRebates", fees AS "queryFeesCollected", fees + curator AS "totalQueryFees", CAST(0 AS BIGNUM) AS "taxedQueryFees", curator AS "curatorQueryFees", CAST(0 AS BIGNUM) AS "totalRewards", CAST(0 AS BIGNUM) AS "totalIndexerRewards", CAST(0 AS BIGNUM) AS "totalDelegatorRewards"
FROM allocation_query_fee_movement f WHERE EXISTS (SELECT 1 FROM subgraph_service__query_fees_collected s WHERE s.block_number = f.block_number AND s.log_index = f.log_index);

CREATE VIEW epoch AS
SELECT b.id, b."startBlock", b."endBlock",
       CAST(coalesce(sum(m."signalledTokens"), CAST(0 AS BIGNUM)) AS VARCHAR) AS "signalledTokens",
       CAST(coalesce(sum(m."stakeDeposited"), CAST(0 AS BIGNUM)) AS VARCHAR) AS "stakeDeposited",
       CAST(coalesce(sum(m."queryFeeRebates"), CAST(0 AS BIGNUM)) AS VARCHAR) AS "queryFeeRebates",
       CAST(coalesce(sum(m."queryFeesCollected"), CAST(0 AS BIGNUM)) AS VARCHAR) AS "queryFeesCollected",
       CAST(coalesce(sum(m."totalQueryFees"), CAST(0 AS BIGNUM)) AS VARCHAR) AS "totalQueryFees",
       CAST(coalesce(sum(m."taxedQueryFees"), CAST(0 AS BIGNUM)) AS VARCHAR) AS "taxedQueryFees",
       CAST(coalesce(sum(m."curatorQueryFees"), CAST(0 AS BIGNUM)) AS VARCHAR) AS "curatorQueryFees",
       CAST(coalesce(sum(m."totalRewards"), CAST(0 AS BIGNUM)) AS VARCHAR) AS "totalRewards",
       CAST(coalesce(sum(m."totalIndexerRewards"), CAST(0 AS BIGNUM)) AS VARCHAR) AS "totalIndexerRewards",
       CAST(coalesce(sum(m."totalDelegatorRewards"), CAST(0 AS BIGNUM)) AS VARCHAR) AS "totalDelegatorRewards"
FROM epoch_bounds b
LEFT JOIN network_epoch_observation o ON o.epoch = b.epoch
LEFT JOIN epoch_financial_movement m ON m.block_number = o.block_number AND m.log_index = o.log_index
GROUP BY b.id, b."startBlock", b."endBlock";
