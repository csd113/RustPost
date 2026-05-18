#![allow(clippy::multiple_crate_versions)]

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustpost::cli::run().await
}
