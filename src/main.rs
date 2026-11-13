/* -------------------------------------------------------------------------- */
/*                                  main.rs                                   */
/* -------------------------------------------------------------------------- */

mod agent;
mod backend;
mod browser;
mod client;
mod config;
mod gate;
mod host;
mod mcp;
mod memory;
mod mention;
mod permissions;
mod preamble;
mod protocol;
mod render;
mod repl;
mod results;
mod server;
mod sessions;
mod skills;
mod subagents;
mod token;
mod turn;
mod ui;
mod web;

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
use preamble::Hosting;
use protocol::Machine;
use repl::Repl;
use sessions::SessionId;
use subagents::Crew;

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

    /// Continue the most recent conversation that is not archived
    #[arg(long, short)]
    resume: bool,

    /// List conversations and what they are doing, to open, archive,
    /// delete or fork them
    #[arg(long, short = 'l', conflicts_with_all = ["resume", "interactive", "message"])]
    threads: bool,

    /// List what hey remembers, to write, edit or forget memories
    #[arg(long, conflicts_with_all = ["resume", "threads", "interactive", "message"])]
    memories: bool,

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
        conflicts_with_all = ["prompt_path", "model", "think", "resume", "threads", "memories", "interactive", "remote", "local", "message"],
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
        let hosting = Hosting::Served {
            listen: config.server.listen,
        };
        let host = Arc::new(Host::new(&config, hosting).await?);
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
    if args.memories {
        browser::memories::browse(&backend).await?;
        return Ok(ExitCode::SUCCESS);
    }
    let defaults = backend.defaults().await?;
    let model = backend.model(args.model.as_deref().unwrap_or(&defaults.model)).await?;
    let thinking = args.think.or(defaults.thinking);
    let thread = match (args.threads, args.resume) {
        (true, _) => match browser::threads::browse(&backend, None).await?.open {
            Some(thread) => thread,
            None => return Ok(ExitCode::SUCCESS),
        },
        (false, true) => latest(&backend).await?,
        (false, false) => SessionId::new(),
    };

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
            backend.settle().await;
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
                config_file: config.file,
                editor: config.editor,
                colors: config.display.colors,
            })
            .await;
            repl.run(first).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// The newest conversation that is not archived, else a new one.
async fn latest(backend: &Backend) -> Result<SessionId> {
    let latest = backend
        .threads()
        .await?
        .into_iter()
        .find(|thread| !thread.archived)
        .map(|thread| thread.summary.id);
    Ok(latest.unwrap_or_else(|| {
        render::notice("no saved conversation; starting a new one");
        SessionId::new()
    }))
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
            let crew = match config.subagents.enabled {
                true => Some(Arc::new(Crew::new(config)?)),
                false => None,
            };
            let machine = Machine::here(config.file.as_deref());
            let subagents = crew.as_ref().map(|crew| crew.machine().to_owned());
            let client = Client::connect(name, remote, machine, subagents).await?;
            Ok(Backend::Remote(client, crew))
        }
        None => Ok(Backend::Local(Arc::new(
            Host::new(config, Hosting::Local).await?,
        ))),
    }
}
