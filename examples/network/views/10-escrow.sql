-- Derived from graph-network-subgraph 3ca039189e35912729e878f54ceb1ae684276ae6,
-- paymentsEscrow.ts and graphTallyCollector.ts. See LICENSE-upstream.
-- Keep amounts arbitrary precision; a uint256 does not fit in HUGEINT.
CREATE VIEW escrow_movement AS
SELECT payer, collector, receiver, CAST(tokens AS BIGNUM) AS delta FROM escrow__deposit
UNION ALL
SELECT payer, collector, receiver, -CAST(tokens AS BIGNUM) FROM escrow__withdraw
UNION ALL
SELECT payer, collector, receiver, -CAST(tokens AS BIGNUM) FROM escrow__escrow_collected;

CREATE VIEW escrow_thaw_update AS
SELECT payer, collector, receiver, tokens AS amount, "thawEndTimestamp" AS deadline,
       block_number, log_index FROM escrow__thaw
UNION ALL
SELECT payer, collector, receiver, '0', '0', block_number, log_index FROM escrow__cancel_thaw
UNION ALL
SELECT payer, collector, receiver, '0', '0', block_number, log_index FROM escrow__withdraw;

CREATE VIEW escrow_identity AS
SELECT payer, collector, receiver FROM escrow_movement
UNION
SELECT payer, collector, receiver FROM escrow_thaw_update
UNION
SELECT payer, '0x8f69f5c07477ac46fbc491b1e6d91e2bb0111a9e', receiver FROM tally__payment_collected;

CREATE VIEW payments_escrow_account AS
WITH balance AS (
    SELECT payer, collector, receiver, sum(delta) AS amount
    FROM escrow_movement GROUP BY payer, collector, receiver
), thaw AS (
    SELECT *, row_number() OVER (PARTITION BY payer, collector, receiver ORDER BY block_number DESC, log_index DESC) AS rank
    FROM escrow_thaw_update
)
SELECT i.payer || substr(i.collector, 3) || substr(i.receiver, 3) AS id,
       i.payer, i.collector, i.receiver,
       coalesce(CAST(b.amount AS VARCHAR), '0') AS balance,
       coalesce(t.amount, '0') AS "totalAmountThawing",
       coalesce(t.deadline, '0') AS "thawEndTimestamp"
FROM escrow_identity i
LEFT JOIN balance b USING (payer, collector, receiver)
LEFT JOIN thaw t ON t.payer = i.payer AND t.collector = i.collector AND t.receiver = i.receiver AND t.rank = 1;

-- These entities are created by deposit, withdrawal and payment, not merely by a thaw or signer.
CREATE VIEW payer AS
SELECT payer AS id FROM escrow__deposit UNION SELECT payer FROM escrow__withdraw
UNION SELECT payer FROM tally__payment_collected;

CREATE VIEW receiver AS
SELECT receiver AS id FROM escrow__deposit UNION SELECT receiver FROM escrow__withdraw
UNION SELECT receiver FROM tally__payment_collected;

CREATE VIEW signer_update AS
SELECT signer, authorizer, true AS authorized, NULL::VARCHAR AS deadline,
       block_number, log_index FROM tally__signer_authorized
UNION ALL
SELECT signer, authorizer, true, "thawEndTimestamp", block_number, log_index FROM tally__signer_thawing
UNION ALL
SELECT signer, authorizer, true, '0', block_number, log_index FROM tally__signer_thaw_canceled
UNION ALL
SELECT signer, authorizer, false, '0', block_number, log_index FROM tally__signer_revoked;

CREATE VIEW signer AS
WITH state AS (
    SELECT *,
        last_value(deadline IGNORE NULLS) OVER (
            PARTITION BY signer ORDER BY block_number, log_index
            ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS thaw,
        row_number() OVER (PARTITION BY signer ORDER BY block_number DESC, log_index DESC) AS rank
    FROM signer_update
)
SELECT signer AS id, authorizer AS payer, authorized AS "isAuthorized",
       coalesce(thaw, '0') AS "thawEndTimestamp" FROM state WHERE rank = 1;

CREATE VIEW escrow_transaction_event AS
SELECT tx_hash, log_index, 'deposit' AS type, payer, collector, receiver, tokens AS amount,
       NULL::VARCHAR AS allocation_id, collector AS escrow_collector, block_timestamp FROM escrow__deposit
UNION ALL
SELECT tx_hash, log_index, 'withdraw', payer, collector, receiver, tokens,
       NULL, collector, block_timestamp FROM escrow__withdraw
UNION ALL
SELECT tx_hash, log_index, 'redeem', payer, "dataService", receiver, tokens,
       '0x' || substr("collectionId", 27), '0x8f69f5c07477ac46fbc491b1e6d91e2bb0111a9e', block_timestamp
FROM tally__payment_collected;

CREATE VIEW payments_escrow_transaction AS
SELECT tx_hash || lower(
           lpad(hex(log_index & 255), 2, '0') || lpad(hex((log_index >> 8) & 255), 2, '0') ||
           lpad(hex((log_index >> 16) & 255), 2, '0') || lpad(hex((log_index >> 24) & 255), 2, '0')) AS id,
       tx_hash AS "transactionGroupId", type, payer, collector, receiver,
       allocation_id AS "allocationId", amount,
       payer || substr(escrow_collector, 3) || substr(receiver, 3) AS "escrowAccount",
       CAST(block_timestamp AS VARCHAR) AS timestamp
FROM escrow_transaction_event;
