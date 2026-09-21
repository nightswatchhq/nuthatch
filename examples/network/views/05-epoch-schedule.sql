-- Epoch coordinates are Ethereum L1 blocks, not Arbitrum L2 block numbers.
-- A length update anchors at the current epoch's old start, even when emitted
-- mid-epoch. Only the constructor's first update anchors at the event's L1 block.
CREATE VIEW epoch_length_event AS
SELECT e.block_number, e.log_index, CAST(e.epoch AS BIGINT) AS epoch,
       CASE WHEN CAST(e."epochLength" AS BIGINT) > 0 THEN CAST(e."epochLength" AS BIGINT)
            ELSE error('EpochLengthUpdate has a non-positive epoch length') END AS length,
       CASE WHEN c.result IS NULL OR c.reverted OR c.result = '0x'
            THEN error('EpochLengthUpdate requires a successful pinned EpochManager.blockNum() read')
            ELSE CAST(nuthatch_uint256(c.result) AS BIGINT) END AS l1_block,
       row_number() OVER (ORDER BY e.block_number, e.log_index) AS seq
FROM epoch_manager__epoch_length_update e
LEFT JOIN epoch_manager_l1_block c ON c.block_number = e.block_number AND c.block_hash = e.block_hash;

CREATE VIEW epoch_schedule AS
WITH RECURSIVE schedule(seq, block_number, log_index, epoch, length, start_block) AS (
    SELECT seq, block_number, log_index, epoch, length, l1_block
    FROM epoch_length_event WHERE seq = 1
    UNION ALL
    SELECT e.seq, e.block_number, e.log_index, e.epoch, e.length,
           s.start_block + ((e.l1_block - s.start_block) // s.length) * s.length
    FROM schedule s JOIN epoch_length_event e ON e.seq = s.seq + 1
)
SELECT * FROM schedule;
