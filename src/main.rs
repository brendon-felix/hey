/* -------------------------------------------------------------------------- */
/*                                  main.rs                                   */
/* -------------------------------------------------------------------------- */

mod agent;
mod config;
mod mcp;
mod permissions;
mod render;
mod repl;
mod sessions;
mod turn;
mod ui;

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result};
use clap::Parser;
use yansi::Paint;

use agent::{Assistant, Model};
use config::Config;
use repl::{Repl, ServerReport};
use sessions::{SessionId, Sessions};

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

    /// Read this config file instead of the default one
    #[arg(long)]
    config: Option<PathBuf>,

    /// Continue the most recent conversation
    #[arg(long, short)]
    resume: bool,

    /// Stay in the REPL after answering MESSAGE
    #[arg(long, short)]
    interactive: bool,

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

    let system_prompt = match &args.prompt_path {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("cannot read system prompt {}", path.display()))?,
        None => config
            .system_prompt
            .clone()
            .unwrap_or_else(|| agent::DEFAULT_SYSTEM_PROMPT.into()),
    };
    let reference = args.model.as_deref().unwrap_or(&config.model);
    let model = Model::connect(reference, &config.providers)?;
    let sessions = Sessions::open(config.sessions.dir()?)?;
    let session = match args.resume {
        true => sessions.latest()?.unwrap_or_else(|| {
            render::notice("no saved conversation; starting a new one");
            SessionId::new()
        }),
        false => SessionId::new(),
    };

    let (tools, servers) = connect_mcp(&config).await;
    let assistant = Assistant::new(&config, model, &system_prompt, &sessions, tools);
    let style = render::Style::from(&config.display);
    let message = (!args.message.is_empty()).then(|| args.message.join(" "));

    match message {
        Some(message) if !args.interactive => {
            let turn = turn::run(&assistant, &message, &session, &style).await;
            Ok(match turn.outcome {
                turn::Outcome::Answered => ExitCode::SUCCESS,
                turn::Outcome::Interrupted | turn::Outcome::Failed => ExitCode::FAILURE,
            })
        }
        first => {
            let repl = Repl::new(repl::Setup {
                assistant,
                sessions,
                session,
                providers: config.providers.clone(),
                servers,
                style,
                greetings: config.greetings,
                editor: config.editor,
                colors: config.display.colors,
            });
            repl.run(first).await?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// The tools of every server that connected, and how each connection went.
async fn connect_mcp(config: &Config) -> (Vec<mcp::Tool>, Vec<ServerReport>) {
    let mut tools = Vec::new();
    let mut reports = Vec::new();
    for server in mcp::connect_all(&config.mcp_servers).await {
        let outcome = server.tools.map(|server_tools| {
            let count = server_tools.len();
            tools.extend(server_tools);
            count
        });
        if let Err(error) = &outcome {
            render::notice(&format!(
                "MCP server `{}` unavailable: {}",
                server.name,
                error.root_cause()
            ));
        }
        reports.push(ServerReport {
            name: server.name,
            tools: outcome.map_err(|error| error.root_cause().to_string()),
        });
    }
    (tools, reports)
}
