# Return early on the first introspection root, dropping every other root field from the response.
# Anchored on the `continue` rather than the insert, because mutation 47 owns the insert line.
p = "src/serve.rs"; s = open(p).read()
old = """                data.insert(root.key.clone(), project(&d["__schema"], &root.sel));
                continue;"""
assert s.count(old) == 1, s.count(old)
new = """                return (
                    StatusCode::OK,
                    Json(serde_json::json!({"data":
                        {root.key.clone(): project(&d["__schema"], &root.sel)}})),
                );"""
open(p, "w").write(s.replace(old, new))
