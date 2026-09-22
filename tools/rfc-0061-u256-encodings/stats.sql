.mode line
SELECT file_name, stats_min_value, stats_max_value FROM parquet_metadata(['out-graph_token__approval/a_text.parquet', 'out-graph_token__approval/b_flba32.parquet', 'out-graph_token__transfer/d_text_dec38.parquet']) WHERE row_group_id = 0 AND path_in_schema IN ('value', 'value_dec');
