use anyhow::Result;
use notify::{Config, Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use tokio::sync::mpsc;
use tracing::info;

#[async_trait::async_trait]
pub trait Handler: Send + Sync {
    async fn on_change(&self, file_path: &str);
    async fn on_delete(&self, file_path: &str);
}

pub struct FileWatcher;

impl FileWatcher {
    pub async fn watch(path: impl AsRef<Path>, handler: impl Handler + 'static) -> Result<()> {
        let (tx, mut rx) = mpsc::channel(1024);

        let mut watcher = RecommendedWatcher::new(
            move |res: Result<Event, notify::Error>| {
                if let Ok(event) = res {
                    let _ = tx.blocking_send(event);
                }
            },
            Config::default(),
        )?;

        watcher.watch(path.as_ref(), RecursiveMode::Recursive)?;
        info!("watching {:?}", path.as_ref());

        while let Some(event) = rx.recv().await {
            for path in event.paths {
                let path_str = path.to_string_lossy().to_string();
                match event.kind {
                    notify::EventKind::Modify(_) | notify::EventKind::Create(_) => {
                        handler.on_change(&path_str).await;
                    }
                    notify::EventKind::Remove(_) => {
                        handler.on_delete(&path_str).await;
                    }
                    _ => {}
                }
            }
        }

        Ok(())
    }
}
