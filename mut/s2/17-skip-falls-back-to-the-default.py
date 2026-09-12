p = "src/graph_query.rs"; s = open(p).read()
old = '            Some(_) => return Err(Unsupported::Argument("skip must be an integer".to_string())),'
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, "            Some(_) => 0,"))
