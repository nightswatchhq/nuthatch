# Stop escaping the caller's own `%` and `_`, so `hooks_contains: "%"` matches every row instead of
# the literal percent sign it asked for.
p = "src/graph_query.rs"; s = open(p).read()
old = """            .replace('%', "\\\\%")
            .replace('_', "\\\\_")
"""
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, ""))
