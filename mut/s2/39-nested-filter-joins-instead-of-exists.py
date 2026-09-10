# A join would multiply the parent row per matching child, so `first` would stop meaning what it says.
p = "src/graph_query.rs"; s = open(p).read()
old = '            "EXISTS (SELECT 1 FROM \\"{view}\\" {alias} WHERE {alias}.\\"id\\" = {base}.\\"{field_name}\\" AND {})",'
assert s.count(old) == 1, s.count(old)
new = '            "{alias}.\\"id\\" = {base}.\\"{field_name}\\" AND {} AND TRUE /* {view} */",'
open(p, "w").write(s.replace(old, new))
