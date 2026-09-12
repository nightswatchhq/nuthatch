# Answer `__type` with whatever type comes first instead of the one named.
p = "src/serve.rs"; s = open(p).read()
old = '                .find(|t| t["name"].as_str() == Some(want.as_str()))'
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, '                .find(|t| t["name"].as_str() == Some(want.as_str()) || true)'))
