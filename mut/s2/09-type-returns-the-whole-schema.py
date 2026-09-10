# Put back the branch that treated `__type` as `__schema`, returning the whole document under the
# wrong key.
p = "src/serve.rs"; s = open(p).read()
old = '    if query.contains("__schema") {'
assert s.count(old) == 1
open(p, "w").write(s.replace(old, '    if query.contains("__schema") || query.contains("__type") {'))
