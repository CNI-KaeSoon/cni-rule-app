#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = public_rules_mcp::load_config_arg()?;
    public_rules_mcp::run_stdio_server(config).await
}
