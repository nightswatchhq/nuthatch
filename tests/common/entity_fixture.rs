//! Compact test input uses `query`; the nest on disk always uses SQL files.

pub fn write(dir: &std::path::Path, fixture: &str) -> anyhow::Result<()> {
    let mut manifest: toml::Value = toml::from_str(fixture)?;
    std::fs::create_dir_all(dir.join("entities"))?;
    for entity in manifest["entities"].as_array_mut().unwrap() {
        let entity = entity.as_table_mut().unwrap();
        let query = entity.remove("query").unwrap();
        let path = format!("entities/{}.sql", entity["name"].as_str().unwrap());
        std::fs::write(dir.join(&path), query.as_str().unwrap())?;
        entity.insert("sql".into(), toml::Value::String(path));
    }
    std::fs::write(dir.join("entities.toml"), toml::to_string(&manifest)?)?;
    Ok(())
}
