CREATE VIEW indexer_stake_movement AS
SELECT indexer, block_number, log_index, block_timestamp, CAST(tokens AS BIGNUM) AS tokens
FROM staking_legacy__stake_deposited
UNION ALL
SELECT indexer, block_number, log_index, block_timestamp, CAST(0 AS BIGNUM) - CAST(tokens AS BIGNUM)
FROM staking_legacy__stake_withdrawn
UNION ALL
SELECT indexer, block_number, log_index, block_timestamp, CAST(0 AS BIGNUM) - CAST(tokens AS BIGNUM)
FROM staking_legacy__stake_slashed
UNION ALL
SELECT "serviceProvider", block_number, log_index, block_timestamp, CAST(tokens AS BIGNUM)
FROM horizon_staking__horizon_stake_deposited
UNION ALL
SELECT "serviceProvider", block_number, log_index, block_timestamp, CAST(0 AS BIGNUM) - CAST(tokens AS BIGNUM)
FROM horizon_staking__horizon_stake_withdrawn
UNION ALL
SELECT "serviceProvider", block_number, log_index, block_timestamp, CAST(0 AS BIGNUM) - CAST(tokens AS BIGNUM)
FROM horizon_staking__provision_slashed;

CREATE VIEW indexer_identity_event AS
SELECT indexer, block_number, log_index, block_timestamp, true AS legacy FROM staking_legacy__stake_deposited
UNION ALL
SELECT indexer, block_number, log_index, block_timestamp, true FROM staking_legacy__stake_delegated
UNION ALL
SELECT indexer, block_number, log_index, block_timestamp, true FROM staking_legacy__delegation_parameters_updated
UNION ALL
SELECT indexer, block_number, log_index, block_timestamp, true FROM service_registry__service_registered
UNION ALL
SELECT indexer, block_number, log_index, block_timestamp, false FROM horizon_registration WHERE registration IS NOT NULL
UNION ALL
SELECT "serviceProvider", block_number, log_index, block_timestamp, NULL FROM horizon_staking__horizon_stake_deposited
UNION ALL
SELECT "serviceProvider", block_number, log_index, block_timestamp, NULL FROM horizon_staking__tokens_delegated
UNION ALL
SELECT "serviceProvider", block_number, log_index, block_timestamp, NULL FROM horizon_staking__delegation_fee_cut_set
UNION ALL
SELECT indexer, block_number, log_index, block_timestamp, NULL FROM subgraph_service__rewards_destination_set;

-- Legacy slashing may release locked tokens, depending on allocated stake at
-- that exact log. Horizon withdrawal clears the general lock, not the legacy one.
CREATE VIEW indexer_lock_event AS
SELECT indexer, block_number, log_index, 'deposit' AS kind, tokens, 0 AS until
FROM staking_legacy__stake_deposited
UNION ALL SELECT "serviceProvider", block_number, log_index, 'deposit', tokens, 0
FROM horizon_staking__horizon_stake_deposited
UNION ALL SELECT indexer, block_number, log_index, 'legacy_withdraw', tokens, 0
FROM staking_legacy__stake_withdrawn
UNION ALL SELECT "serviceProvider", block_number, log_index, 'withdraw', tokens, 0
FROM horizon_staking__horizon_stake_withdrawn
UNION ALL SELECT indexer, block_number, log_index, 'slash', tokens, 0
FROM staking_legacy__stake_slashed
UNION ALL SELECT "serviceProvider", block_number, log_index, 'provision_slash', tokens, 0
FROM horizon_staking__provision_slashed
UNION ALL SELECT indexer, block_number, log_index, 'legacy_lock', tokens, CAST(until AS INTEGER)
FROM staking_legacy__stake_locked
UNION ALL SELECT "serviceProvider", block_number, log_index, 'lock', tokens, CAST(until AS INTEGER)
FROM horizon_staking__horizon_stake_locked
UNION ALL SELECT indexer, block_number, log_index, 'allocate', tokens, 0
FROM staking_legacy__allocation_created
UNION ALL SELECT indexer, block_number, log_index, 'close', tokens, 0
FROM staking_legacy__allocation_closed_f672
UNION ALL SELECT indexer, block_number, log_index, 'close', tokens, 0
FROM staking_legacy__allocation_closed_7203;

