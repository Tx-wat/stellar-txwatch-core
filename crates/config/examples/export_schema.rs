use txwatch_config::AppConfig;

fn main() -> anyhow::Result<()> {
    let schema = serde_json::to_string_pretty(&AppConfig::json_schema())?;
    println!("{}", schema);
    Ok(())
}
