-- The first VersionUpdated following Published fills metadata on the already-created
-- version. Counting both logs as versions shifts every subsequent version ID.
CREATE VIEW gns_version_event AS
SELECT nuthatch_base58_uint256("subgraphID") AS subgraph, "subgraphDeploymentID" AS deployment,
       block_number, log_index, block_timestamp, 'publish' AS kind
FROM gns__subgraph_published_42a5
UNION ALL
SELECT nuthatch_base58_uint256("subgraphID"), "subgraphDeploymentID",
       block_number, log_index, block_timestamp, 'update'
FROM gns__subgraph_version_updated
UNION ALL
SELECT nuthatch_base58_uint256(nuthatch_uint256(nuthatch_keccak256(
           "graphAccount" || substr(nuthatch_uint256_word("subgraphNumber"), 3)))),
       "subgraphDeploymentID", block_number, log_index, block_timestamp, 'legacy'
FROM gns__subgraph_published_3e8d;

CREATE VIEW gns_version_creation AS
WITH ordered AS (
    SELECT *, lag(kind) OVER (PARTITION BY subgraph ORDER BY block_number, log_index) AS previous
    FROM gns_version_event
)
SELECT *, CAST(row_number() OVER (PARTITION BY subgraph ORDER BY block_number, log_index) - 1 AS INTEGER) AS version
FROM ordered WHERE kind != 'update' OR previous IS DISTINCT FROM 'publish';

CREATE VIEW subgraph_version AS
SELECT subgraph || '-' || CAST(version AS VARCHAR) AS id, subgraph,
       deployment AS "subgraphDeployment", version, CAST(block_timestamp AS INTEGER) AS "createdAt"
FROM gns_version_creation;

CREATE VIEW subgraph AS
WITH identities AS (
    SELECT subgraph AS id, block_timestamp FROM gns_version_creation
    UNION ALL SELECT nuthatch_base58_uint256("_l2SubgraphID"), block_timestamp
    FROM gns__subgraph_received_from_l1
), versions AS (
    SELECT subgraph, count(*) AS count FROM subgraph_version GROUP BY subgraph
)
SELECT i.id, CAST(min(i.block_timestamp) AS INTEGER) AS "createdAt",
       CAST(coalesce(v.count, 0) AS VARCHAR) AS "versionCount"
FROM identities i LEFT JOIN versions v ON v.subgraph = i.id GROUP BY i.id, v.count;
