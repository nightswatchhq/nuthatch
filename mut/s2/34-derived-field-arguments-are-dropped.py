# The guard that refuses arguments on a traversed field, moved *before* the derived-list branch
# because it used to sit after it - so `swaps(first: 5)` compiled with the `first` silently dropped.
p = "src/graph_query.rs"; s = open(p).read()
old = """        if !sel.args.is_empty() {
            return Err(Unsupported::NestedSelection(sel.name.clone()));
        }
        // A `@derivedFrom` list is aggregated rather than joined."""
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, "        // A `@derivedFrom` list is aggregated rather than joined."))
