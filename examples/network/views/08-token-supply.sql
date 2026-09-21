-- Self-transfers, including zero-to-zero, do not change the upstream supply ledger.
CREATE VIEW graph_token_supply AS
SELECT CAST(coalesce(sum(CASE WHEN "from" = '0x0000000000000000000000000000000000000000'
                                 AND "to" <> "from" THEN CAST(value AS BIGNUM)
                             WHEN "to" = '0x0000000000000000000000000000000000000000'
                                 AND "from" <> "to" THEN CAST(0 AS BIGNUM) - CAST(value AS BIGNUM)
                             ELSE CAST(0 AS BIGNUM) END), CAST(0 AS BIGNUM)) AS VARCHAR) AS "totalSupply",
       CAST(coalesce(sum(CASE WHEN "from" = '0x0000000000000000000000000000000000000000'
                                 AND "to" <> "from" THEN CAST(value AS BIGNUM)
                             ELSE CAST(0 AS BIGNUM) END), CAST(0 AS BIGNUM)) AS VARCHAR) AS "totalGRTMinted",
       CAST(coalesce(sum(CASE WHEN "to" = '0x0000000000000000000000000000000000000000'
                                 AND "from" <> "to" THEN CAST(value AS BIGNUM)
                             ELSE CAST(0 AS BIGNUM) END), CAST(0 AS BIGNUM)) AS VARCHAR) AS "totalGRTBurned"
FROM graph_token__transfer;
