# `__type(name: "Nope")` must be null, not some other type. Separate from 09/10 because those died on
# a different assertion, so this is the one that proves the null case can fail.
p = "src/serve.rs"; s = open(p).read()
old = """                            .find(|t| t["name"].as_str() == Some(want.as_str()))
                            .cloned()"""
new = """                            .find(|t| t["name"].as_str() == Some(want.as_str()))
                            .or(d["__schema"]["types"].as_array().and_then(|a| a.first()))
                            .cloned()"""
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, new))
