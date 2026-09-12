p = "src/graph_query.rs"; s = open(p).read()
old = '        let joiner = if key == "and" { " AND " } else { " OR " };'
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, '        let joiner = " AND ";'))
