# A non-derived list of entity ids needs `unnest`, not this aggregation: treating it as derived joins
# on a column the parent does not have, so the query answers with the wrong relation entirely.
p = "src/graph_query.rs"; s = open(p).read()
old = """        if let (graph_schema::FieldType::List(inner), Some(back)) = (&field.ty, &field.derived_from)
        {"""
assert s.count(old) == 1, s.count(old)
new = """        if let (graph_schema::FieldType::List(inner), back) = (
            &field.ty,
            &field.derived_from.clone().unwrap_or_else(|| "id".to_string()),
        ) {"""
open(p, "w").write(s.replace(old, new))
