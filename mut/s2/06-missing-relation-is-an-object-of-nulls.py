p = "src/serve.rs"; s = open(p).read()
old = """                    if any {
                        serde_json::Value::Object(inner)
                    } else {
                        serde_json::Value::Null
                    },"""
assert s.count(old) == 1
open(p, "w").write(s.replace(old, "                    serde_json::Value::Object(inner),"))
