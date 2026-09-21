-- Match calculateCapacities call sites, not every event that changes a balance.
-- Parameters and Horizon stake movements do not themselves refresh these cached fields.
CREATE VIEW indexer_capacity_refresh AS
SELECT indexer, block_number, log_index FROM (
    SELECT indexer, block_number, log_index FROM staking_legacy__stake_deposited
    UNION ALL SELECT indexer, block_number, log_index FROM staking_legacy__stake_locked
    UNION ALL SELECT indexer, block_number, log_index FROM staking_legacy__stake_withdrawn
    UNION ALL SELECT indexer, block_number, log_index FROM staking_legacy__stake_slashed
    UNION ALL SELECT indexer, block_number, log_index FROM staking_legacy__allocation_created
    UNION ALL SELECT indexer, block_number, log_index FROM staking_legacy__allocation_closed_f672
    UNION ALL SELECT indexer, block_number, log_index FROM staking_legacy__allocation_closed_7203
    -- Every delegation event refreshes capacity. Finding the last event needs its
    -- key, not a reconstruction of every intermediate pool balance.
    UNION ALL SELECT indexer, block_number, log_index FROM delegation_event
    UNION ALL SELECT indexer, block_number, log_index FROM provision_movement
    UNION ALL SELECT indexer, block_number, log_index FROM provision_allocation_movement
    UNION ALL SELECT "serviceProvider", block_number, log_index
        FROM horizon_staking__thaw_request_created WHERE "requestType" = '0'
)
QUALIFY row_number() OVER (PARTITION BY indexer ORDER BY block_number DESC, log_index DESC) = 1;

CREATE VIEW indexer_capacity AS
WITH refresh AS MATERIALIZED (
    SELECT * FROM indexer_capacity_refresh
), stake AS (
    SELECT r.indexer, l.stake, l.allocated, l.locked
    FROM refresh r LEFT JOIN indexer_lock_ledger l ON l.indexer = r.indexer
      AND (l.block_number < r.block_number OR
           (l.block_number = r.block_number AND l.log_index <= r.log_index))
    QUALIFY row_number() OVER (PARTITION BY r.indexer ORDER BY l.block_number DESC, l.log_index DESC) = 1
), delegation AS (
    SELECT r.indexer, l.delegated, l.thawing
    FROM refresh r LEFT JOIN delegation_ledger l ON l.indexer = r.indexer
      AND (l.block_number < r.block_number OR
           (l.block_number = r.block_number AND l.log_index <= r.log_index))
    QUALIFY row_number() OVER (PARTITION BY r.indexer ORDER BY l.block_number DESC, l.log_index DESC) = 1
), provision AS (
    SELECT r.indexer, sum(p.provisioned) AS provisioned, sum(p.thawing) AS thawing
    FROM refresh r LEFT JOIN provision_movement p ON p.indexer = r.indexer
      AND (p.block_number < r.block_number OR
           (p.block_number = r.block_number AND p.log_index <= r.log_index))
    GROUP BY r.indexer
), allocation AS (
    SELECT r.indexer, sum(a.tokens) AS tokens
    FROM refresh r LEFT JOIN provision_allocation_movement a ON a.indexer = r.indexer
      AND (a.block_number < r.block_number OR
           (a.block_number = r.block_number AND a.log_index <= r.log_index))
    GROUP BY r.indexer
), parameter_at_refresh AS (
    SELECT r.indexer, p.parameter, p.value
    FROM refresh r LEFT JOIN protocol_parameter_event p
      ON p.parameter IN ('delegationRatio', 'maxThawingPeriod')
      AND (p.block_number < r.block_number OR
           (p.block_number = r.block_number AND p.log_index <= r.log_index))
    QUALIFY row_number() OVER (PARTITION BY r.indexer, p.parameter
        ORDER BY p.block_number DESC, p.log_index DESC) = 1
), inputs AS (
    SELECT r.indexer,
        coalesce(s.stake, CAST(0 AS BIGNUM)) AS stake,
        coalesce(s.allocated, CAST(0 AS BIGNUM)) AS legacy_allocated,
        coalesce(s.locked, CAST(0 AS BIGNUM)) AS locked,
        coalesce(d.delegated, CAST(0 AS BIGNUM)) AS delegated,
        coalesce(d.thawing, CAST(0 AS BIGNUM)) AS delegated_thawing,
        coalesce(p.provisioned, CAST(0 AS BIGNUM)) AS provisioned,
        coalesce(p.thawing, CAST(0 AS BIGNUM)) AS thawing,
        coalesce(a.tokens, CAST(0 AS BIGNUM)) AS allocated,
        coalesce(ratio.value, '0') AS ratio,
        CAST(coalesce(mode.value, '0') AS BIGNUM) > CAST(0 AS BIGNUM) AS horizon
    FROM refresh r
    LEFT JOIN stake s ON s.indexer = r.indexer
    LEFT JOIN delegation d ON d.indexer = r.indexer
    LEFT JOIN provision p ON p.indexer = r.indexer
    LEFT JOIN allocation a ON a.indexer = r.indexer
    LEFT JOIN parameter_at_refresh ratio ON ratio.indexer = r.indexer AND ratio.parameter = 'delegationRatio'
    LEFT JOIN parameter_at_refresh mode ON mode.indexer = r.indexer AND mode.parameter = 'maxThawingPeriod'
), delegated AS (
    SELECT *, least(
        delegated - CASE WHEN horizon THEN delegated_thawing ELSE CAST(0 AS BIGNUM) END,
        CAST(nuthatch_mul_div(CAST(CASE WHEN horizon THEN provisioned ELSE stake END AS VARCHAR), ratio, '1') AS BIGNUM)
    ) AS delegated_capacity FROM inputs
), capacity AS (
    SELECT *, CASE WHEN horizon THEN provisioned - thawing ELSE stake END + delegated_capacity AS capacity
    FROM delegated
)
SELECT indexer, CAST(delegated_capacity AS VARCHAR) AS "delegatedCapacity",
       CAST(capacity AS VARCHAR) AS "tokenCapacity",
       CAST(capacity - allocated - CASE WHEN horizon THEN CAST(0 AS BIGNUM)
            ELSE legacy_allocated + locked END AS VARCHAR) AS "availableStake"
FROM capacity;
