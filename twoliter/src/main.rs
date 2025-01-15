use crate::cmd::{init_logger, Args};
use anyhow::Result;
use clap::Parser;

mod cargo_make;
mod cmd;
mod common;
mod compatibility;
mod docker;
mod preflight;
mod project;
mod schema_version;
/// Test code that should only be compiled when running tests.
#[cfg(test)]
mod test;
mod tools;

/// `anyhow` prints a nicely formatted error message with `Debug`, so we can return a result from
/// the `main` function.
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_logger(args.log_level);
    preflight::preflight().await?;
    cmd::run(args).await
}
