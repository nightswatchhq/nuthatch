# Route on the raw text again, so a filter value of "__schema" is answered with the schema document.
p = "src/serve.rs"; s = open(p).read()
old = '    if roots.iter().any(|r| r.name == "__schema") {'
assert s.count(old) == 1
open(p, "w").write(s.replace(old, '    if query.contains("__schema") {'))
