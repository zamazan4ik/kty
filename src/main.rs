//! # kuberift
mod cli;
mod dashboard;
mod events;
mod identity;
mod io;
mod openid;
mod resources;
mod ssh;
mod widget;

use cata::execute;
use clap::Parser;
use eyre::Result;

use crate::cli::Root;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::config::HookBuilder::default()
        .display_env_section(false)
        .display_location_section(false)
        .install()?;

    execute(&Root::parse()).await
}
