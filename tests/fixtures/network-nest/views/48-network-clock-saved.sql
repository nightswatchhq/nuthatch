-- GraphToken handlers read the clock on every event, but only mint/burn saves
-- that GraphNetwork object. Ordinary transfers, self-transfers and approvals
-- persist it only through createOrLoadEpoch when a new epoch is created.
-- POIPresented likewise saves the allocation, not the refreshed network object.
-- Delegation parameter changes only save GraphNetwork through a new indexer
-- counter or a newly created epoch, not when updating an existing indexer.
CREATE VIEW network_persisted_l1_observation AS
SELECT o.block_number, o.log_index, o.l1_block
FROM network_l1_observation o
WHERE NOT EXISTS (
    SELECT 1 FROM graph_token__approval a
    WHERE a.block_number = o.block_number AND a.log_index = o.log_index
) AND NOT EXISTS (
    SELECT 1 FROM subgraph_service__p_o_i_presented p
    WHERE p.block_number = o.block_number AND p.log_index = o.log_index
) AND NOT EXISTS (
    SELECT 1 FROM staking_legacy__delegation_parameters_updated p
    WHERE p.block_number = o.block_number AND p.log_index = o.log_index
      AND EXISTS (
          SELECT 1 FROM indexer_identity_event i WHERE i.indexer = p.indexer
          AND (i.block_number < p.block_number OR
               (i.block_number = p.block_number AND i.log_index < p.log_index))
      )
) AND NOT EXISTS (
    SELECT 1 FROM graph_token__transfer t
    WHERE t.block_number = o.block_number AND t.log_index = o.log_index
      AND (t."from" = t."to" OR
           (t."from" <> '0x0000000000000000000000000000000000000000'
            AND t."to" <> '0x0000000000000000000000000000000000000000'))
)
UNION ALL
SELECT block_number, log_index, l1_block FROM (
    SELECT block_number, log_index, l1_block,
           row_number() OVER (PARTITION BY epoch ORDER BY block_number, log_index) AS ordinal
    FROM network_epoch_observation
) first_epoch WHERE ordinal = 1
UNION ALL
SELECT block_number, log_index, l1_block FROM (
    SELECT block_number, log_index, l1_block FROM network_l1_observation
    ORDER BY block_number, log_index LIMIT 1
) initial_network;
