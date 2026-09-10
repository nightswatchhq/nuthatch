# Go back to emitting only queryType and types, so a standard introspection request gets a response
# missing fields it selected.
p = "src/graph_schema.rs"; s = open(p).read()
old = """            "mutationType": Value::Null,
            "subscriptionType": Value::Null,
            "types": types,
            "directives": directives,"""
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, """            "types": types,
            "__unused": directives,"""))
