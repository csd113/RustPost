use clap::Parser;

#[derive(Parser)]
#[command(version, about = "Restricted native Linux RustPost updater")]
struct Arguments {
    #[arg(long, default_value = "/etc/rustpost-updater/updater.toml")]
    config: std::path::PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustpost::logging::init();
    rustpost::updates::run(&Arguments::parse().config).await
}
