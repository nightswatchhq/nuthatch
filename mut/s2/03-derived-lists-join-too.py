p = "src/graph_query.rs"; s = open(p).read()
old = """            (graph_schema::FieldType::Entity(t), None) => t.clone(),
            _ => return Err(Unsupported::NestedSelection(sel.name.clone())),"""
new = """            _ => match field.ty.entity_name() {
                Some(t) => t.to_string(),
                None => return Err(Unsupported::NestedSelection(sel.name.clone())),
            },"""
assert s.count(old) == 1
open(p, "w").write(s.replace(old, new))
