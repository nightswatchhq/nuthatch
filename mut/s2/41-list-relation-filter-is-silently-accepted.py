# Treat a list relation's nested filter as a to-one, joining on a column the parent does not have.
p = "src/graph_query.rs"; s = open(p).read()
old = """            (graph_schema::FieldType::Entity(t), None) => t.clone(),
            _ => {
                return Err(Unsupported::Operator(format!("""
assert s.count(old) == 1, s.count(old)
new = """            (graph_schema::FieldType::Entity(t), None) => t.clone(),
            (graph_schema::FieldType::List(i), _) if i.entity_name().is_some() => {
                i.entity_name().unwrap().to_string()
            }
            _ => {
                return Err(Unsupported::Operator(format!("""
open(p, "w").write(s.replace(old, new))
