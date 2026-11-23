use clap::Parser;
use std::path::PathBuf;
use toolkit::bootstrap::{AppConfig, run_migrate, run_server};
use {
    api_gateway as _, authn_resolver as _, publisher as _, publisher_authn as _,
    types_registry as _,
};

#[derive(Parser)]
#[command(about = "gears-rust release publisher")]
struct Cli {
    #[arg(long, default_value = "config/publisher.yaml")]
    config: PathBuf,
    /// Apply database migrations and exit.
    #[arg(long)]
    migrate_only: bool,
    /// Print the effective configuration and exit.
    #[arg(long)]
    print_config: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let mut config = AppConfig::load_or_default(Some(&cli.config))?;
    // The gateway takes its address literally; let the environment choose it.
    if let Ok(bind) = std::env::var("PUBLISHER_BIND") {
        let gateway = config
            .gears
            .get_mut("api-gateway")
            .ok_or_else(|| anyhow::anyhow!("config has no api-gateway"))?;
        gateway["config"]["bind_addr"] = serde_json::Value::String(bind);
    }
    if cli.print_config {
        println!("{}", config.to_yaml()?);
        return Ok(());
    }
    if cli.migrate_only {
        run_migrate(config).await
    } else {
        run_server(config).await
    }
}
