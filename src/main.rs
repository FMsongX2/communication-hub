use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use communication_hub::{
    config::{Config, default_config_path},
    daemon,
    event::{Event, Plan, now},
    store::Store,
    worker,
};
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version,
    about = "Local communication and work hub; channel/account/conversation isolation"
)]
struct Cli {
    #[arg(long,global=true,default_value_os_t=default_config_path())]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Run {
        #[arg(long)]
        no_receiver: bool,
    },
    Status,
    Adapters,
    Pause,
    Resume,
    RefreshPolicy,
    Board {
        #[arg(long)]
        open: bool,
    },
    Ingest {
        #[arg(long)]
        file: PathBuf,
    },
    ExpressionPick {
        #[arg(long)]
        file: PathBuf,
    },
    ModelSmoke {
        #[arg(long)]
        file: PathBuf,
    },
    DeliveryCheck {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        reply: String,
        #[arg(long)]
        sticker: Option<String>,
        #[arg(long)]
        send: bool,
    },
    PrepareBundle {
        #[arg(long)]
        file: PathBuf,
        #[arg(long)]
        bundle: String,
    },
}
#[tokio::main(worker_threads = 2)]
async fn main() {
    unsafe {
        libc::umask(0o077);
    }
    if let Err(_error) = execute().await {
        eprintln!(
            "communication-hub: operation failed; inspect private service state and configuration"
        );
        std::process::exit(1)
    }
}
async fn execute() -> Result<()> {
    let cli = Cli::parse();
    let cfg = Config::load(&cli.config)?;
    let load = |p: &PathBuf| -> Result<Event> { Ok(serde_json::from_slice(&std::fs::read(p)?)?) };
    let result: Value = match cli.command {
        Command::Run { no_receiver } => {
            daemon::run(cfg, no_receiver).await?;
            return Ok(());
        }
        Command::Status => daemon::request(&cfg.socket, json!({"method":"status"})).await?,
        Command::Adapters => daemon::request(&cfg.socket, json!({"method":"adapters"})).await?,
        Command::Pause => daemon::request(&cfg.socket, json!({"method":"pause"})).await?,
        Command::Resume => daemon::request(&cfg.socket, json!({"method":"resume"})).await?,
        // Stateless calls touch no room thread, so a running hub needs no pause for this check.
        Command::RefreshPolicy => {
            worker::refresh_policies(&cfg, &Store::open(cfg.state.clone())?).await?
        }
        Command::Board { open } => {
            let info =
                communication_hub::config::json_file(&cfg.state.join("dashboard-status.json"))?;
            if info["online"] != true {
                bail!("dashboard_not_running")
            }
            // Verify the hub itself is reachable; old status files alone are insufficient.
            let running = daemon::request(&cfg.socket, json!({"method":"status"})).await?;
            if running["result"]["pid"] != info["pid"] {
                bail!("dashboard_status_is_stale")
            }
            if open {
                let auth =
                    communication_hub::config::json_file(&cfg.state.join("dashboard-auth.json"))?;
                let url = format!(
                    "{}#token={}",
                    info["url"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("dashboard_url_missing"))?,
                    auth["token"]
                        .as_str()
                        .ok_or_else(|| anyhow::anyhow!("dashboard_auth_missing"))?
                );
                let ok = tokio::process::Command::new("/usr/bin/open")
                    .arg(url)
                    .status()
                    .await?;
                if !ok.success() {
                    bail!("browser_open_failed")
                }
            }
            info
        }
        Command::Ingest { file } => {
            daemon::request(&cfg.socket, json!({"method":"ingest","event":load(&file)?})).await?
        }
        Command::ExpressionPick { file } => {
            let e = load(&file)?;
            cfg.validate_channel(&e.conversation.provider, &e.conversation.account)?;
            json!(communication_hub::expressions::pick(
                &cfg,
                &Store::open(cfg.state.clone())?,
                &e
            )?)
        }
        Command::ModelSmoke { file } => {
            if cfg.external_auto_send || cfg.dispatch_enabled {
                bail!("smoke_requires_isolated_non_dispatch_config")
            }
            let e = load(&file)?;
            cfg.validate_channel(&e.conversation.provider, &e.conversation.account)?;
            let store = Store::open(cfg.state.clone())?;
            e.validate(now(), store.initialized(&e.conversation)?)?;
            worker::model(&cfg, &store, &e).await?
        }
        Command::DeliveryCheck {
            file,
            reply,
            sticker,
            send,
        } => {
            if !send || !cfg.external_auto_send {
                bail!("delivery_check_requires_explicit_send")
            }
            let status = daemon::request(&cfg.socket, json!({"method":"status"})).await?;
            if status["result"]["processing"] == true
                || status["result"]["dispatch_enabled"] == true
            {
                bail!("pause_hub_before_controlled_delivery")
            }
            let e = load(&file)?;
            cfg.validate_channel(&e.conversation.provider, &e.conversation.account)?;
            let store = Store::open(cfg.state.clone())?;
            e.validate(now(), store.initialized(&e.conversation)?)?;
            worker::deliver(
                &cfg,
                &store,
                &e,
                &e.key(),
                "controlled",
                &Plan {
                    reply,
                    bundle_id: None,
                    sticker_id: sticker,
                },
            )
            .await?
        }
        Command::PrepareBundle { file, bundle } => {
            let e = load(&file)?;
            communication_hub::attachments::prepare(&cfg, &e, &e.key(), &bundle)?
        }
    };
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}
