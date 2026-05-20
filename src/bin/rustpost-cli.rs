#![allow(clippy::multiple_crate_versions)]

use std::io::Write as _;

#[tokio::main]
async fn main() {
    if let Err(error) = rustpost::cli::run().await {
        let mut stderr = std::io::stderr().lock();
        let _write_result = stderr.write_all(rustpost::terminal::render_error(&error).as_bytes());
        std::process::exit(1);
    }
}
