CREATE VIEW controller_activity AS
SELECT block_number, block_hash, log_index FROM controller__set_contract_proxy
UNION ALL SELECT block_number, block_hash, log_index FROM controller__new_ownership
UNION ALL SELECT block_number, block_hash, log_index FROM controller__partial_pause_changed
UNION ALL SELECT block_number, block_hash, log_index FROM controller__pause_changed
UNION ALL SELECT block_number, block_hash, log_index FROM controller__new_pause_guardian;

CREATE VIEW controller_bootstrap AS
WITH first_event AS (
    SELECT * FROM controller_activity ORDER BY block_number, log_index LIMIT 1
)
SELECT CASE WHEN c.result IS NULL THEN error('missing pinned Controller.getGovernor at network creation')
            WHEN c.reverted OR c.result = '0x' THEN '0x0000000000000000000000000000000000000000'
            WHEN NOT regexp_full_match(c.result, '0x0{24}[0-9a-fA-F]{40}') THEN error('invalid Controller.getGovernor address result')
            ELSE '0x' || lower(substr(c.result, 27)) END AS governor
FROM first_event e LEFT JOIN governor_read c
ON c.block_number = e.block_number AND c.block_hash = e.block_hash
WHERE NOT EXISTS (SELECT 1 FROM controller__new_ownership);

CREATE VIEW controller_state AS
SELECT '1' AS id,
       coalesce((SELECT CAST("isPaused" AS BOOLEAN) FROM controller__pause_changed ORDER BY block_number DESC, log_index DESC LIMIT 1), false) AS "isPaused",
       coalesce((SELECT CAST("isPaused" AS BOOLEAN) FROM controller__partial_pause_changed ORDER BY block_number DESC, log_index DESC LIMIT 1), false) AS "isPartialPaused",
       coalesce((SELECT "to" FROM controller__new_ownership ORDER BY block_number DESC, log_index DESC LIMIT 1),
           (SELECT governor FROM controller_bootstrap),
           '0x0000000000000000000000000000000000000000') AS governor,
       coalesce((SELECT "pauseGuardian" FROM controller__new_pause_guardian ORDER BY block_number DESC, log_index DESC LIMIT 1),
           '0x0000000000000000000000000000000000000000') AS "pauseGuardian"
WHERE EXISTS (SELECT 1 FROM controller_activity);

CREATE VIEW controller_contract AS
SELECT id, "contractAddress"
FROM controller__set_contract_proxy
QUALIFY row_number() OVER (PARTITION BY id ORDER BY block_number DESC, log_index DESC) = 1;
