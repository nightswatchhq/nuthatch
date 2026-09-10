# Put back the silent fallback: a non-integer `first` becomes `LIMIT 100`.
p = "src/graph_query.rs"; s = open(p).read()
old = """            Some(_) => {
                return Err(Unsupported::Argument(
                    "first must be an integer".to_string(),
                ))
            }"""
assert s.count(old) == 1
open(p, "w").write(s.replace(old, "            Some(_) => 100,"))
