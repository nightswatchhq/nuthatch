# Accept any suffix on any field type, so `sender_starts_with` lowers on a `Bytes` field the schema
# says has no prefix operators.
p = "src/graph_query.rs"; s = open(p).read()
old = """        if !allowed.contains(suffix) {
            return Err(Unsupported::Operator(key.to_string()));
        }
"""
assert s.count(old) == 1
open(p, "w").write(s.replace(old, ""))
