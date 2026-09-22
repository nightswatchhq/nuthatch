CREATE VIEW protocol_percentage_update AS
SELECT p.block_number, p.log_index,
       CASE WHEN r.result IS NULL OR r.reverted
            THEN error('missing pinned protocolPercentage read')
            ELSE nuthatch_uint256(r.result) END AS percentage
FROM staking_parameters__parameter_updated p
LEFT JOIN (SELECT DISTINCT block_number, block_hash, result, reverted FROM protocol_percentage_read) r
ON r.block_number = p.block_number AND r.block_hash = p.block_hash
WHERE p.param = 'protocolPercentage';

CREATE VIEW allocation_query_fee_movement AS
SELECT "allocationID" AS allocation, "subgraphDeploymentID" AS deployment,
       indexer, block_number, log_index, block_timestamp,
       CAST("rebateFees" AS BIGNUM) AS fees, CAST("curationFees" AS BIGNUM) AS curator
FROM staking_legacy__allocation_collected
UNION ALL
SELECT "allocationID", "subgraphDeploymentID", indexer, block_number, log_index, block_timestamp,
       CAST("queryFees" AS BIGNUM), CAST("curationFees" AS BIGNUM)
FROM staking_legacy__rebate_collected
UNION ALL
SELECT f."allocationId", f."subgraphDeploymentId", f."serviceProvider",
       f.block_number, f.log_index, f.block_timestamp,
       CAST(f."tokensCollected" AS BIGNUM) - CAST(f."tokensCurators" AS BIGNUM) -
       CAST(nuthatch_mul_div(f."tokensCollected", coalesce((
           SELECT percentage FROM protocol_percentage_update p
           WHERE p.block_number < f.block_number OR (p.block_number = f.block_number AND p.log_index < f.log_index)
           ORDER BY p.block_number DESC, p.log_index DESC LIMIT 1
       ), '0'), '1000000') AS BIGNUM), CAST(f."tokensCurators" AS BIGNUM)
FROM subgraph_service__query_fees_collected f;

CREATE VIEW allocation_reward_movement AS
SELECT "allocationID" AS allocation, amount AS tokens, block_number, log_index, block_timestamp
FROM rewards__rewards_assigned
UNION ALL
SELECT r."allocationID", r.amount, r.block_number, r.log_index, r.block_timestamp
FROM rewards__horizon_rewards_assigned r
JOIN allocation_creation a ON a.id = r."allocationID" AND a.legacy
UNION ALL
SELECT "allocationId", "tokensRewards", block_number, log_index, block_timestamp
FROM subgraph_service__indexing_rewards_collected;

CREATE VIEW allocation_fee_totals AS
SELECT allocation AS id, CAST(sum(fees) AS VARCHAR) AS "queryFeesCollected",
       CAST(sum(curator) AS VARCHAR) AS "curatorRewards"
FROM allocation_query_fee_movement GROUP BY allocation;

CREATE VIEW allocation_reward_totals AS
SELECT allocation AS id, CAST(sum(CAST(tokens AS BIGNUM)) AS VARCHAR) AS "indexingRewards"
FROM allocation_reward_movement GROUP BY allocation;
