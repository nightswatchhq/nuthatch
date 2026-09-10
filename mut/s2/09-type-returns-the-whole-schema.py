# Put back the branch that treated `__type` as `__schema`, returning the whole document under the
# wrong key.
p = "src/serve.rs"; s = open(p).read()
old = '    if roots.iter().any(|r| r.name == "__schema") {'
assert s.count(old) == 1
new = '    if roots.iter().any(|r| r.name == "__schema" || r.name == "__type") {'
open(p, "w").write(s.replace(old, new))
