# Decide a to-one relation's presence from its selected values rather than the id marker, so a row that
# exists but whose selected fields are all null is reported as absent.
p = "src/serve.rs"; s = open(p).read()
old = "                let present = row.get(marker).is_some_and(|v| !v.is_null());"
assert s.count(old) == 1, s.count(old)
new = "                let present = inner.values().any(|v| !v.is_null());"
open(p, "w").write(s.replace(old, new))
