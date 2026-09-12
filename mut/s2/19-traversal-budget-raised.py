# The budget is now the mechanism rather than a comment, so raising it must break the depth test.
p = "src/graph_query.rs"; s = open(p).read()
old = "const MAX_TRAVERSAL: usize = 1;"
assert s.count(old) == 1
open(p, "w").write(s.replace(old, "const MAX_TRAVERSAL: usize = 2;"))
