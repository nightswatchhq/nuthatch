CREATE VIEW deployment_creation AS
SELECT id, block_number, log_index, block_timestamp FROM (
    SELECT deployment AS id, block_number, log_index, block_timestamp FROM gns_version_creation
    UNION ALL SELECT deployment, block_number, log_index, block_timestamp FROM allocation_creation
    UNION ALL SELECT "subgraphDeploymentID", block_number, log_index, block_timestamp FROM curation__signalled
) QUALIFY row_number() OVER (PARTITION BY id ORDER BY block_number, log_index) = 1;

CREATE VIEW subgraph_deployment AS
SELECT d.id, nuthatch_cid_v0(d.id) AS "ipfsHash", CAST(d.block_timestamp AS INTEGER) AS "createdAt",
       coalesce((SELECT CAST(r."sinceBlock" AS INTEGER) FROM rewards__rewards_denylist_updated r
           WHERE r."subgraphDeploymentID" = d.id AND
             (r.block_number > d.block_number OR (r.block_number = d.block_number AND r.log_index > d.log_index))
           ORDER BY r.block_number DESC, r.log_index DESC LIMIT 1), 0) AS "deniedAt",
       coalesce(f."stakedTokens", '0') AS "stakedTokens",
       coalesce(f."signalledTokens", '0') AS "signalledTokens",
       coalesce(f."signalAmount", '0') AS "signalAmount",
       coalesce(f."queryFeesAmount", '0') AS "queryFeesAmount",
       coalesce(f."curatorFeeRewards", '0') AS "curatorFeeRewards",
       coalesce(f."indexingRewardAmount", '0') AS "indexingRewardAmount",
       CAST(NULL AS VARCHAR) AS "originalName"
FROM deployment_creation d LEFT JOIN deployment_financials f ON f.id = d.id;
