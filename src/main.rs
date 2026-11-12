/* -------------------------------------------------------------------------- */
/*                                  main.rs                                   */
/* -------------------------------------------------------------------------- */

mod agent;
mod backend;
mod client;
mod config;
mod host;
mod mcp;
mod mention;
mod permissions;
mod protocol;
mod render;
mod repl;
mod server;
mod sessions;
mod turn;
mod ui;

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use yansi::Paint;

use backend::Backend;
use client::Client;
use config::Config;
use host::Host;
use repl::Repl;
use sessions::SessionId;

/// A minimal command-line agent.
#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// Read the system prompt from this file
    #[arg(long, short)]
    prompt_path: Option<PathBuf>,

    /// Use this model, as `vendor/model`, instead of the configured one
    #[arg(long, short)]
    model: Option<String>,

    /// Thinking level: off, minimal, low, medium, high, xhigh or max
    #[arg(long, short)]
    think: Option<config::Thinking>,

    /// Read this config file instead of the default one
    #[arg(long)]
    config: Option<PathBuf>,

    /// Continue the most recent conversation
    #[arg(long, short)]
    resume: bool,

    /// Stay in the REPL after answering MESSAGE
    #[arg(long, short)]
    interactive: bool,

    /// Run the agent on NAME from `[remotes]`, which keeps the conversations
    #[arg(long, value_name = "NAME")]
    remote: Option<String>,

    /// Run the agent here, even when `remote` is configured
    #[arg(long, conflicts_with = "remote")]
    local: bool,

    /// Host the agent for remote clients, as `[server]` configures
    #[arg(
        long,
        conflicts_with_all = ["prompt_path", "model", "think", "resume", "interactive", "remote", "local", "message"],
    )]
    serve: bool,

    /// Message to send; without one, hey starts a REPL
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    message: Vec<String>,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Args::parse()).await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("{} {error:#}", "Error:".red());
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<ExitCode> {
    let config = Config::load(args.config.as_deref())?;
    render::configure(&config.display);
    if args.serve {
        let host = Arc::new(Host::new(&config).await?);
        server::serve(host, &config.server).await?;
        return Ok(ExitCode::SUCCESS);
    }

    let system_prompt = match &args.prompt_path {
        Some(path) => Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("cannot read system prompt {}", path.display()))?,
        ),
        None => None,
    };
    let backend = connect(&args, &config).await?;
    let defaults = backend.defaults().await?;
    let model = backend.model(args.model.as_deref().unwrap_or(&defaults.model)).await?;
    let thinking = args.think.or(defaults.thinking);
    let thread = match args.resume {
        true => backend.threads().await?.into_iter().next().map(|summary| summary.id),
        false => None,
    }
    .unwrap_or_else(|| {
        if args.resume {
            render::notice("no saved conversation; starting a new one");
        }
        SessionId::new()
    });

    let style = render::Style::from(&config.display);
    let message = (!args.message.is_empty()).then(|| args.message.join(" "));

    match message {
        Some(prompt) if !args.interactive => {
            let request = turn::request(
                prompt,
                protocol::RunOptions {
                    model: model.reference,
                    thinking,
                    system_prompt,
                },
            );
            let turn = turn::run(&backend, &thread, request, &style).await;
            Ok(match turn.outcome {
                turn::Outcome::Answered => ExitCode::SUCCESS,
                turn::Outcome::Interrupted | turn::Outcome::Failed => ExitCode::FAILURE,
            })
        }
        first => {
            let repl = Repl::new(repl::Setup {
                backend,
                thread,
                model,
                thinking,
                system_prompt,
                style,
                greetings: config.greetings,
                editor: config.editor,
                colors: config.display.colors,
            })
            .await;
            repl.run(first).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// The remote that `--remote` or the config names, unless `--local`; else a
/// host in this process.
async fn connect(args: &Args, config: &Config) -> Result<Backend> {
    let remote = match args.local {
        true => None,
        false => args.remote.as_ref().or(config.remote.as_ref()),
    };
    match remote {
        Some(name) => {
            let remote = config
                .remotes
                .get(name)
                .with_context(|| format!("no remote `{name}` in `[remotes]`"))?;
            Ok(Backend::Remote(Client::new(name, remote)?))
        }
        None => Ok(Backend::Local(Arc::new(Host::new(config).await?))),
    }
}
