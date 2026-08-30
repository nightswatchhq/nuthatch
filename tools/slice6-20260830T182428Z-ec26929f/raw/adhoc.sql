SELECT count(*) FROM service__allocation_created
SELECT "indexer", count(*) AS n FROM service__allocation_created GROUP BY "indexer" ORDER BY n DESC LIMIT 20
SELECT * FROM service__allocation_created WHERE "indexer" = '0xdeadbeef' ORDER BY block_number DESC LIMIT 100
SELECT date_trunc('month', to_timestamp(block_timestamp)) AS m, count(*) FROM service__service_started GROUP BY 1 ORDER BY 1
SELECT a."indexer", SUM(r."tokensRewards_dec") FROM service__allocation_created a JOIN service__indexing_rewards_collected r ON r."indexer" = a."indexer" GROUP BY 1
SELECT DISTINCT "subgraphDeploymentId" FROM service__allocation_created
SELECT "indexer", SUM("tokensRewards_dec") AS t FROM service__indexing_rewards_collected GROUP BY 1 HAVING SUM("tokensRewards_dec") > 0 ORDER BY t DESC LIMIT 10
WITH r AS (SELECT "indexer", SUM("tokensRewards_dec") AS t FROM service__indexing_rewards_collected GROUP BY 1) SELECT * FROM r WHERE t > 1000
SELECT i.*, d.delegators FROM indexers i LEFT JOIN delegators_active d ON d."indexer" = i."indexer"
SELECT count(*) FROM service__allocation_created a WHERE EXISTS (SELECT 1 FROM service__allocation_closed c WHERE c."allocationId" = a."allocationId")
SELECT "indexer", min(block_number), max(block_number) FROM service__allocation_created GROUP BY 1
SELECT approx_count_distinct("indexer") FROM service__allocation_created
SELECT block_number, "indexer", SUM("tokensRewards_dec") OVER (PARTITION BY "indexer" ORDER BY block_number) AS running FROM service__indexing_rewards_collected
SELECT * FROM service__allocation_created a LEFT JOIN service__allocation_closed c USING ("allocationId") LIMIT 500
SELECT quantile_cont("tokensRewards_dec", 0.5) FROM service__indexing_rewards_collected
SELECT "indexer" FROM service__allocation_created INTERSECT SELECT "indexer" FROM service__allocation_closed
SELECT "indexer" FROM service__allocation_created EXCEPT SELECT "indexer" FROM service__allocation_closed
SELECT count(*) FROM (SELECT "indexer" FROM service__allocation_created UNION SELECT "indexer" FROM service__allocation_closed)
