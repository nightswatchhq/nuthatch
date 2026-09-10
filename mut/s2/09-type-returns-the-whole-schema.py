# Answer `__type` with the entire schema document instead of the named type.
p = "src/serve.rs"; s = open(p).read()
old = '                data.insert("__type".into(), found.unwrap_or(serde_json::Value::Null));'
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, '                data.insert("__type".into(), d.clone());'))
