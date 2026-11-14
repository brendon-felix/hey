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
        Ok(directives) => EnvFilter::try_new(&directives).with_context(|| format!("invalid `{ENV}`"))?,
        Err(_) => EnvFilter::try_new(&config.filter).context("invalid `log.filter`")?,
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .with_span_events(FmtSpan::CLOSE)
        .try_init()
        .map_err(|error| anyhow::anyhow!(error))?;
    Ok(guard)
}

/// Whether agents record what is said, not only what happens.
pub fn content() -> bool {
    CONTENT.load(Ordering::Relaxed)
}
