use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use kronical::util::config::AppConfig;
use std::path::PathBuf;

#[derive(Parser)]
#[command(author, version, about = "Kronicle admin/bootstrapping CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Init,
}

fn ensure_workspace_dir(workspace_dir: &PathBuf) -> Result<()> {
    if !workspace_dir.exists() {
        std::fs::create_dir_all(workspace_dir).context("Failed to create workspace directory")?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(workspace_dir)?.permissions();
        perms.set_mode(0o700);
        std::fs::set_permissions(workspace_dir, perms)?;
    }
    Ok(())
}

fn main() -> Result<()> {
    env_logger::init();
    let cli = Cli::parse();
    match cli.command {
        Commands::Init => {
            let cfg = AppConfig::load().unwrap_or_default();
            ensure_workspace_dir(&cfg.workspace_dir)?;
            println!("Initialized workspace at {}", cfg.workspace_dir.display());
        }
    }
    Ok(())
}
