# Drop a selected field the document does not carry instead of answering null, so a client that asked for
# a key cannot find it at all.
p = "src/serve.rs"; s = open(p).read()
old = """            for s in sel {
                let v = o.get(&s.name).cloned().unwrap_or(serde_json::Value::Null);
                out.insert(s.key.clone(), project(&v, &s.sub));
            }"""
assert s.count(old) == 1, s.count(old)
new = """            for s in sel {
                if let Some(v) = o.get(&s.name) {
                    out.insert(s.key.clone(), project(v, &s.sub));
                }
            }"""
open(p, "w").write(s.replace(old, new))
