#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = public_rules_mcp::load_cli_args()?;
    let config = toml::from_str(&std::fs::read_to_string(&args.config_path)?)?;
    public_rules_mcp::run_server_with_transport_args(
        config,
        public_rules_mcp::TransportArgs {
            transport: args.transport,
            bind_addr: args.bind_addr,
            allowed_hosts: args.allowed_hosts,
            query_log_path: args.query_log_path,
        },
    )
    .await
}
