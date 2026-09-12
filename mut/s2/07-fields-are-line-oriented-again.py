p = "src/graph_schema.rs"; s = open(p).read()
old = "        if b[i].is_ascii_whitespace() || b[i] == b',' {"
new = "        if b[i] == b'\\n' {\n            i += 1;\n            continue;\n        }\n" + old
assert s.count(old) == 1
old2 = """            derived_from: derived_target(tail),
        });
    }
    out
}"""
new2 = """            derived_from: derived_target(tail),
        });
        while i < b.len() && b[i] != b'\\n' {
            i += 1;
        }
    }
    out
}"""
assert s.count(old2) == 1
open(p, "w").write(s.replace(old, new).replace(old2, new2))
