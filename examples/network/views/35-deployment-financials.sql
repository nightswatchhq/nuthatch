CREATE VIEW deployment_signal_movement AS
SELECT "subgraphDeploymentID" AS deployment, block_number, log_index, block_timestamp,
       CAST(tokens AS BIGNUM) - CAST("curationTax" AS BIGNUM) AS tokens,
       CAST(signal AS BIGNUM) AS signal
FROM curation__signalled
UNION ALL
SELECT "subgraphDeploymentID", block_number, log_index, block_timestamp,
       CAST(0 AS BIGNUM) - CAST(tokens AS BIGNUM), CAST(0 AS BIGNUM) - CAST(signal AS BIGNUM)
FROM curation__burned
UNION ALL
SELECT deployment, block_number, log_index, block_timestamp, curator, CAST(0 AS BIGNUM)
FROM allocation_query_fee_movement;

-- Publishing-only deployments are joined by the GNS projection. This financial projection
-- deliberately does not claim to enumerate every deployment in the network.
CREATE VIEW deployment_financials AS
WITH ids AS (
    SELECT deployment FROM allocation_creation
    UNION SELECT deployment FROM deployment_signal_movement
), stake AS (
    SELECT "subgraphDeployment" AS deployment, sum(CAST("allocatedTokens" AS BIGNUM)) AS tokens
    FROM allocation_lifecycle WHERE status = 'Active' GROUP BY "subgraphDeployment"
), signal AS (
    SELECT deployment, sum(tokens) AS tokens, sum(signal) AS signal
    FROM deployment_signal_movement GROUP BY deployment
), fees AS (
    SELECT deployment, sum(fees) AS fees, sum(curator) AS curator
    FROM allocation_query_fee_movement GROUP BY deployment
), rewards AS (
    SELECT a.deployment, sum(CAST(r.tokens AS BIGNUM)) AS tokens
    FROM allocation_reward_movement r JOIN allocation_creation a ON a.id = r.allocation
    GROUP BY a.deployment
)
SELECT i.deployment AS id, nuthatch_cid_v0(i.deployment) AS "ipfsHash",
       CAST(coalesce(s.tokens, CAST(0 AS BIGNUM)) AS VARCHAR) AS "stakedTokens",
       CAST(coalesce(c.tokens, CAST(0 AS BIGNUM)) AS VARCHAR) AS "signalledTokens",
       CAST(coalesce(c.signal, CAST(0 AS BIGNUM)) AS VARCHAR) AS "signalAmount",
       CAST(coalesce(f.fees, CAST(0 AS BIGNUM)) AS VARCHAR) AS "queryFeesAmount",
       CAST(coalesce(f.curator, CAST(0 AS BIGNUM)) AS VARCHAR) AS "curatorFeeRewards",
       CAST(coalesce(r.tokens, CAST(0 AS BIGNUM)) AS VARCHAR) AS "indexingRewardAmount"
FROM ids i
LEFT JOIN stake s ON s.deployment = i.deployment
LEFT JOIN signal c ON c.deployment = i.deployment
LEFT JOIN fees f ON f.deployment = i.deployment
LEFT JOIN rewards r ON r.deployment = i.deployment;
