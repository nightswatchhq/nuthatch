import re, json, pathlib
f = sorted(pathlib.Path("/home/pepe/bom/target/cargo-timings").glob("cargo-timing-*.html"))[-1]
data = json.loads(re.search(r"UNIT_DATA = (\[.*?\]);", f.read_text(), re.S).group(1))
tot = sum(u["duration"] for u in data)
groups = {
    "duckdb (incl. libduckdb-sys)": lambda n: "duckdb" in n,
    "wasmtime + cranelift + wasm*": lambda n: n.startswith(("wasmtime", "cranelift", "wasm", "wast", "wit-")),
    "dbsp + feldera":               lambda n: n.startswith(("dbsp", "feldera")),
    "arrow + parquet":              lambda n: n.startswith(("arrow", "parquet")),
    "ring / zstd / mimalloc / ittapi": lambda n: n.startswith(("ring", "zstd", "mimalloc", "ittapi")),
}
print("%-34s %9s %7s  %s" % ("group", "seconds", "share", "units"))
for name, pred in groups.items():
    us = [u for u in data if pred(u["name"])]
    s = sum(u["duration"] for u in us)
    print("%-34s %9.1f %6.1f%%  %d" % (name, s, 100 * s / tot, len(us)))
print()
print("total summed unit time: %.0fs across %d units" % (tot, len(data)))
