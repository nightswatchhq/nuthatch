# Return early on the first introspection root, dropping every other root field from the response.
p = "src/serve.rs"; s = open(p).read()
old = """                data.insert(root.key.clone(), d["__schema"].clone());
                continue;"""
assert s.count(old) == 1, s.count(old)
new = """                return (
                    StatusCode::OK,
                    Json(serde_json::json!({"data": {"__schema": d["__schema"].clone()}})),
                );"""
open(p, "w").write(s.replace(old, new))
