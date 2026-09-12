# Remove the block-string pre-pass, so a description containing `type Fake @entity {` is syntax.
p = "src/graph_schema.rs"; s = open(p).read()
old = """    let blanked = blank_block_strings(text);
    let text: &str = &blanked;
"""
assert s.count(old) == 1
open(p, "w").write(s.replace(old, ""))
