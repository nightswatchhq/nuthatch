# Lower `_nocase` as a case-sensitive match.
p = "src/graph_query.rs"; s = open(p).read()
old = '                    (false, true) => "ILIKE",'
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, '                    (false, true) => "LIKE",'))
