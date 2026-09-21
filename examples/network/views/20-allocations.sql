-- Lifecycle fields only. Rewards and deployment/indexer aggregates are separate folds.
CREATE VIEW allocation_creation AS
SELECT "allocationId" AS id, indexer, "subgraphDeploymentId" AS deployment, tokens,
       CAST("currentEpoch" AS INTEGER) AS epoch, block_number, block_hash, block_timestamp, log_index, false AS legacy
FROM subgraph_service__allocation_created
UNION ALL
SELECT "allocationID", indexer, "subgraphDeploymentID", tokens, CAST(epoch AS INTEGER),
       block_number, block_hash, block_timestamp, log_index, true FROM staking_legacy__allocation_created;

CREATE VIEW allocation_closure AS
SELECT "allocationID" AS id, CAST(epoch AS INTEGER) AS epoch, block_number, block_hash,
       block_timestamp, poi, NULL::BOOLEAN AS force_closed, '0' AS effective
FROM staking_legacy__allocation_closed_f672
UNION ALL
SELECT "allocationID", CAST(epoch AS INTEGER), block_number, block_hash,
       block_timestamp, poi, NULL::BOOLEAN, "effectiveAllocation"
FROM staking_legacy__allocation_closed_7203
UNION ALL
SELECT c."allocationId",
       CASE WHEN e.result IS NULL OR e.reverted OR length(e.result) != 66
            THEN error('missing pinned EpochManager.currentEpoch at allocation closure')
            ELSE CAST(('0x' || coalesce(nullif(ltrim(substr(e.result, 3), '0'), ''), '0')) AS INTEGER) END,
       c.block_number, c.block_hash, c.block_timestamp, NULL, CAST(c."forceClosed" AS BOOLEAN), '0'
FROM subgraph_service__allocation_closed c
LEFT JOIN (SELECT DISTINCT block_number, block_hash, result, reverted FROM allocation_close_epoch) e
ON e.block_number = c.block_number AND e.block_hash = c.block_hash;

CREATE VIEW allocation_poi AS
SELECT "allocationId" AS id, poi, block_number, log_index FROM subgraph_service__p_o_i_presented
UNION ALL
-- Once POIPresented has set the allocation's condition, reward collection no
-- longer owns its latest POI, including later transactions without a presentation.
SELECT r."allocationId", r.poi, r.block_number, r.log_index
FROM subgraph_service__indexing_rewards_collected r
WHERE NOT EXISTS (
    SELECT 1 FROM subgraph_service__p_o_i_presented p WHERE p."allocationId" = r."allocationId"
    AND (p.block_number < r.block_number OR (p.block_number = r.block_number AND p.log_index < r.log_index))
);

-- Arbitrum allocation block-number fields are the mapping's L1 clock, while
-- their block hashes and the historical filter remain on the L2 chain.
CREATE VIEW allocation_l1_clock AS
SELECT DISTINCT block_number, block_hash,
       -- Normalize the upstream pre-deployment/revert fallback before decoding.
       -- Guarding only the result expression failed on the larger cold replay.
       CAST(nuthatch_uint256(CASE WHEN reverted OR result = '0x'
            THEN '0x0000000000000000000000000000000000000000000000000000000000000000'
            ELSE result END) AS INTEGER) AS l1_block
FROM epoch_manager_l1_block;

CREATE VIEW allocation_lifecycle AS
WITH resize AS (
    SELECT *, row_number() OVER (PARTITION BY "allocationId" ORDER BY block_number DESC, log_index DESC) AS rank
    FROM subgraph_service__allocation_resized
), latest_poi AS (
    SELECT *, row_number() OVER (PARTITION BY id ORDER BY block_number DESC, log_index DESC) AS rank
    FROM allocation_poi
)
SELECT a.id, a.indexer, a.deployment AS "subgraphDeployment", a.legacy AS "isLegacy",
       CASE WHEN c.id IS NULL THEN a.indexer ELSE NULL END AS "activeForIndexer",
       coalesce(r."newTokens", a.tokens) AS "allocatedTokens",
       a.epoch AS "createdAtEpoch", a.block_hash AS "createdAtBlockHash",
       CASE WHEN ca.block_number IS NULL THEN error('missing pinned EpochManager.blockNum at allocation creation')
            ELSE ca.l1_block END AS "createdAtBlockNumber",
       CAST(a.block_timestamp AS INTEGER) AS "createdAt",
       c.epoch AS "closedAtEpoch", c.block_hash AS "closedAtBlockHash",
       CASE WHEN c.id IS NULL THEN NULL
            WHEN cc.block_number IS NULL THEN error('missing pinned EpochManager.blockNum at allocation closure')
            ELSE cc.l1_block END AS "closedAtBlockNumber",
       CAST(c.block_timestamp AS INTEGER) AS "closedAt",
       CASE WHEN c.id IS NULL THEN 'Active' ELSE 'Closed' END AS status,
       CASE WHEN a.legacy THEN c.poi ELSE p.poi END AS poi,
       c.force_closed AS "forceClosed", coalesce(c.effective, '0') AS "effectiveAllocation"
FROM allocation_creation a
LEFT JOIN allocation_closure c ON c.id = a.id
LEFT JOIN allocation_l1_clock ca ON ca.block_number = a.block_number AND ca.block_hash = a.block_hash
LEFT JOIN allocation_l1_clock cc ON cc.block_number = c.block_number AND cc.block_hash = c.block_hash
LEFT JOIN resize r ON r."allocationId" = a.id AND r.rank = 1
LEFT JOIN latest_poi p ON p.id = a.id AND p.rank = 1;
