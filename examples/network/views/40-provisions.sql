-- Stage-1 Provision fields selected by indexer-agent. Parameter staging must not
-- change the active parameters, and thawing does not itself remove provisioned stake.
CREATE VIEW provision_movement AS
SELECT "serviceProvider" AS indexer, verifier AS service, block_number, log_index, block_timestamp,
       CAST(tokens AS BIGNUM) AS provisioned, CAST(0 AS BIGNUM) AS thawing,
       CAST(0 AS BIGNUM) AS slashed
FROM horizon_staking__provision_created
UNION ALL
SELECT "serviceProvider", verifier, block_number, log_index, block_timestamp,
       CAST(tokens AS BIGNUM), CAST(0 AS BIGNUM), CAST(0 AS BIGNUM)
FROM horizon_staking__provision_increased
UNION ALL
SELECT "serviceProvider", verifier, block_number, log_index, block_timestamp,
       CAST(0 AS BIGNUM), CAST(tokens AS BIGNUM), CAST(0 AS BIGNUM)
FROM horizon_staking__provision_thawed
UNION ALL
SELECT "serviceProvider", verifier, block_number, log_index, block_timestamp,
       CAST(0 AS BIGNUM) - CAST(tokens AS BIGNUM),
       CAST(0 AS BIGNUM) - CAST(tokens AS BIGNUM), CAST(0 AS BIGNUM)
FROM horizon_staking__tokens_deprovisioned
UNION ALL
SELECT "serviceProvider", verifier, block_number, log_index, block_timestamp,
       CAST(0 AS BIGNUM) - CAST(tokens AS BIGNUM), CAST(0 AS BIGNUM), CAST(tokens AS BIGNUM)
FROM horizon_staking__provision_slashed;

CREATE VIEW provision_parameters AS
SELECT "serviceProvider" AS indexer, verifier AS service, block_number, log_index, block_timestamp,
       "maxVerifierCut", "thawingPeriod", true AS active, true AS pending
FROM horizon_staking__provision_created
UNION ALL
SELECT "serviceProvider", verifier, block_number, log_index, block_timestamp,
       "maxVerifierCut", "thawingPeriod", true, false
FROM horizon_staking__provision_parameters_set
UNION ALL
SELECT "serviceProvider", verifier, block_number, log_index, block_timestamp,
       "maxVerifierCut", "thawingPeriod", false, true
FROM horizon_staking__provision_parameters_staged;

CREATE VIEW provision_allocation_movement AS
SELECT indexer, '0xb2bb92d0de618878e438b55d5846cfecd9301105' AS service,
       block_number, log_index, block_timestamp, CAST(tokens AS BIGNUM) AS tokens, 1 AS active, 1 AS created
FROM subgraph_service__allocation_created
UNION ALL
SELECT indexer, '0xb2bb92d0de618878e438b55d5846cfecd9301105', block_number, log_index, block_timestamp,
       CAST("newTokens" AS BIGNUM) - CAST("oldTokens" AS BIGNUM), 0, 0
FROM subgraph_service__allocation_resized
UNION ALL
SELECT indexer, '0xb2bb92d0de618878e438b55d5846cfecd9301105', block_number, log_index, block_timestamp,
       CAST(0 AS BIGNUM) - CAST(tokens AS BIGNUM), -1, 0
FROM subgraph_service__allocation_closed;

-- Registration, delegation and reward handlers can create a zero-stake provision.
-- Its identity must not depend on a later ProvisionCreated event.
CREATE VIEW provision_other_identity AS
SELECT "serviceProvider" AS indexer, verifier AS service, block_number, log_index, block_timestamp
FROM horizon_staking__delegation_fee_cut_set
UNION ALL SELECT "serviceProvider", verifier, block_number, log_index, block_timestamp FROM horizon_staking__thaw_request_created
WHERE "requestType" = '0'
UNION ALL SELECT "serviceProvider", verifier, block_number, log_index, block_timestamp FROM horizon_staking__tokens_delegated
UNION ALL SELECT "serviceProvider", verifier, block_number, log_index, block_timestamp FROM horizon_staking__tokens_to_delegation_pool_added
UNION ALL SELECT "serviceProvider", verifier, block_number, log_index, block_timestamp FROM horizon_staking__delegation_slashed
UNION ALL SELECT "serviceProvider", verifier, block_number, log_index, block_timestamp FROM horizon_staking__tokens_undelegated
UNION ALL SELECT "serviceProvider", verifier, block_number, log_index, block_timestamp FROM horizon_staking__delegated_tokens_withdrawn
UNION ALL SELECT "serviceProvider", '0xb2bb92d0de618878e438b55d5846cfecd9301105', block_number, log_index, block_timestamp
FROM subgraph_service__service_provider_registered
WHERE TRY(nuthatch_abi_tuple('string,string,address', data)) IS NOT NULL
UNION ALL SELECT indexer, '0xb2bb92d0de618878e438b55d5846cfecd9301105', block_number, log_index, block_timestamp
FROM subgraph_service__rewards_destination_set
UNION ALL SELECT indexer, '0xb2bb92d0de618878e438b55d5846cfecd9301105', block_number, log_index, block_timestamp
FROM subgraph_service__indexing_rewards_collected
UNION ALL SELECT "serviceProvider", '0xb2bb92d0de618878e438b55d5846cfecd9301105', block_number, log_index, block_timestamp
FROM subgraph_service__query_fees_collected;

CREATE VIEW provision_identity AS
SELECT * FROM (
    SELECT indexer, service, block_number, log_index, block_timestamp FROM provision_movement
    UNION ALL SELECT indexer, service, block_number, log_index, block_timestamp FROM provision_parameters
    UNION ALL SELECT indexer, service, block_number, log_index, block_timestamp FROM provision_allocation_movement
    UNION ALL SELECT indexer, service, block_number, log_index, block_timestamp FROM provision_other_identity
)
QUALIFY row_number() OVER (PARTITION BY indexer, service ORDER BY block_number, log_index) = 1;

CREATE VIEW provision_fee_cut AS
SELECT "serviceProvider" AS indexer, verifier AS service, "paymentType" AS payment_type,
       CAST(CAST(1000000 AS BIGNUM) - CAST("feeCut" AS BIGNUM) AS VARCHAR) AS cut
FROM horizon_staking__delegation_fee_cut_set
QUALIFY row_number() OVER (PARTITION BY "serviceProvider", verifier, "paymentType"
    ORDER BY block_number DESC, log_index DESC) = 1;
