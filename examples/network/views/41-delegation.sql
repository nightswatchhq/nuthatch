-- Legacy reward distribution depends on whether the delegation pool was empty at that
-- event, not whether it is empty now. Keep the ordered balance fold explicit.
CREATE VIEW delegation_event AS
SELECT indexer, block_number, log_index, block_timestamp, CAST(tokens AS BIGNUM) AS delta,
       '0' AS reward, CAST(shares AS BIGNUM) AS shares, CAST(0 AS BIGNUM) AS thawing
FROM staking_legacy__stake_delegated
UNION ALL
SELECT indexer, block_number, log_index, block_timestamp, CAST(0 AS BIGNUM) - CAST(tokens AS BIGNUM),
       '0', CAST(0 AS BIGNUM) - CAST(shares AS BIGNUM), CAST(0 AS BIGNUM)
FROM staking_legacy__stake_delegated_locked
UNION ALL
SELECT indexer, block_number, log_index, block_timestamp, CAST("delegationFees" AS BIGNUM),
       '0', CAST(0 AS BIGNUM), CAST(0 AS BIGNUM) FROM staking_legacy__rebate_claimed
UNION ALL
SELECT indexer, block_number, log_index, block_timestamp, CAST("delegationRewards" AS BIGNUM),
       '0', CAST(0 AS BIGNUM), CAST(0 AS BIGNUM) FROM staking_legacy__rebate_collected
UNION ALL
SELECT "serviceProvider", block_number, log_index, block_timestamp, CAST(tokens AS BIGNUM),
       '0', CAST(shares AS BIGNUM), CAST(0 AS BIGNUM) FROM horizon_staking__tokens_delegated
UNION ALL
SELECT "serviceProvider", block_number, log_index, block_timestamp, CAST(tokens AS BIGNUM),
       '0', CAST(0 AS BIGNUM), CAST(0 AS BIGNUM) FROM horizon_staking__tokens_to_delegation_pool_added
UNION ALL
SELECT "serviceProvider", block_number, log_index, block_timestamp, CAST(0 AS BIGNUM) - CAST(tokens AS BIGNUM),
       '0', CAST(0 AS BIGNUM), CAST(0 AS BIGNUM) FROM horizon_staking__delegation_slashed
UNION ALL
SELECT "serviceProvider", block_number, log_index, block_timestamp, CAST(0 AS BIGNUM),
       '0', CAST(0 AS BIGNUM) - CAST(shares AS BIGNUM), CAST(tokens AS BIGNUM) FROM horizon_staking__tokens_undelegated
UNION ALL
SELECT "serviceProvider", block_number, log_index, block_timestamp, CAST(0 AS BIGNUM) - CAST(tokens AS BIGNUM),
       '0', CAST(0 AS BIGNUM), CAST(0 AS BIGNUM) - CAST(tokens AS BIGNUM) FROM horizon_staking__delegated_tokens_withdrawn
UNION ALL
SELECT indexer, block_number, log_index, block_timestamp, CAST(0 AS BIGNUM), amount,
       CAST(0 AS BIGNUM), CAST(0 AS BIGNUM) FROM rewards__rewards_assigned
UNION ALL
SELECT r.indexer, r.block_number, r.log_index, r.block_timestamp, CAST(0 AS BIGNUM), r.amount,
       CAST(0 AS BIGNUM), CAST(0 AS BIGNUM)
FROM rewards__horizon_rewards_assigned r JOIN allocation_creation a ON a.id = r."allocationID" AND a.legacy;

CREATE VIEW delegation_ordered AS
SELECT e.*, row_number() OVER (PARTITION BY indexer ORDER BY block_number, log_index) AS seq,
       coalesce((SELECT p."indexingRewardCut" FROM staking_legacy__delegation_parameters_updated p
           WHERE p.indexer = e.indexer AND
               (p.block_number < e.block_number OR (p.block_number = e.block_number AND p.log_index < e.log_index))
           ORDER BY p.block_number DESC, p.log_index DESC LIMIT 1), '0') AS cut
FROM delegation_event e;

CREATE VIEW delegation_ledger AS
-- Deposits, shares and thawing are additive. Only rewards need a recursive
-- decision based on the preceding pool balance; carry their cumulative result
-- back to every event rather than traversing all movements recursively.
WITH RECURSIVE ordered AS MATERIALIZED (
    SELECT *,
           sum(delta) OVER w AS base_tokens,
           sum(shares) OVER w AS total_shares,
           sum(thawing) OVER w AS total_thawing,
           sum(CASE WHEN reward <> '0' THEN 1 ELSE 0 END) OVER w AS reward_seq
    FROM delegation_ordered
    WINDOW w AS (PARTITION BY indexer ORDER BY seq
                 ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)
), reward_events AS MATERIALIZED (
    SELECT * FROM ordered WHERE reward <> '0'
), ledger(indexer, reward_seq, reward_total, delegator_reward) AS (
    SELECT DISTINCT indexer, 0::HUGEINT, CAST(0 AS BIGNUM), CAST(0 AS BIGNUM)
    FROM ordered
    UNION ALL
    SELECT l.indexer, e.reward_seq, l.reward_total + r.tokens, r.tokens
    FROM ledger l JOIN reward_events e ON e.indexer = l.indexer AND e.reward_seq = l.reward_seq + 1
    CROSS JOIN LATERAL (
        -- The pool before this event includes previous distributed rewards,
        -- but excludes the current event's delta.
        SELECT CASE WHEN e.base_tokens - e.delta + l.reward_total = CAST(0 AS BIGNUM)
                    THEN CAST(0 AS BIGNUM)
                    ELSE CAST(e.reward AS BIGNUM) - CAST(nuthatch_mul_div(e.reward, e.cut, '1000000') AS BIGNUM) END AS tokens
    ) r
)
SELECT e.indexer, e.block_number, e.log_index, e.block_timestamp, e.reward,
       e.base_tokens + l.reward_total AS delegated,
       CASE WHEN e.reward <> '0' THEN l.delegator_reward ELSE CAST(0 AS BIGNUM) END AS delegator_reward,
       CAST(e.reward AS BIGNUM) - CASE WHEN e.reward <> '0' THEN l.delegator_reward ELSE CAST(0 AS BIGNUM) END AS indexer_reward,
       e.total_shares AS shares, e.total_thawing AS thawing
