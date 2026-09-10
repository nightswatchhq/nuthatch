# Answer under the schema field name instead of the caller's key, so `{ p: pools … }` comes back as
# `pools` and the client finds nothing where it looked.
p = "src/serve.rs"; s = open(p).read()
old = "                data.insert(root.key.clone(), value);"
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, "                data.insert(root.name.clone(), value);"))
