# Return early on the first introspection root, dropping every other root field from the response.
p = "src/serve.rs"; s = open(p).read()
old = """                data.insert("__schema".into(), d["__schema"].clone());
                continue;"""
new = """                return (
                    StatusCode::OK,
                    Json(serde_json::json!({"data": {"__schema": d["__schema"].clone()}})),
                );"""
assert s.count(old) == 1
open(p, "w").write(s.replace(old, new))
