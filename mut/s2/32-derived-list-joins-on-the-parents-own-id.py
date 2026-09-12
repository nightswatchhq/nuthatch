# The back-reference is the one `@derivedFrom(field: …)` names. Joining the child's own id instead
# returns the wrong children.
p = "src/graph_query.rs"; s = open(p).read()
old = 'WHERE {alias}.\\"{back}\\" = {BASE}.\\"id\\"'
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, 'WHERE {alias}.\\"id\\" = {BASE}.\\"id\\"'))
