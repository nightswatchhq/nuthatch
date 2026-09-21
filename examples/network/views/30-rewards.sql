CREATE VIEW deployment_denial AS
SELECT "subgraphDeploymentID" AS id, CAST("sinceBlock" AS INTEGER) AS "deniedAt"
FROM rewards__rewards_denylist_updated
QUALIFY row_number() OVER (PARTITION BY "subgraphDeploymentID" ORDER BY block_number DESC, log_index DESC) = 1;

CREATE VIEW issuance_parameter_read AS
SELECT p.block_number, p.log_index,
       CASE WHEN a.reverted = false THEN a.result
            WHEN a.reverted = true AND l.reverted = false THEN l.result
            ELSE error('missing or reverted pinned issuance read') END AS word
FROM rewards__parameter_updated p
LEFT JOIN (SELECT DISTINCT block_number, block_hash, result, reverted FROM allocated_issuance_read) a
ON a.block_number = p.block_number AND a.block_hash = p.block_hash
LEFT JOIN (SELECT DISTINCT block_number, block_hash, result, reverted FROM legacy_issuance_read) l
ON l.block_number = p.block_number AND l.block_hash = p.block_hash
WHERE p.param = 'issuancePerBlock';

CREATE VIEW network_issuance_update AS
SELECT block_number, log_index,
       nuthatch_uint256(word) AS rate
FROM issuance_parameter_read
UNION ALL
SELECT block_number, log_index, "newSelfMintingRate"
FROM issuance_allocator__target_allocation_updated
WHERE target = '0x971b9d3d0ae3eca029cab5ea1fb0f72c85e6a525';

CREATE VIEW network_issuance AS
SELECT rate AS "networkGRTIssuancePerBlock" FROM network_issuance_update
ORDER BY block_number DESC, log_index DESC LIMIT 1;
