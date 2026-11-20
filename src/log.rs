/* -------------------------------------------------------------------------- */
/*                                   log.rs                                   */
/* -------------------------------------------------------------------------- */

//! What hey records of what it does, through `tracing`: its own events and
//! rig's spans (https://rig.rs/docs/concepts/observability), each closing
//! with its fields and timing, in a file a day. The terminal is the
//! conversation's, so nothing is logged to it.

use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::format::FmtSpan;

use crate::config;

/// The environment variable whose filter directives override the config's.
const ENV: &str = "HEY_LOG";

/// Days of logs kept.
const KEPT: usize = 7;

static CONTENT: AtomicBool = AtomicBool::new(false);

/// Starts logging as `config` says, until the returned guard is dropped,
/// which writes out what is still buffered.
pub fn init(config: &config::Log) -> Result<WorkerGuard> {
    CONTENT.store(config.content, Ordering::Relaxed);
    let dir = config.dir()?;
    // The appender reports a missing directory on stderr, over the conversation.
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let file = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("hey")
        .filename_suffix("log")
        .max_log_files(KEPT)
        .build(&dir)
        .with_context(|| format!("cannot log to {}", dir.display()))?;
    let (writer, guard) = tracing_appender::non_blocking(file);
    let filter = match std::env::var(ENV) {
        Ok(directives) => {
            EnvFilter::try_new(&directives).with_context(|| format!("invalid `{ENV}`"))?
        }
        Err(_) => EnvFilter::try_new(&config.filter).context("invalid `log.filter`")?,
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .with_span_events(FmtSpan::CLOSE)
        .try_init()
        .map_err(|error| anyhow::anyhow!(error))?;
    record_panics();
    Ok(guard)
}

/// Records a panic before the default hook reports it, which the terminal
/// may lose behind the conversation.
fn record_panics() {
    let report = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = std::backtrace::Backtrace::force_capture();
        tracing::error!(%info, %backtrace, "panicked");
        report(info);
    }));
}

/// `error` and its sources, each told once: many errors repeat their
/// source's message in their own.
pub fn chain(error: &(dyn std::error::Error + 'static)) -> String {
    let mut told = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        let message = cause.to_string();
        if !told.contains(&message) {
            told.push_str(": ");
            told.push_str(&message);
        }
        source = cause.source();
    }
    told
}

/// Whether agents record what is said, not only what happens.
pub fn content() -> bool {
    CONTENT.load(Ordering::Relaxed)
}
