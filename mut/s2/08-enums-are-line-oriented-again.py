p = "src/graph_schema.rs"; s = open(p).read()
old = "        .flat_map(|l| strip_comment(l).split_whitespace())"
assert s.count(old) == 1
open(p, "w").write(s.replace(old, "        .map(|l| strip_comment(l).trim())"))
