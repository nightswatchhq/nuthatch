# Route on the raw text again, so a filter value of "__schema" takes the introspection arm.
p = "src/serve.rs"; s = open(p).read()
old = '            "__schema" => {'
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, '            n if n == "__schema" || query.contains("__schema") => {'))
