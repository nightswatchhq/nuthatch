# Give every aliased scalar the same column, so two aliases on one field overwrite each other.
p = "src/graph_query.rs"; s = open(p).read()
old = '                let c = format!("a{i}");'
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, '                let c = "a0".to_string();'))
