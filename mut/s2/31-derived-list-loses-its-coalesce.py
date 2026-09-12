# `list()` over zero rows is NULL in DuckDB, so without the coalesce a childless parent answers null
# for a field the generated schema types `[T!]!`. Measured with the CLI before the code was written.
p = "src/graph_query.rs"; s = open(p).read()
old = '                "coalesce((SELECT to_json(list(t.s)) FROM (SELECT struct_pack({}) AS s \\'
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, '                "((SELECT to_json(list(t.s)) FROM (SELECT struct_pack({}) AS s \\'))
