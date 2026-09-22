.mode line
SELECT '-- types as DuckDB 1.5.4 reads them' AS section;
SELECT (SELECT typeof(value) FROM 'out-graph_token__transfer/a_text.parquet' LIMIT 1) AS a_text,
       (SELECT typeof(value) FROM 'out-graph_token__transfer/b_flba32.parquet' LIMIT 1) AS b_flba32,
       (SELECT typeof(value) FROM 'out-graph_token__transfer/c_decimal76.parquet' LIMIT 1) AS c_decimal76,
       (SELECT typeof(value_dec) FROM 'out-graph_token__transfer/d_text_dec38.parquet' LIMIT 1) AS d_dec38;
SELECT '-- C: does DECIMAL(76) survive the read? rows whose value differs from the exact text' AS section;
SELECT count(*) AS rows_compared,
       count(*) FILTER (WHERE CAST(c.value AS VARCHAR) <> a.value) AS rows_changed_by_read,
       max(length(a.value)) AS longest_exact_text
FROM (SELECT value, row_number() OVER () AS r FROM 'out-graph_token__transfer/c_decimal76.parquet') c
JOIN (SELECT value, row_number() OVER () AS r FROM 'out-graph_token__transfer/a_text.parquet') a USING (r);
SELECT '-- C: one example of what the read does to a value' AS section;
SELECT a.value AS exact_text, CAST(c.value AS VARCHAR) AS as_read
FROM (SELECT value, row_number() OVER () AS r FROM 'out-graph_token__transfer/c_decimal76.parquet') c
JOIN (SELECT value, row_number() OVER () AS r FROM 'out-graph_token__transfer/a_text.parquet') a USING (r)
WHERE length(a.value) >= 22 LIMIT 1;
SELECT '-- ordering: max by each encoding on approvals (numeric max is 2^256-1)' AS section;
SELECT (SELECT max(value) FROM 'out-graph_token__approval/a_text.parquet') AS a_text_max,
       (SELECT hex(max(value)) FROM 'out-graph_token__approval/b_flba32.parquet') AS b_flba32_max_hex;
SELECT '-- footer statistics for the value column' AS section;
SELECT file_name, stats_min, stats_max FROM parquet_metadata(['out-graph_token__approval/a_text.parquet', 'out-graph_token__approval/b_flba32.parquet']) WHERE path_in_schema = 'value' AND row_group_id = 0;
SELECT '-- D: exact sum over the physical DECIMAL(38,0) companion, transfers' AS section;
SELECT sum(value_dec) AS sum_dec38, count(*) FILTER (WHERE value_overflow) AS overflow_rows FROM 'out-graph_token__transfer/d_text_dec38.parquet';
