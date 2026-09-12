# Hand back the whole rendered document regardless of what was selected, so a client asking for `name`
# receives all eight keys of every type.
p = "src/serve.rs"; s = open(p).read()
old = '                data.insert(root.key.clone(), project(&d["__schema"], &root.sel));'
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, '                data.insert(root.key.clone(), d["__schema"].clone());'))
