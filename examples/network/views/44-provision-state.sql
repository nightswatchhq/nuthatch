CREATE VIEW provision AS
WITH identities AS (
    SELECT indexer, service, block_timestamp AS created FROM provision_identity
), balances AS (
    SELECT indexer, service, sum(provisioned) AS provisioned, sum(thawing) AS thawing,
           sum(slashed) AS slashed FROM provision_movement GROUP BY indexer, service
), allocations AS (
    SELECT indexer, service, sum(tokens) AS tokens, sum(active) AS active, sum(created) AS created
    FROM provision_allocation_movement GROUP BY indexer, service
), parameters AS (
    SELECT *, row_number() OVER (PARTITION BY indexer, service ORDER BY block_number DESC, log_index DESC) AS rank
    FROM provision_parameters WHERE active
), staged AS (
    SELECT *, row_number() OVER (PARTITION BY indexer, service ORDER BY block_number DESC, log_index DESC) AS rank
    FROM provision_parameters WHERE pending
), thaws AS (
    SELECT "serviceProvider" AS indexer, verifier AS service,
           max(CAST("thawingUntil" AS BIGNUM)) AS until
    FROM horizon_staking__thaw_request_created WHERE "requestType" = '0'
    GROUP BY "serviceProvider", verifier
)
SELECT i.indexer || '-' || i.service AS id, i.indexer, i.service AS "dataService",
       CAST(i.created AS VARCHAR) AS "createdAt",
       coalesce(fees."queryFeesCollected", '0') AS "queryFeesCollected",
       coalesce(fees."indexerQueryFees", '0') AS "indexerQueryFees",
       coalesce(fees."delegatorQueryFees", '0') AS "delegatorQueryFees",
       coalesce(rew."rewardsEarned", '0') AS "rewardsEarned",
       coalesce(rew."indexerIndexingRewards", '0') AS "indexerIndexingRewards",
       coalesce(rew."delegatorIndexingRewards", '0') AS "delegatorIndexingRewards",
       coalesce(reg.url, '') AS url, coalesce(reg."geoHash", '') AS "geoHash",
       coalesce(dest.destination, '0x00000000') AS "rewardsDestination",
       CAST(coalesce(b.provisioned, CAST(0 AS BIGNUM)) AS VARCHAR) AS "tokensProvisioned",
       CAST(coalesce(b.thawing, CAST(0 AS BIGNUM)) AS VARCHAR) AS "tokensThawing",
       CAST(coalesce(t.until, CAST(0 AS BIGNUM)) AS VARCHAR) AS "thawingUntil",
       CAST(coalesce(b.slashed, CAST(0 AS BIGNUM)) AS VARCHAR) AS "tokensSlashedServiceProvider",
       d."delegatedTokens", d."delegatorShares", d."delegatedThawingTokens", d."tokensSlashedDelegationPool",
       CAST(coalesce(a.tokens, CAST(0 AS BIGNUM)) AS VARCHAR) AS "tokensAllocated",
       CAST(coalesce(a.active, 0) AS INTEGER) AS "allocationCount",
       CAST(coalesce(a.created, 0) AS VARCHAR) AS "totalAllocationCount",
       coalesce(p."maxVerifierCut", '0') AS "maxVerifierCut",
       coalesce(p."thawingPeriod", '0') AS "thawingPeriod",
       coalesce(s."maxVerifierCut", '0') AS "maxVerifierCutPending",
       coalesce(s."thawingPeriod", '0') AS "thawingPeriodPending",
       coalesce(q.cut, '1000000') AS "queryFeeCut",
       coalesce(f.cut, '1000000') AS "indexingFeeCut",
       coalesce(r.cut, '1000000') AS "indexingRewardsCut"
FROM identities i
LEFT JOIN provision_query_fee_totals fees ON fees.indexer = i.indexer
    AND i.service = '0xb2bb92d0de618878e438b55d5846cfecd9301105'
LEFT JOIN provision_reward_distribution rew ON rew.indexer = i.indexer AND rew.service = i.service
JOIN provision_delegation d ON d.indexer = i.indexer AND d.service = i.service
LEFT JOIN provision_registration reg ON reg.indexer = i.indexer
    AND i.service = '0xb2bb92d0de618878e438b55d5846cfecd9301105'
LEFT JOIN provision_rewards_destination dest ON dest.indexer = i.indexer
    AND i.service = '0xb2bb92d0de618878e438b55d5846cfecd9301105'
LEFT JOIN balances b ON b.indexer = i.indexer AND b.service = i.service
LEFT JOIN thaws t ON t.indexer = i.indexer AND t.service = i.service
LEFT JOIN allocations a ON a.indexer = i.indexer AND a.service = i.service
LEFT JOIN parameters p ON p.indexer = i.indexer AND p.service = i.service AND p.rank = 1
LEFT JOIN staged s ON s.indexer = i.indexer AND s.service = i.service AND s.rank = 1
LEFT JOIN provision_fee_cut q ON q.indexer = i.indexer AND q.service = i.service AND q.payment_type = '0'
LEFT JOIN provision_fee_cut f ON f.indexer = i.indexer AND f.service = i.service AND f.payment_type = '1'
LEFT JOIN provision_fee_cut r ON r.indexer = i.indexer AND r.service = i.service AND r.payment_type = '2';
