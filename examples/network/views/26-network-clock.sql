-- Mapping refresh points, in receipt-log order. L1 coordinates come from a
-- pinned EpochManager call; neither the Arbitrum height nor wall time is a clock.
CREATE VIEW network_refresh_event AS
SELECT block_number, block_hash, log_index, false AS length_update
FROM issuance_allocator__target_allocation_updated
WHERE target = '0x971b9d3d0ae3eca029cab5ea1fb0f72c85e6a525'
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM l2_gateway__withdrawal_initiated
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM l2_gateway__deposit_finalized
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM graph_payments__graph_payment_collected
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM dispute_legacy__parameter_updated
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM dispute_horizon__arbitrator_set
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM dispute_horizon__dispute_accepted
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM dispute_horizon__dispute_period_set
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM dispute_horizon__fisherman_reward_cut_set
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM dispute_horizon__max_slashing_cut_set
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM graph_token__transfer
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM graph_token__approval
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM controller__set_contract_proxy
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM controller__new_ownership
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM controller__partial_pause_changed
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM controller__pause_changed
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM controller__new_pause_guardian
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM epoch_manager__epoch_run
UNION ALL
SELECT block_number, block_hash, log_index, true AS length_update FROM epoch_manager__epoch_length_update
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM subgraph_service__allocation_closed
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM subgraph_service__allocation_created
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM subgraph_service__indexing_rewards_collected
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM subgraph_service__query_fees_collected
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM subgraph_service__p_o_i_presented
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM staking_legacy__allocation_collected
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM staking_legacy__rebate_claimed
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM staking_legacy__allocation_closed_f672
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM staking_legacy__allocation_closed_7203
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM staking_legacy__allocation_created
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM staking_legacy__asset_holder_update
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM staking_legacy__delegation_parameters_updated
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM staking_legacy__rebate_collected
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM staking_legacy__slasher_update
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM staking_legacy__stake_delegated_locked
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM staking_legacy__stake_deposited
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM staking_legacy__stake_locked
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM staking_legacy__stake_slashed
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM staking_legacy__stake_withdrawn
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM staking_parameters__parameter_updated
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM gns__n_signal_burned
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM gns__n_signal_minted
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM gns__subgraph_deprecated_cd16
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM gns__subgraph_deprecated_905d
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM gns__signal_burned
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM gns__signal_minted
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM horizon_staking__delegation_slashed
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM horizon_staking__delegation_slashing_enabled
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM horizon_staking__horizon_stake_deposited
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM horizon_staking__horizon_stake_locked
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM horizon_staking__horizon_stake_withdrawn
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM horizon_staking__max_thawing_period_set
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM horizon_staking__provision_created
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM horizon_staking__provision_increased
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM horizon_staking__provision_slashed
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM horizon_staking__provision_thawed
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM horizon_staking__thawing_period_cleared
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM horizon_staking__tokens_deprovisioned
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM horizon_staking__tokens_to_delegation_pool_added
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM horizon_staking__tokens_undelegated
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM rewards__parameter_updated
UNION ALL
SELECT block_number, block_hash, log_index, false AS length_update FROM rewards__rewards_assigned
UNION ALL
SELECT r.block_number, r.block_hash, r.log_index, false AS length_update
FROM rewards__horizon_rewards_assigned r WHERE EXISTS (
    SELECT 1 FROM allocation_creation a WHERE a.id = r."allocationID" AND a.legacy
    AND (a.block_number < r.block_number OR (a.block_number = r.block_number AND a.log_index <= r.log_index))
);

CREATE VIEW network_l1_observation AS
SELECT e.*,
       CASE WHEN c.result IS NULL THEN error('network epoch clock requires a pinned EpochManager.blockNum() read')
            WHEN c.reverted OR c.result = '0x' THEN 0
            ELSE CAST(nuthatch_uint256(c.result) AS BIGINT) END AS l1_block
FROM network_refresh_event e
LEFT JOIN epoch_manager_l1_block c ON c.block_number = e.block_number AND c.block_hash = e.block_hash;

-- Signalled uses the previously stored L1 clock rather than refreshing it.
CREATE VIEW network_epoch_observation AS
WITH clock_stream AS (
    SELECT block_number, log_index, length_update, l1_block FROM network_l1_observation
    UNION ALL
    SELECT block_number, log_index, false, NULL::BIGINT FROM curation__signalled
), observations AS (
    -- Carry the preceding clock in receipt order. A per-signal inequality join
    -- instead materializes all earlier refreshes as history grows.
    SELECT block_number, log_index, length_update,
           coalesce(l1_block, last_value(l1_block IGNORE NULLS) OVER (
               ORDER BY block_number, log_index
               ROWS BETWEEN UNBOUNDED PRECEDING AND 1 PRECEDING
           ), 0) AS l1_block
    FROM clock_stream
)
SELECT o.block_number, o.log_index, o.l1_block, s.length,
       CASE WHEN o.length_update THEN s.epoch
            ELSE s.epoch + (o.l1_block - s.start_block) // s.length END AS epoch,
       CASE WHEN o.length_update THEN s.start_block
            ELSE s.start_block + ((o.l1_block - s.start_block) // s.length) * s.length END AS start_block
FROM observations o
JOIN LATERAL (
    SELECT * FROM epoch_schedule s
    WHERE s.block_number < o.block_number OR (s.block_number = o.block_number AND s.log_index <= o.log_index)
    ORDER BY s.block_number DESC, s.log_index DESC LIMIT 1
) s ON true
WHERE o.l1_block > 0;

-- Only touched epochs exist. A length change can amend the current epoch's end.
CREATE VIEW epoch_bounds AS
SELECT CAST(epoch AS VARCHAR) AS id, CAST(epoch AS INTEGER) AS epoch,
       CAST(start_block AS INTEGER) AS "startBlock", CAST(start_block + length AS INTEGER) AS "endBlock"
FROM network_epoch_observation
QUALIFY row_number() OVER (PARTITION BY epoch ORDER BY block_number DESC, log_index DESC) = 1;