FROM ordered e JOIN ledger l ON l.indexer = e.indexer AND l.reward_seq = e.reward_seq;

CREATE VIEW indexer_delegation AS
SELECT indexer, CAST(delegated AS VARCHAR) AS "delegatedTokens",
       CAST(shares AS VARCHAR) AS "delegatorShares", CAST(thawing AS VARCHAR) AS "delegatedThawingTokens"
FROM delegation_ledger
QUALIFY row_number() OVER (PARTITION BY indexer ORDER BY block_number DESC, log_index DESC) = 1;

-- SubgraphService inherits the indexer's pool exactly when its provision is first created.
-- Other verifiers start at zero. Legacy rewards subsequently update an existing service
-- provision, while legacy deposits/locks and movements for other verifiers do not.
CREATE VIEW provision_delegation_movement AS
SELECT "serviceProvider" AS indexer, verifier AS service, block_number, log_index,
       CAST(tokens AS BIGNUM) AS tokens, CAST(shares AS BIGNUM) AS shares,
       CAST(0 AS BIGNUM) AS thawing, CAST(0 AS BIGNUM) AS slashed
FROM horizon_staking__tokens_delegated
UNION ALL
SELECT "serviceProvider", verifier, block_number, log_index, CAST(tokens AS BIGNUM),
       CAST(0 AS BIGNUM), CAST(0 AS BIGNUM), CAST(0 AS BIGNUM)
FROM horizon_staking__tokens_to_delegation_pool_added
UNION ALL
SELECT "serviceProvider", verifier, block_number, log_index, CAST(0 AS BIGNUM) - CAST(tokens AS BIGNUM),
       CAST(0 AS BIGNUM), CAST(0 AS BIGNUM), CAST(tokens AS BIGNUM)
FROM horizon_staking__delegation_slashed
UNION ALL
SELECT "serviceProvider", verifier, block_number, log_index, CAST(0 AS BIGNUM),
       CAST(0 AS BIGNUM) - CAST(shares AS BIGNUM), CAST(tokens AS BIGNUM), CAST(0 AS BIGNUM)
FROM horizon_staking__tokens_undelegated
UNION ALL
SELECT "serviceProvider", verifier, block_number, log_index, CAST(0 AS BIGNUM) - CAST(tokens AS BIGNUM),
       CAST(0 AS BIGNUM), CAST(0 AS BIGNUM) - CAST(tokens AS BIGNUM), CAST(0 AS BIGNUM)
FROM horizon_staking__delegated_tokens_withdrawn
UNION ALL
SELECT indexer, '0xb2bb92d0de618878e438b55d5846cfecd9301105', block_number, log_index,
       delegator_reward, CAST(0 AS BIGNUM), CAST(0 AS BIGNUM), CAST(0 AS BIGNUM)
FROM delegation_ledger WHERE reward <> '0';

CREATE VIEW provision_delegation_bootstrap AS
    SELECT i.indexer, i.service, l.delegated, l.shares, l.thawing
    FROM provision_identity i LEFT JOIN delegation_ledger l
      ON i.service = '0xb2bb92d0de618878e438b55d5846cfecd9301105' AND l.indexer = i.indexer
      AND (l.block_number < i.block_number OR
           (l.block_number = i.block_number AND l.log_index < i.log_index))
    QUALIFY row_number() OVER (PARTITION BY i.indexer, i.service
        ORDER BY l.block_number DESC, l.log_index DESC) = 1;

CREATE VIEW provision_delegation AS
WITH movements AS (
    SELECT i.indexer, i.service, sum(m.tokens) AS tokens, sum(m.shares) AS shares,
           sum(m.thawing) AS thawing, sum(m.slashed) AS slashed
    FROM provision_identity i JOIN provision_delegation_movement m
      ON m.indexer = i.indexer AND m.service = i.service
      AND (m.block_number > i.block_number OR
           (m.block_number = i.block_number AND m.log_index >= i.log_index))
    GROUP BY i.indexer, i.service
)
SELECT i.indexer, i.service,
       CAST(coalesce(b.delegated, CAST(0 AS BIGNUM)) + coalesce(m.tokens, CAST(0 AS BIGNUM)) AS VARCHAR) AS "delegatedTokens",
       CAST(coalesce(b.shares, CAST(0 AS BIGNUM)) + coalesce(m.shares, CAST(0 AS BIGNUM)) AS VARCHAR) AS "delegatorShares",
       CAST(coalesce(b.thawing, CAST(0 AS BIGNUM)) + coalesce(m.thawing, CAST(0 AS BIGNUM)) AS VARCHAR) AS "delegatedThawingTokens",
       CAST(coalesce(m.slashed, CAST(0 AS BIGNUM)) AS VARCHAR) AS "tokensSlashedDelegationPool"
FROM provision_identity i
LEFT JOIN provision_delegation_bootstrap b ON b.indexer = i.indexer AND b.service = i.service
LEFT JOIN movements m ON m.indexer = i.indexer AND m.service = i.service;
