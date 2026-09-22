CREATE VIEW indexer AS
WITH identities AS (
    SELECT indexer AS id, min(block_timestamp) AS created FROM indexer_identity_event GROUP BY indexer
), legacy AS (
    SELECT indexer, legacy FROM indexer_identity_event WHERE legacy IS NOT NULL
    QUALIFY row_number() OVER (PARTITION BY indexer ORDER BY block_number DESC, log_index DESC) = 1
), stakes AS (
    SELECT indexer, sum(tokens) AS tokens FROM indexer_stake_movement GROUP BY indexer
), allocations AS (
    SELECT indexer, count(*) AS total, count(*) FILTER (WHERE status = 'Active') AS active,
           sum(CASE WHEN status = 'Active' THEN CAST("allocatedTokens" AS BIGNUM) ELSE CAST(0 AS BIGNUM) END) AS tokens
    FROM allocation_lifecycle GROUP BY indexer
), provisions AS (
    SELECT indexer, sum(provisioned) AS tokens, sum(thawing) AS thawing
    FROM provision_movement GROUP BY indexer
), thaws AS (
    SELECT "serviceProvider" AS indexer, max(CAST("thawingUntil" AS BIGNUM)) AS until
    FROM horizon_staking__thaw_request_created WHERE "requestType" = '0'
    GROUP BY "serviceProvider"
), fee_cuts AS (
    SELECT "serviceProvider" AS indexer, "paymentType" AS kind,
           1000000 - CAST("feeCut" AS INTEGER) AS cut
    FROM horizon_staking__delegation_fee_cut_set
    QUALIFY row_number() OVER (PARTITION BY "serviceProvider", "paymentType"
        ORDER BY block_number DESC, log_index DESC) = 1
), legacy_cuts AS (
    SELECT indexer, CAST("indexingRewardCut" AS INTEGER) AS rewards,
           CAST("queryFeeCut" AS INTEGER) AS query, CAST("cooldownBlocks" AS INTEGER) AS cooldown
    FROM staking_legacy__delegation_parameters_updated
    QUALIFY row_number() OVER (PARTITION BY indexer ORDER BY block_number DESC, log_index DESC) = 1
)
SELECT i.id, CAST(i.created AS INTEGER) AS "createdAt", coalesce(l.legacy, false) AS "isLegacy",
       coalesce(fees."queryFeesCollected", '0') AS "queryFeesCollected",
       coalesce(fees."queryFeeRebates", '0') AS "queryFeeRebates",
       coalesce(fees."delegatorQueryFees", '0') AS "delegatorQueryFees",
       coalesce(rew."rewardsEarned", '0') AS "rewardsEarned",
       coalesce(rew."indexerIndexingRewards", '0') AS "indexerIndexingRewards",
       coalesce(rew."delegatorIndexingRewards", '0') AS "delegatorIndexingRewards",
       coalesce(cap."delegatedCapacity", '0') AS "delegatedCapacity",
       coalesce(cap."tokenCapacity", '0') AS "tokenCapacity",
       coalesce(cap."availableStake", '0') AS "availableStake",
       CAST(coalesce(s.tokens, CAST(0 AS BIGNUM)) AS VARCHAR) AS "stakedTokens",
       CAST(coalesce(a.tokens, CAST(0 AS BIGNUM)) AS VARCHAR) AS "allocatedTokens",
       CAST(coalesce(p.tokens, CAST(0 AS BIGNUM)) AS VARCHAR) AS "provisionedTokens",
       CAST(coalesce(p.thawing, CAST(0 AS BIGNUM)) AS VARCHAR) AS "thawingTokens",
       CAST(coalesce(t.until, CAST(0 AS BIGNUM)) AS VARCHAR) AS "thawingUntil",
       coalesce(q.cut, 1000000) AS "queryFeeCut",
       coalesce(r.cut, 1000000) AS "indexingRewardCut",
       coalesce(c.query, 0) AS "legacyQueryFeeCut",
       coalesce(c.rewards, 0) AS "legacyIndexingRewardCut",
       coalesce(c.cooldown, 0) AS "delegatorParameterCooldown",
       coalesce(d."delegatedTokens", '0') AS "delegatedTokens",
       coalesce(d."delegatorShares", '0') AS "delegatorShares",
       coalesce(d."delegatedThawingTokens", '0') AS "delegatedThawingTokens",
       coalesce(k."lockedTokens", '0') AS "lockedTokens",
       coalesce(k."tokensLockedUntil", 0) AS "tokensLockedUntil",
       coalesce(k."legacyLockedTokens", '0') AS "legacyLockedTokens",
       coalesce(k."legacyTokensLockedUntil", 0) AS "legacyTokensLockedUntil",
       CAST(coalesce(a.active, 0) AS INTEGER) AS "allocationCount",
       CAST(coalesce(a.total, 0) AS VARCHAR) AS "totalAllocationCount"
FROM identities i
LEFT JOIN indexer_query_fee_totals fees ON fees.indexer = i.id
LEFT JOIN indexer_reward_distribution rew ON rew.indexer = i.id
LEFT JOIN indexer_capacity cap ON cap.indexer = i.id
LEFT JOIN legacy l ON l.indexer = i.id
LEFT JOIN stakes s ON s.indexer = i.id
LEFT JOIN allocations a ON a.indexer = i.id
LEFT JOIN provisions p ON p.indexer = i.id
LEFT JOIN thaws t ON t.indexer = i.id
LEFT JOIN indexer_lock_state k ON k.indexer = i.id
LEFT JOIN fee_cuts q ON q.indexer = i.id AND q.kind = '0'
LEFT JOIN fee_cuts r ON r.indexer = i.id AND r.kind = '2'
LEFT JOIN legacy_cuts c ON c.indexer = i.id
LEFT JOIN indexer_delegation d ON d.indexer = i.id;

CREATE VIEW data_service AS
SELECT DISTINCT service AS id FROM provision_identity;
