# `too_deep` must spend the budget one level per sub-selection. Handing the full budget to each child
# makes the guard never fire, which raising MAX_TRAVERSAL (mutation 19) cannot distinguish.
p = "src/graph_query.rs"; s = open(p).read()
old = "        if let Some(deeper) = too_deep(&s.sub, budget - 1) {"
assert s.count(old) == 1, s.count(old)
open(p, "w").write(s.replace(old, "        if let Some(deeper) = too_deep(&s.sub, budget) {"))
