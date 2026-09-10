# Make the kind resolution a no-op, which is the hardcoded-SCALAR defect it replaced.
p = "src/graph_schema.rs"; s = open(p).read()
old = """        let mut dangling = Vec::new();
        walk(doc, kinds, &mut dangling);"""
new = """        let mut dangling = Vec::new();
        let _ = (doc, kinds);"""
assert s.count(old) == 1
open(p, "w").write(s.replace(old, new))
