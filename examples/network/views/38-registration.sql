CREATE VIEW horizon_registration AS
SELECT "serviceProvider" AS indexer, block_number, log_index, block_timestamp,
       TRY(nuthatch_abi_tuple('string,string,address', data)) AS registration
FROM subgraph_service__service_provider_registered;

CREATE VIEW provision_registration AS
SELECT indexer, json_extract_string(registration, '$[0]') AS url,
       json_extract_string(registration, '$[1]') AS "geoHash"
FROM horizon_registration WHERE registration IS NOT NULL
QUALIFY row_number() OVER (PARTITION BY indexer ORDER BY block_number DESC, log_index DESC) = 1;

CREATE VIEW provision_rewards_destination AS
SELECT indexer, destination FROM (
    SELECT indexer, block_number, log_index,
           json_extract_string(registration, '$[2]') AS destination
    FROM horizon_registration WHERE registration IS NOT NULL
    UNION ALL
    SELECT indexer, block_number, log_index, "rewardsDestination"
    FROM subgraph_service__rewards_destination_set
)
QUALIFY row_number() OVER (PARTITION BY indexer ORDER BY block_number DESC, log_index DESC) = 1;