CREATE VIEW indexer_lock_ledger AS
WITH RECURSIVE ordered AS MATERIALIZED (
    SELECT *, CAST(tokens AS BIGNUM) AS amount,
           row_number() OVER w AS seq,
           sum(CASE WHEN kind = 'deposit' THEN CAST(tokens AS BIGNUM)
               WHEN kind IN ('legacy_withdraw', 'withdraw', 'slash', 'provision_slash') THEN -CAST(tokens AS BIGNUM)
               ELSE CAST(0 AS BIGNUM) END) OVER w AS stake,
           sum(CASE WHEN kind = 'allocate' THEN CAST(tokens AS BIGNUM)
               WHEN kind = 'close' THEN -CAST(tokens AS BIGNUM)
               ELSE CAST(0 AS BIGNUM) END) OVER w AS allocated,
           sum(CASE WHEN kind IN ('legacy_lock', 'lock', 'legacy_withdraw', 'withdraw', 'slash')
               THEN 1 ELSE 0 END) OVER w AS lock_seq
    FROM indexer_lock_event
    WINDOW w AS (PARTITION BY indexer ORDER BY block_number, log_index
                 ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)
), lock_events AS MATERIALIZED (
    SELECT * FROM ordered
    WHERE kind IN ('legacy_lock', 'lock', 'legacy_withdraw', 'withdraw', 'slash')
), fold(indexer, lock_seq, locked, legacy_locked, until, legacy_until) AS (
    SELECT DISTINCT indexer, 0::HUGEINT, CAST(0 AS BIGNUM), CAST(0 AS BIGNUM), 0, 0 FROM ordered
    UNION ALL
    SELECT f.indexer, e.lock_seq, next_lock.locked, next_lock.legacy_locked,
        CASE WHEN e.kind IN ('legacy_lock', 'lock') THEN e.until
             WHEN e.kind IN ('legacy_withdraw', 'withdraw') THEN 0
             WHEN e.kind = 'slash' AND next_lock.locked = CAST(0 AS BIGNUM) THEN 0 ELSE f.until END,
        CASE WHEN e.kind = 'legacy_lock' THEN e.until WHEN e.kind = 'legacy_withdraw' THEN 0
             WHEN e.kind = 'slash' AND next_lock.legacy_locked = CAST(0 AS BIGNUM) THEN 0 ELSE f.legacy_until END
    FROM fold f JOIN lock_events e ON e.indexer = f.indexer AND e.lock_seq = f.lock_seq + 1
    CROSS JOIN LATERAL (
        -- The slash row's stake is post-slash. Restore its amount to obtain
        -- the pre-event stake used by the mapping's locked-token calculation.
        SELECT CASE WHEN e.kind = 'slash' AND e.amount > CAST(0 AS BIGNUM) THEN
            least(f.legacy_locked, greatest(CAST(0 AS BIGNUM), e.amount -
                greatest(CAST(0 AS BIGNUM), e.stake + e.amount - e.allocated - f.legacy_locked)))
            ELSE CAST(0 AS BIGNUM) END AS unlock
    ) slash
    CROSS JOIN LATERAL (
        SELECT CASE WHEN e.kind IN ('legacy_lock', 'lock') THEN e.amount
                    WHEN e.kind = 'legacy_withdraw' THEN f.locked - e.amount
                    WHEN e.kind = 'withdraw' THEN CAST(0 AS BIGNUM)
                    ELSE f.locked - slash.unlock END AS locked,
               CASE WHEN e.kind = 'legacy_lock' THEN e.amount
                    WHEN e.kind = 'legacy_withdraw' THEN f.legacy_locked - e.amount
                    ELSE f.legacy_locked - slash.unlock END AS legacy_locked
    ) next_lock
)
SELECT e.indexer, e.seq, e.stake, e.allocated, f.locked, f.legacy_locked,
       f.until, f.legacy_until, e.block_number, e.log_index
FROM ordered e JOIN fold f ON f.indexer = e.indexer AND f.lock_seq = e.lock_seq;

CREATE VIEW indexer_lock_state AS
SELECT indexer, CAST(locked AS VARCHAR) AS "lockedTokens", until AS "tokensLockedUntil",
       CAST(legacy_locked AS VARCHAR) AS "legacyLockedTokens", legacy_until AS "legacyTokensLockedUntil"
FROM indexer_lock_ledger QUALIFY row_number() OVER (PARTITION BY indexer ORDER BY seq DESC) = 1;
