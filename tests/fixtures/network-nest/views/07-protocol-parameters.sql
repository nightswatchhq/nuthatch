-- ParameterUpdated carries a name, not a value. The selected getter must have
-- succeeded at the same canonical block; an absent read must not become zero.
CREATE VIEW protocol_parameter_event AS
SELECT 'minimumIndexerStake' AS parameter, e.block_number, e.log_index,
       CASE WHEN c.result IS NULL OR c.reverted OR c.result = '0x'
            THEN error('missing pinned minimumIndexerStake read')
            ELSE nuthatch_uint256(c.result) END AS value
FROM staking_parameters__parameter_updated e
LEFT JOIN minimum_indexer_stake_read c ON c.block_number = e.block_number AND c.block_hash = e.block_hash
WHERE e.param = 'minimumIndexerStake'
UNION ALL
SELECT 'thawingPeriod' AS parameter, e.block_number, e.log_index,
       CASE WHEN c.result IS NULL OR c.reverted OR c.result = '0x'
            THEN error('missing pinned thawingPeriod read')
            ELSE nuthatch_uint256(c.result) END AS value
FROM staking_parameters__parameter_updated e
LEFT JOIN thawing_period_read c ON c.block_number = e.block_number AND c.block_hash = e.block_hash
WHERE e.param = 'thawingPeriod'
UNION ALL
SELECT 'curationPercentage' AS parameter, e.block_number, e.log_index,
       CASE WHEN c.result IS NULL OR c.reverted OR c.result = '0x'
            THEN error('missing pinned curationPercentage read')
            ELSE nuthatch_uint256(c.result) END AS value
FROM staking_parameters__parameter_updated e
LEFT JOIN curation_percentage_read c ON c.block_number = e.block_number AND c.block_hash = e.block_hash
WHERE e.param = 'curationPercentage'
UNION ALL
SELECT 'protocolFeePercentage' AS parameter, e.block_number, e.log_index,
       CASE WHEN c.result IS NULL OR c.reverted OR c.result = '0x'
            THEN error('missing pinned protocolPercentage read')
            ELSE nuthatch_uint256(c.result) END AS value
FROM staking_parameters__parameter_updated e
LEFT JOIN protocol_percentage_read c ON c.block_number = e.block_number AND c.block_hash = e.block_hash
WHERE e.param = 'protocolPercentage'
UNION ALL
SELECT 'delegationRatio' AS parameter, e.block_number, e.log_index,
       CASE WHEN c.result IS NULL OR c.reverted OR c.result = '0x'
            THEN error('missing pinned delegationRatio read')
            ELSE nuthatch_uint256(c.result) END AS value
FROM staking_parameters__parameter_updated e
LEFT JOIN delegation_ratio_read c ON c.block_number = e.block_number AND c.block_hash = e.block_hash
WHERE e.param = 'delegationRatio'
UNION ALL
SELECT 'maxAllocationEpochs' AS parameter, e.block_number, e.log_index,
       CASE WHEN c.result IS NULL OR c.reverted OR c.result = '0x'
            THEN error('missing pinned maxAllocationEpochs read')
            ELSE nuthatch_uint256(c.result) END AS value
FROM staking_parameters__parameter_updated e
LEFT JOIN max_allocation_epochs_read c ON c.block_number = e.block_number AND c.block_hash = e.block_hash
WHERE e.param = 'maxAllocationEpochs'
UNION ALL
SELECT 'channelDisputeEpochs' AS parameter, e.block_number, e.log_index,
       CASE WHEN c.result IS NULL OR c.reverted OR c.result = '0x'
            THEN error('missing pinned channelDisputeEpochs read')
            ELSE nuthatch_uint256(c.result) END AS value
FROM staking_parameters__parameter_updated e
LEFT JOIN channel_dispute_epochs_read c ON c.block_number = e.block_number AND c.block_hash = e.block_hash
WHERE e.param = 'channelDisputeEpochs'
UNION ALL
SELECT 'delegationParametersCooldown' AS parameter, e.block_number, e.log_index,
       CASE WHEN c.result IS NULL OR c.reverted OR c.result = '0x'
            THEN error('missing pinned delegationParametersCooldown read')
            ELSE nuthatch_uint256(c.result) END AS value
FROM staking_parameters__parameter_updated e
LEFT JOIN delegation_parameters_cooldown_read c ON c.block_number = e.block_number AND c.block_hash = e.block_hash
WHERE e.param = 'delegationParametersCooldown'
UNION ALL
SELECT 'delegationUnbondingPeriod' AS parameter, e.block_number, e.log_index,
       CASE WHEN c.result IS NULL OR c.reverted OR c.result = '0x'
            THEN error('missing pinned delegationUnbondingPeriod read')
            ELSE nuthatch_uint256(c.result) END AS value
FROM staking_parameters__parameter_updated e
LEFT JOIN delegation_unbonding_period_read c ON c.block_number = e.block_number AND c.block_hash = e.block_hash
WHERE e.param = 'delegationUnbondingPeriod'
UNION ALL
SELECT 'delegationTaxPercentage' AS parameter, e.block_number, e.log_index,
       CASE WHEN c.result IS NULL OR c.reverted OR c.result = '0x'
            THEN error('missing pinned delegationTaxPercentage read')
            ELSE nuthatch_uint256(c.result) END AS value
FROM staking_parameters__parameter_updated e
LEFT JOIN delegation_tax_percentage_read c ON c.block_number = e.block_number AND c.block_hash = e.block_hash
WHERE e.param = 'delegationTaxPercentage'
UNION ALL
SELECT 'maxThawingPeriod', block_number, log_index, "maxThawingPeriod" FROM horizon_staking__max_thawing_period_set
UNION ALL
SELECT 'thawingPeriod', block_number, log_index, '0' FROM horizon_staking__thawing_period_cleared;

CREATE VIEW protocol_parameters AS
SELECT parameter, value FROM protocol_parameter_event
QUALIFY row_number() OVER (PARTITION BY parameter ORDER BY block_number DESC, log_index DESC) = 1;
