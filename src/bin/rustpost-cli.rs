#![allow(clippy::multiple_crate_versions)]

#[tokio::main]
async fn main() {
    if let Err(error) = rustpost::cli::run().await {
        eprint!("{}", rustpost::terminal::render_error(&error));
        std::process::exit(1);
    }
}
