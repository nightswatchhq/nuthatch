# Escape the wildcards before the backslash, so a caller's backslash re-escapes the escapes.
p = "src/graph_query.rs"; s = open(p).read()
old = """        let escaped = text
            .replace('\\\\', "\\\\\\\\")
            .replace('%', "\\\\%")"""
new = """        let escaped = text
            .replace('%', "\\\\%")
            .replace('\\\\', "\\\\\\\\")"""
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, new))
