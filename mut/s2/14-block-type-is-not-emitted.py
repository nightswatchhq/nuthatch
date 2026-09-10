# Stop emitting `_Block_` while `_Meta_.block` still references it: the document then points at a
# type it does not declare, which the dangling assertion in `render` exists to catch.
p = "src/graph_schema.rs"; s = open(p).read()
old = """        types.push(json!({"kind":"OBJECT","name":"_Block_","fields":[
            {"name":"hash","args":[],"type":type_ref("Bytes")},
            {"name":"number","args":[],"type":type_ref("Int!")},
            {"name":"timestamp","args":[],"type":type_ref("Int")},
            {"name":"parentHash","args":[],"type":type_ref("Bytes")}]}));
"""
assert s.count(old) == 1
open(p, "w").write(s.replace(old, ""))
