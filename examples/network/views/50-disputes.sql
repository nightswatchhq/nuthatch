-- Dispute lifecycle, including the implicit decision on a linked conflicting dispute.
CREATE VIEW dispute_creation AS
SELECT e."disputeID" AS id, e.indexer, e.fisherman,
       e."subgraphDeploymentID" AS deployment,
       NULL AS allocation,
       e."tokens" AS deposit, '0' AS cancellable,
       true AS legacy, 'SingleQuery' AS kind, e.block_number, e.log_index, e.block_timestamp
FROM dispute_legacy__query_dispute_created e
UNION ALL
SELECT e."disputeID" AS id, e.indexer, e.fisherman,
       coalesce(a.deployment, error('dispute refers to an unavailable allocation')) AS deployment,
       e."allocationID" AS allocation,
       e."tokens" AS deposit, '0' AS cancellable,
       true AS legacy, 'Indexing' AS kind, e.block_number, e.log_index, e.block_timestamp
FROM dispute_legacy__indexing_dispute_created e
LEFT JOIN allocation_creation a ON a.id = e."allocationID"
UNION ALL
SELECT e."disputeId" AS id, e.indexer, e.fisherman,
       e."subgraphDeploymentId" AS deployment,
       NULL AS allocation,
       e."tokens" AS deposit, "cancellableAt" AS cancellable,
       false AS legacy, 'SingleQuery' AS kind, e.block_number, e.log_index, e.block_timestamp
FROM dispute_horizon__query_dispute_created e
UNION ALL
SELECT e."disputeId" AS id, e.indexer, e.fisherman,
       coalesce(a.deployment, error('dispute refers to an unavailable allocation')) AS deployment,
       e."allocationId" AS allocation,
       e."tokens" AS deposit, "cancellableAt" AS cancellable,
       false AS legacy, 'Indexing' AS kind, e.block_number, e.log_index, e.block_timestamp
FROM dispute_horizon__indexing_dispute_created e
LEFT JOIN allocation_creation a ON a.id = e."allocationId"
UNION ALL
SELECT e."disputeId" AS id, e.indexer, e.fisherman,
       coalesce(a.deployment, error('dispute refers to an unavailable allocation')) AS deployment,
       e."allocationId" AS allocation,
       '0' AS deposit, '0' AS cancellable,
       true AS legacy, 'Legacy' AS kind, e.block_number, e.log_index, e.block_timestamp
FROM dispute_horizon__legacy_dispute_created e
LEFT JOIN allocation_creation a ON a.id = e."allocationId";

CREATE VIEW dispute_link AS
SELECT "disputeID1" AS id, "disputeID2" AS partner, block_number, log_index
FROM dispute_legacy__dispute_linked
UNION ALL
SELECT "disputeID2" AS id, "disputeID1" AS partner, block_number, log_index
FROM dispute_legacy__dispute_linked
UNION ALL
SELECT "disputeId1" AS id, "disputeId2" AS partner, block_number, log_index
FROM dispute_horizon__dispute_linked
UNION ALL
SELECT "disputeId2" AS id, "disputeId1" AS partner, block_number, log_index
FROM dispute_horizon__dispute_linked;

CREATE VIEW dispute_direct_decision AS
SELECT "disputeID" AS id, 'Accepted' AS status, tokens, block_number, log_index, block_timestamp
FROM dispute_legacy__dispute_accepted
UNION ALL
SELECT "disputeID" AS id, 'Rejected' AS status, tokens, block_number, log_index, block_timestamp
FROM dispute_legacy__dispute_rejected
UNION ALL
SELECT "disputeID" AS id, 'Draw' AS status, tokens, block_number, log_index, block_timestamp
FROM dispute_legacy__dispute_drawn
UNION ALL
SELECT "disputeId" AS id, 'Accepted' AS status, tokens, block_number, log_index, block_timestamp
FROM dispute_horizon__dispute_accepted
UNION ALL
SELECT "disputeId" AS id, 'Rejected' AS status, tokens, block_number, log_index, block_timestamp
FROM dispute_horizon__dispute_rejected
UNION ALL
SELECT "disputeId" AS id, 'Draw' AS status, tokens, block_number, log_index, block_timestamp
FROM dispute_horizon__dispute_drawn
UNION ALL
SELECT "disputeId" AS id, 'Cancelled' AS status, tokens, block_number, log_index, block_timestamp
FROM dispute_horizon__dispute_cancelled;

CREATE VIEW dispute_decision AS
SELECT * FROM dispute_direct_decision
UNION ALL
SELECT l.partner AS id, CASE WHEN d.status = 'Accepted' THEN 'Rejected' ELSE 'Draw' END AS status,
       '0' AS tokens, d.block_number, d.log_index, d.block_timestamp
FROM dispute_direct_decision d
JOIN LATERAL (
    SELECT partner FROM dispute_link l WHERE l.id = d.id AND
        (l.block_number < d.block_number OR (l.block_number = d.block_number AND l.log_index < d.log_index))
    ORDER BY l.block_number DESC, l.log_index DESC LIMIT 1
) l ON true
WHERE d.status IN ('Accepted', 'Draw');

CREATE VIEW dispute AS
WITH creations AS (
    SELECT * FROM dispute_creation
    QUALIFY row_number() OVER (PARTITION BY id ORDER BY block_number DESC, log_index DESC) = 1
)
SELECT c.id, c.indexer, c.fisherman, c.deployment AS "subgraphDeployment", c.allocation, c.deposit,
       c.legacy AS "isLegacy", CAST(c.block_timestamp AS INTEGER) AS "createdAt",
       CAST(coalesce(d.block_timestamp, 0) AS INTEGER) AS "closedAt", c.cancellable AS "cancellableAt",
       coalesce(d.status, 'Undecided') AS status,
       CASE WHEN l.partner IS NOT NULL THEN 'Conflicting' ELSE c.kind END AS type,
       l.partner AS "linkedDispute",
       CASE WHEN r.tokens IS NULL THEN '0' ELSE CAST(CAST(r.tokens AS BIGNUM) - CAST(c.deposit AS BIGNUM) AS VARCHAR) END AS "tokensRewarded"
FROM creations c
LEFT JOIN LATERAL (
    SELECT * FROM dispute_decision d WHERE d.id = c.id AND
        (d.block_number > c.block_number OR (d.block_number = c.block_number AND d.log_index > c.log_index))
    ORDER BY d.block_number DESC, d.log_index DESC LIMIT 1
) d ON true
LEFT JOIN LATERAL (
    SELECT partner FROM dispute_link l WHERE l.id = c.id AND
        (l.block_number > c.block_number OR (l.block_number = c.block_number AND l.log_index > c.log_index))
    ORDER BY l.block_number DESC, l.log_index DESC LIMIT 1
) l ON true
LEFT JOIN LATERAL (
    SELECT tokens FROM dispute_direct_decision r WHERE r.id = c.id AND r.status = 'Accepted' AND
        (r.block_number > c.block_number OR (r.block_number = c.block_number AND r.log_index > c.log_index))
    ORDER BY r.block_number DESC, r.log_index DESC LIMIT 1
) r ON true;

CREATE VIEW graph_account AS
SELECT "from" AS id FROM graph_token__transfer
UNION SELECT "to" AS id FROM graph_token__transfer
UNION SELECT owner AS id FROM graph_token__approval
UNION SELECT indexer AS id FROM dispute_creation
UNION SELECT fisherman AS id FROM dispute_creation;
