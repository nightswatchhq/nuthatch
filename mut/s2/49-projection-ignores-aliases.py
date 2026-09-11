# Key the projected object by the schema field name rather than the caller's alias.
p = "src/serve.rs"; s = open(p).read()
old = "                out.insert(s.key.clone(), project(&v, &s.sub));"
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, "                out.insert(s.name.clone(), project(&v, &s.sub));"))
