CREATE VIEW allocation AS
SELECT a.*, coalesce(f."queryFeesCollected", '0') AS "queryFeesCollected",
       coalesce(reb."queryFeeRebates", '0') AS "queryFeeRebates",
       coalesce(reb."delegationFees", '0') AS "delegationFees",
       coalesce(reb."distributedRebates", '0') AS "distributedRebates",
       coalesce(d."indexingIndexerRewards", '0') AS "indexingIndexerRewards",
       coalesce(d."indexingDelegatorRewards", '0') AS "indexingDelegatorRewards",
       coalesce(f."curatorRewards", '0') AS "curatorRewards",
       coalesce(r."indexingRewards", '0') AS "indexingRewards"
FROM allocation_lifecycle a
LEFT JOIN allocation_rebate_totals reb ON reb.id = a.id
LEFT JOIN allocation_reward_distribution d ON d.id = a.id
LEFT JOIN allocation_fee_totals f ON f.id = a.id
LEFT JOIN allocation_reward_totals r ON r.id = a.id;
