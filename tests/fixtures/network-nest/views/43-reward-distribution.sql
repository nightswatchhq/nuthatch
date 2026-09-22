-- Legacy rewards use the ordered pool/cut fold. Horizon reports the split explicitly;
-- its pool balance changes through TokensToDelegationPoolAdded, not this reward event.
CREATE VIEW indexing_reward_distribution AS
SELECT d.indexer, r.allocation, r.block_number, r.log_index, true AS legacy,
       CAST(r.tokens AS BIGNUM) AS total, d.indexer_reward, d.delegator_reward
FROM allocation_reward_movement r JOIN delegation_ledger d
    ON d.block_number = r.block_number AND d.log_index = r.log_index
WHERE d.reward <> '0'
UNION ALL
SELECT indexer, "allocationId", block_number, log_index, false,
       CAST("tokensRewards" AS BIGNUM), CAST("tokensIndexerRewards" AS BIGNUM),
       CAST("tokensDelegationRewards" AS BIGNUM)
FROM subgraph_service__indexing_rewards_collected;

CREATE VIEW allocation_reward_distribution AS
SELECT allocation AS id, CAST(sum(indexer_reward) AS VARCHAR) AS "indexingIndexerRewards",
       CAST(sum(delegator_reward) AS VARCHAR) AS "indexingDelegatorRewards"
FROM indexing_reward_distribution GROUP BY allocation;

CREATE VIEW indexer_reward_distribution AS
SELECT indexer, CAST(sum(total) AS VARCHAR) AS "rewardsEarned",
       CAST(sum(indexer_reward) AS VARCHAR) AS "indexerIndexingRewards",
       CAST(sum(delegator_reward) AS VARCHAR) AS "delegatorIndexingRewards"
FROM indexing_reward_distribution GROUP BY indexer;

CREATE VIEW provision_reward_distribution AS
SELECT i.indexer, i.service,
       CAST(sum(CASE WHEN r.legacy THEN CAST(0 AS BIGNUM) ELSE r.total END) AS VARCHAR) AS "rewardsEarned",
       CAST(sum(r.indexer_reward) AS VARCHAR) AS "indexerIndexingRewards",
       CAST(sum(r.delegator_reward) AS VARCHAR) AS "delegatorIndexingRewards"
FROM provision_identity i JOIN indexing_reward_distribution r ON r.indexer = i.indexer
    AND i.service = '0xb2bb92d0de618878e438b55d5846cfecd9301105'
    AND (r.block_number > i.block_number OR
         (r.block_number = i.block_number AND r.log_index >= i.log_index))
GROUP BY i.indexer, i.service;
