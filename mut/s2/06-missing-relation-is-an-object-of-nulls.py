# Answer an absent to-one relation as an object of nulls rather than `null`, which a client cannot tell
# from a row that really exists with null fields - the distinction the id marker exists to make.
p = "src/serve.rs"; s = open(p).read()
old = """                    if present {
                        serde_json::Value::Object(inner)
                    } else {
                        serde_json::Value::Null
                    },"""
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, "                    serde_json::Value::Object(inner),"))
