#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = public_rules_mcp::load_cli_args()?;
    let config = toml::from_str(&std::fs::read_to_string(&args.config_path)?)?;
    public_rules_mcp::run_server(config, args.transport, args.bind_addr).await
}
