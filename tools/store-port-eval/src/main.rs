fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args_os()
        .nth(1)
        .ok_or("Usage: store-port-probe OWNED_TEMP_DATABASE")?;
    let report = devworks_store_port_probe::run(std::path::Path::new(&path))?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
