-- The largest holders of the vault's shares.
CREATE VIEW top_holders AS
SELECT "to" AS holder, sum(value_dec) AS shares
FROM vault__transfer
GROUP BY 1
ORDER BY shares DESC
LIMIT 10;
