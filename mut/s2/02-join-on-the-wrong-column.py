# `sel.name` stays consumed on purpose. Dropping it made this a compile error rather than a
# behaviour change, and `error: argument never used` carries no bracketed code, so the first
# version of this mutation read as GREEN against a `^error\[` detector.
p = "src/graph_query.rs"; s = open(p).read()
old = 'ON {alias}.\\"id\\" = {BASE}.\\"{}\\""'
assert s.count(old) == 1, s.count(old)
new = 'ON {alias}.\\"id\\" = {BASE}.\\"id\\" /* {} */"'
open(p, "w").write(s.replace(old, new))
