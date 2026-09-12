# Stop filling in the `__Type` keys a standard client selects, so `FullType` receives objects missing
# `description`, `interfaces`, `possibleTypes`, `isDeprecated` and `deprecationReason`.
p = "src/graph_schema.rs"; s = open(p).read()
i = s.index('        for t in &mut types {\n            let object = t.get("kind")')
j = s.index('        let mut doc = json!({"__schema":{', i)
open(p, "w").write(s[:i] + s[j:])
