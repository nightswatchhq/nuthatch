# Splice the boolean tree in without its outer brackets, so `x AND a OR b` rebinds.
p = "src/graph_query.rs"; s = open(p).read()
old = '        return Ok(format!("({})", parts.join(joiner)));'
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, '        return Ok(parts.join(joiner));'))
