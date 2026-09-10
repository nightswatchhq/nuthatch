# Treat a list relation's nested filter as a to-one, joining `EXISTS` on a column the parent does not
# have - so the endpoint answers a query whose semantics the reference will not even demonstrate.
import re
p = "src/graph_query.rs"; s = open(p).read()
# The arm sits in `lower_predicate`, which is the second occurrence of this pattern in the file.
pat = "            (graph_schema::FieldType::Entity(t), None) => t.clone(),\n            _ => return Err(Unsupported::Operator(format!("
assert s.count(pat) == 1, s.count(pat)
new = ("            (graph_schema::FieldType::Entity(t), None) => t.clone(),\n"
       "            (graph_schema::FieldType::List(i), _) if i.entity_name().is_some() => {\n"
       "                i.entity_name().unwrap().to_string()\n"
       "            }\n"
       "            _ => return Err(Unsupported::Operator(format!(")
open(p, "w").write(s.replace(pat, new))
