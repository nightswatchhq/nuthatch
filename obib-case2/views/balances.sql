-- Recipe: balances (RFC-0023 tier 1) - each address's current token balance, DERIVED from
-- the Transfer events already indexed as Σ(in) − Σ(out). No eth_call `balanceOf` per address.
-- Query it: SELECT * FROM lbtc_balances ORDER BY balance DESC
CREATE VIEW lbtc_balances AS
SELECT addr, balance FROM (SELECT addr, SUM(d) AS balance FROM (SELECT lower("to") AS addr, TRY_CAST("value" AS HUGEINT) AS d FROM "lbtc__transfer" UNION ALL SELECT lower("from") AS addr, -TRY_CAST("value" AS HUGEINT) AS d FROM "lbtc__transfer") GROUP BY addr) WHERE addr <> '0x0000000000000000000000000000000000000000' AND balance <> 0;
