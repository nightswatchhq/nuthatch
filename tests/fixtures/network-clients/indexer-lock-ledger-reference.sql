CREATE VIEW indexer_lock_ledger_reference AS
WITH RECURSIVE ordered AS MATERIALIZED (
    SELECT *, CAST(tokens AS BIGNUM) AS amount,
           row_number() OVER (PARTITION BY indexer ORDER BY block_number, log_index) AS seq
    FROM indexer_lock_event
), fold(indexer, seq, stake, allocated, locked, legacy_locked, until, legacy_until) AS (
    SELECT DISTINCT indexer, 0::BIGINT, CAST(0 AS BIGNUM), CAST(0 AS BIGNUM),
           CAST(0 AS BIGNUM), CAST(0 AS BIGNUM), 0, 0 FROM ordered
    UNION ALL
    SELECT f.indexer, e.seq,
        f.stake + CASE WHEN e.kind = 'deposit' THEN e.amount
            WHEN e.kind IN ('legacy_withdraw', 'withdraw', 'slash', 'provision_slash') THEN -e.amount
            ELSE CAST(0 AS BIGNUM) END,
        f.allocated + CASE WHEN e.kind = 'allocate' THEN e.amount WHEN e.kind = 'close' THEN -e.amount
            ELSE CAST(0 AS BIGNUM) END,
        next_lock.locked, next_lock.legacy_locked,
        CASE WHEN e.kind IN ('legacy_lock', 'lock') THEN e.until
             WHEN e.kind IN ('legacy_withdraw', 'withdraw') THEN 0
             WHEN e.kind = 'slash' AND next_lock.locked = CAST(0 AS BIGNUM) THEN 0 ELSE f.until END,
        CASE WHEN e.kind = 'legacy_lock' THEN e.until WHEN e.kind = 'legacy_withdraw' THEN 0
             WHEN e.kind = 'slash' AND next_lock.legacy_locked = CAST(0 AS BIGNUM) THEN 0 ELSE f.legacy_until END
    FROM fold f JOIN ordered e ON e.indexer = f.indexer AND e.seq = f.seq + 1
    CROSS JOIN LATERAL (
        SELECT CASE WHEN e.kind = 'slash' AND e.amount > CAST(0 AS BIGNUM) THEN
            least(f.legacy_locked, greatest(CAST(0 AS BIGNUM), e.amount -
                greatest(CAST(0 AS BIGNUM), f.stake - f.allocated - f.legacy_locked)))
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
SELECT f.*, e.block_number, e.log_index FROM fold f
JOIN ordered e ON e.indexer = f.indexer AND e.seq = f.seq;
