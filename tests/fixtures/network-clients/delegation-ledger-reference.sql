CREATE VIEW delegation_ledger_reference AS
WITH RECURSIVE ordered AS MATERIALIZED (
    SELECT * FROM delegation_ordered
), ledger(indexer, seq, tokens, delegator_reward, shares, thawing) AS (
    SELECT DISTINCT indexer, CAST(0 AS BIGINT), CAST(0 AS BIGNUM), CAST(0 AS BIGNUM), CAST(0 AS BIGNUM), CAST(0 AS BIGNUM)
    FROM ordered
    UNION ALL
    SELECT l.indexer, e.seq, l.tokens + e.delta + r.tokens, r.tokens,
           l.shares + e.shares, l.thawing + e.thawing
    FROM ledger l JOIN ordered e ON e.indexer = l.indexer AND e.seq = l.seq + 1
    CROSS JOIN LATERAL (
        SELECT CASE WHEN l.tokens = CAST(0 AS BIGNUM) THEN CAST(0 AS BIGNUM)
                    ELSE CAST(e.reward AS BIGNUM) - CAST(nuthatch_mul_div(e.reward, e.cut, '1000000') AS BIGNUM) END AS tokens
    ) r
)
SELECT e.indexer, e.block_number, e.log_index, e.block_timestamp, e.reward,
       l.tokens AS delegated, l.delegator_reward, CAST(e.reward AS BIGNUM) - l.delegator_reward AS indexer_reward,
       l.shares, l.thawing
FROM ledger l JOIN ordered e ON e.indexer = l.indexer AND e.seq = l.seq;
