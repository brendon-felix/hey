/* -------------------------------------------------------------------------- */
/*                                  retry.rs                                  */
/* -------------------------------------------------------------------------- */

//! Trying again what failed for a reason that may pass, as Zed's agent does:
//! the wait the other side asked for, else waits doubling from a base, with
//! jitter so clients that failed together do not all retry together.

use std::fmt;
use std::future::Future;
use std::time::{Duration, SystemTime};

use rig_agent::completion::PromptError;
use rig_core::error::ErrorReport;

/// Waits that double from `base`, for at most `attempts` retries.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    base: Duration,
    attempts: u32,
}

impl Backoff {
    /// A model call: providers' overloads and rate limits take a while.
    pub const MODEL: Self = Self {
        base: Duration::from_secs(5),
        attempts: 4,
    };
    /// A request to a host that never reached it.
    pub const REQUEST: Self = Self {
        base: Duration::from_millis(500),
        attempts: 3,
    };
    /// Following a run again, which is worth waiting a minute for.
    pub const RECONNECT: Self = Self {
        base: Duration::from_secs(1),
        attempts: 6,
    };
}

/// A wait longer than this is a quota, not a hiccup: the call fails instead.
const LONGEST_WAIT: Duration = Duration::from_secs(60);

/// The retries made of one thing since it last got anywhere.
#[derive(Debug)]
pub struct Attempts {
    backoff: Backoff,
    failed: u32,
}

/// The next try: which it is, and how long to wait for it.
#[derive(Debug, Clone, Copy)]
pub struct Retry {
    pub attempt: u32,
    pub of: u32,
    pub delay: Duration,
}

impl fmt::Display for Retry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "retrying in {:.0}s ({}/{})",
            self.delay.as_secs_f64().ceil(),
            self.attempt,
            self.of
        )
    }
}

impl Attempts {
    pub fn new(backoff: Backoff) -> Self {
        Self { backoff, failed: 0 }
    }

    /// The retry after another failure, waiting `asked` when the other side
    /// said how long; `None` once the retries are spent or the wait is too
    /// long to make.
    pub fn next(&mut self, asked: Option<Duration>) -> Option<Retry> {
        if self.failed >= self.backoff.attempts {
            return None;
        }
        self.failed += 1;
        let delay = match asked {
            Some(asked) if asked > LONGEST_WAIT => return None,
            Some(asked) => asked,
            None => jitter(self.backoff.base * 2u32.pow(self.failed - 1)),
        };
        Some(Retry {
            attempt: self.failed,
            of: self.backoff.attempts,
            delay,
        })
    }

    /// Starts counting again, as when what failed has since got somewhere.
    pub fn reset(&mut self) {
        self.failed = 0;
    }
}

/// `delay` and up to a fifth more.
fn jitter(delay: Duration) -> Duration {
    let unit = f64::from(getrandom::u32().unwrap_or_default()) / f64::from(u32::MAX);
    delay.mul_f64(1.0 + unit / 5.0)
}

/// A model call's failure that may pass on its own: a lost connection, a
/// rate limit, an overloaded or failing provider.
pub struct Transient {
    /// The wait the provider asked for.
    pub asked: Option<Duration>,
}

/// Whether the model call that failed with `error` may succeed if tried
/// again, as the provider's reply and rig classify it.
pub fn transient(error: &PromptError) -> Option<Transient> {
    let report = match error {
        PromptError::Provider(error) => ErrorReport::from(error),
        PromptError::Report(report) => report.clone(),
        _ => return None,
    };
    report.is_retryable().then(|| Transient {
        asked: report.retry_after(SystemTime::now()),
    })
}

/// `call`, made again while it fails for a reason that may pass.
pub async fn model<T, Call, Made>(what: &str, mut call: Call) -> Result<T, Box<PromptError>>
where
    Call: FnMut() -> Made,
    Made: Future<Output = Result<T, PromptError>>,
{
    let mut attempts = Attempts::new(Backoff::MODEL);
    loop {
        let error = match call().await {
            Ok(made) => return Ok(made),
            Err(error) => error,
        };
        let Some(retry) = transient(&error).and_then(|transient| attempts.next(transient.asked))
        else {
            return Err(Box::new(error));
        };
        tracing::warn!(error = crate::log::chain(&error), %retry, "{what} failed");
        tokio::time::sleep(retry.delay).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_double_until_the_retries_are_spent() {
        let mut attempts = Attempts::new(Backoff::MODEL);
        let delays: Vec<Duration> = std::iter::from_fn(|| attempts.next(None))
            .map(|retry| retry.delay)
            .collect();
        assert_eq!(delays.len(), 4);
        for (delay, base) in delays.iter().zip([5, 10, 20, 40]) {
            let base = Duration::from_secs(base);
            assert!(
                *delay >= base && *delay <= base.mul_f64(1.2),
                "{delay:?} for {base:?}"
            );
        }
        attempts.reset();
        assert!(attempts.next(None).is_some(), "counting starts again");
    }

    #[test]
    fn the_wait_asked_for_is_kept_unless_it_is_too_long() {
        let mut attempts = Attempts::new(Backoff::MODEL);
        let asked = Duration::from_secs(3);
        assert_eq!(
            attempts.next(Some(asked)).map(|retry| retry.delay),
            Some(asked)
        );
        assert!(attempts.next(Some(LONGEST_WAIT * 2)).is_none());
    }

    #[test]
    fn only_failures_that_may_pass_are_retried() {
        use http::StatusCode;
        use rig_core::error::ProviderError;

        let status =
            |status| PromptError::Provider(ProviderError::from_http_response(status, "nope"));
        assert!(transient(&status(StatusCode::TOO_MANY_REQUESTS)).is_some());
        assert!(transient(&status(StatusCode::SERVICE_UNAVAILABLE)).is_some());
        assert!(transient(&status(StatusCode::UNAUTHORIZED)).is_none());
        assert!(transient(&status(StatusCode::BAD_REQUEST)).is_none());
    }
}
