# Drop the refusal of arguments on a traversed field, so `swaps(first: 5)` silently loses its LIMIT.
p = "src/graph_query.rs"; s = open(p).read()
old = """            if !s.args.is_empty() {
                return Err(Unsupported::NestedSelection(s.name.clone()));
            }
"""
assert s.count(old) == 1
open(p, "w").write(s.replace(old, ""))
