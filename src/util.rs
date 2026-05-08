use std::future::Future;
use std::time::Duration;
use tracing::{debug, warn};

/// Retry an async operation with exponential backoff.
pub async fn retry<F, Fut, T, E>(name: &str, max_attempts: usize, f: F) -> Result<T, E>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: std::fmt::Display,
{
    let mut delay = Duration::from_millis(100);
    let max_delay = Duration::from_secs(5);

    for attempt in 1..=max_attempts {
        match f().await {
            Ok(result) => {
                if attempt > 1 {
                    debug!("{} succeeded on attempt {}", name, attempt);
                }
                return Ok(result);
            }
            Err(e) => {
                if attempt == max_attempts {
                    return Err(e);
                }
                warn!(
                    "{} failed on attempt {}/{}: {}. Retrying in {:?}...",
                    name, attempt, max_attempts, e, delay
                );
                tokio::time::sleep(delay).await;
                delay = std::cmp::min(delay * 2, max_delay);
            }
        }
    }

    unreachable!()
}
