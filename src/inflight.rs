use std::{collections::HashMap, future::Future, sync::Arc};

use tokio::sync::{Mutex, Notify};

use crate::google::GoogleError;

#[derive(Clone, Default)]
pub struct InflightRequests {
    entries: Arc<Mutex<HashMap<String, Arc<InflightEntry>>>>,
}

impl InflightRequests {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn run<F, Fut>(&self, key: String, operation: F) -> Result<String, GoogleError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<String, GoogleError>>,
    {
        let (entry, leader) = {
            let mut entries = self.entries.lock().await;
            if let Some(entry) = entries.get(&key) {
                (entry.clone(), false)
            } else {
                let entry = Arc::new(InflightEntry::default());
                entries.insert(key.clone(), entry.clone());
                (entry, true)
            }
        };

        if !leader {
            return entry.wait().await;
        }

        let result = operation().await;
        entry.finish(result.clone()).await;

        let mut entries = self.entries.lock().await;
        entries.remove(&key);

        result
    }
}

#[derive(Default)]
struct InflightEntry {
    notify: Notify,
    result: Mutex<Option<Result<String, GoogleError>>>,
}

impl InflightEntry {
    async fn wait(&self) -> Result<String, GoogleError> {
        loop {
            if let Some(result) = self.result.lock().await.clone() {
                return result;
            }

            self.notify.notified().await;
        }
    }

    async fn finish(&self, result: Result<String, GoogleError>) {
        *self.result.lock().await = Some(result);
        self.notify.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use tokio::time::sleep;

    use super::InflightRequests;
    use crate::google::GoogleError;

    #[tokio::test]
    async fn coalesces_duplicate_inflight_requests() {
        let inflight = InflightRequests::new();
        let calls = Arc::new(AtomicUsize::new(0));

        let first_calls = calls.clone();
        let first = {
            let inflight = inflight.clone();
            tokio::spawn(async move {
                inflight
                    .run("sheet-key".to_string(), move || async move {
                        first_calls.fetch_add(1, Ordering::SeqCst);
                        sleep(Duration::from_millis(50)).await;
                        Ok("[{\"name\":\"alice\"}]".to_string())
                    })
                    .await
            })
        };

        let second = {
            let inflight = inflight.clone();
            tokio::spawn(async move {
                inflight
                    .run("sheet-key".to_string(), move || async move {
                        Err(GoogleError {
                            message: "should not execute".to_string(),
                            status: 500,
                        })
                    })
                    .await
            })
        };

        let first_result = first.await.expect("first task panicked");
        let second_result = second.await.expect("second task panicked");

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(first_result.unwrap(), "[{\"name\":\"alice\"}]");
        assert_eq!(second_result.unwrap(), "[{\"name\":\"alice\"}]");
    }

    #[tokio::test]
    async fn propagates_errors_to_duplicate_waiters() {
        let inflight = InflightRequests::new();

        let first = {
            let inflight = inflight.clone();
            tokio::spawn(async move {
                inflight
                    .run("sheet-key".to_string(), move || async move {
                        sleep(Duration::from_millis(25)).await;
                        Err(GoogleError {
                            message: "upstream failed".to_string(),
                            status: 502,
                        })
                    })
                    .await
            })
        };

        let second = {
            let inflight = inflight.clone();
            tokio::spawn(async move {
                inflight
                    .run("sheet-key".to_string(), move || async move {
                        Ok("should not execute".to_string())
                    })
                    .await
            })
        };

        let first_result = first.await.expect("first task panicked");
        let second_result = second.await.expect("second task panicked");

        assert_eq!(first_result.unwrap_err().message, "upstream failed");
        assert_eq!(second_result.unwrap_err().message, "upstream failed");
    }
}
