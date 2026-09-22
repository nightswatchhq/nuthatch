CREATE VIEW horizon_query_fee_distribution AS
WITH pool AS (
    SELECT f.allocation, f.indexer, f.block_number, f.log_index, f.fees,
        coalesce(b.delegated, CAST(0 AS BIGNUM)) + coalesce(sum(m.tokens), CAST(0 AS BIGNUM)) AS delegated
    FROM allocation_query_fee_movement f
    JOIN subgraph_service__query_fees_collected raw
        ON raw.block_number = f.block_number AND raw.log_index = f.log_index
    JOIN provision_identity i ON i.indexer = f.indexer
        AND i.service = '0xb2bb92d0de618878e438b55d5846cfecd9301105'
    LEFT JOIN provision_delegation_bootstrap b ON b.indexer = i.indexer AND b.service = i.service
    LEFT JOIN provision_delegation_movement m ON m.indexer = i.indexer AND m.service = i.service
        AND (m.block_number > i.block_number OR (m.block_number = i.block_number AND m.log_index >= i.log_index))
        AND (m.block_number < f.block_number OR (m.block_number = f.block_number AND m.log_index < f.log_index))
    GROUP BY f.allocation, f.indexer, f.block_number, f.log_index, f.fees, b.delegated
), cuts AS (
    SELECT f.*, CAST(1000000 - coalesce(CAST(c."feeCut" AS BIGINT), 0) AS VARCHAR) AS cut
    FROM pool f LEFT JOIN horizon_staking__delegation_fee_cut_set c
        ON c."serviceProvider" = f.indexer AND c.verifier = '0xb2bb92d0de618878e438b55d5846cfecd9301105'
        AND c."paymentType" = '0'
        AND (c.block_number < f.block_number OR (c.block_number = f.block_number AND c.log_index < f.log_index))
    QUALIFY row_number() OVER (PARTITION BY f.block_number, f.log_index
        ORDER BY c.block_number DESC, c.log_index DESC) = 1
), split AS (
    SELECT *, CASE WHEN delegated = CAST(0 AS BIGNUM) THEN fees
        ELSE CAST(nuthatch_mul_div(CAST(fees AS VARCHAR), cut, '1000000') AS BIGNUM) END AS indexer_fees
    FROM cuts
)
SELECT allocation, indexer, block_number, log_index, fees, indexer_fees,
       fees - indexer_fees AS delegator_fees FROM split;

CREATE VIEW allocation_rebate_event AS
SELECT "allocationID" AS allocation, block_number, log_index, true AS replaces,
       CAST(tokens AS BIGNUM) AS indexer_fees, CAST("delegationFees" AS BIGNUM) AS delegator_fees,
       CAST(0 AS BIGNUM) AS distributed FROM staking_legacy__rebate_claimed
UNION ALL
SELECT "allocationID", block_number, log_index, true,
       CAST("queryRebates" AS BIGNUM), CAST("delegationRewards" AS BIGNUM), CAST("queryRebates" AS BIGNUM)
FROM staking_legacy__rebate_collected
UNION ALL
SELECT allocation, block_number, log_index, false, indexer_fees, delegator_fees, fees
FROM horizon_query_fee_distribution;

CREATE VIEW allocation_rebate_totals AS
WITH replacement AS (
    SELECT * FROM allocation_rebate_event WHERE replaces
    QUALIFY row_number() OVER (PARTITION BY allocation ORDER BY block_number DESC, log_index DESC) = 1
), totals AS (
    SELECT allocation, sum(distributed) AS distributed FROM allocation_rebate_event GROUP BY allocation
), additions AS (
    SELECT e.allocation, sum(e.indexer_fees) AS indexer_fees, sum(e.delegator_fees) AS delegator_fees
    FROM allocation_rebate_event e LEFT JOIN replacement r ON r.allocation = e.allocation
    WHERE NOT e.replaces AND (r.allocation IS NULL OR e.block_number > r.block_number
        OR (e.block_number = r.block_number AND e.log_index > r.log_index))
    GROUP BY e.allocation
)
SELECT t.allocation AS id,
       CAST(coalesce(r.indexer_fees, CAST(0 AS BIGNUM)) + coalesce(a.indexer_fees, CAST(0 AS BIGNUM)) AS VARCHAR) AS "queryFeeRebates",
       CAST(coalesce(r.delegator_fees, CAST(0 AS BIGNUM)) + coalesce(a.delegator_fees, CAST(0 AS BIGNUM)) AS VARCHAR) AS "delegationFees",
       CAST(t.distributed AS VARCHAR) AS "distributedRebates"
FROM totals t
LEFT JOIN replacement r ON r.allocation = t.allocation
LEFT JOIN additions a ON a.allocation = t.allocation;

CREATE VIEW provision_query_fee_totals AS
SELECT indexer, CAST(sum(fees) AS VARCHAR) AS "queryFeesCollected",
       CAST(sum(indexer_fees) AS VARCHAR) AS "indexerQueryFees",
       CAST(sum(delegator_fees) AS VARCHAR) AS "delegatorQueryFees"
FROM horizon_query_fee_distribution GROUP BY indexer;

CREATE VIEW indexer_query_fee_totals AS
WITH movements AS (
    SELECT indexer, fees, CAST(0 AS BIGNUM) AS rebates, CAST(0 AS BIGNUM) AS delegator
    FROM allocation_query_fee_movement
    UNION ALL
    SELECT indexer, CAST(0 AS BIGNUM), CAST(tokens AS BIGNUM), CAST("delegationFees" AS BIGNUM)
    FROM staking_legacy__rebate_claimed
    UNION ALL
    SELECT indexer, CAST(0 AS BIGNUM), CAST("queryRebates" AS BIGNUM), CAST("delegationRewards" AS BIGNUM)
    FROM staking_legacy__rebate_collected
    UNION ALL
    SELECT indexer, CAST(0 AS BIGNUM), indexer_fees, delegator_fees
    FROM horizon_query_fee_distribution
)
SELECT indexer, CAST(sum(fees) AS VARCHAR) AS "queryFeesCollected",
       CAST(sum(rebates) AS VARCHAR) AS "queryFeeRebates",
       CAST(sum(delegator) AS VARCHAR) AS "delegatorQueryFees"
FROM movements GROUP BY indexer;
