//! Dead-link checker: probe queued downloads and flag the ones that are gone
//! before a queue wastes a night on them.
//!
//! Reuses the capture probe (`jobs::probe`): one short request per link, up
//! to 8 at a time. A link is dead when the server answers 403/404/410, or
//! when only a browser gets in (a bot check). Anything else — timeout, 5xx,
//! sign-in required — is inconclusive, never dead: the link may be fine and
//! the check just couldn't tell.

use std::sync::Arc;

use futures_util::future::join_all;
use serde::Serialize;
use tokio::sync::Semaphore;

use super::capture::NewItem;
use super::jobs;
use super::Core;
use crate::events::CoreEvent;
use crate::model::{now_ms, ItemKind, LinkStatus, Status};

/// How many links are probed at once.
const CONCURRENCY: usize = 8;

/// One checked link, as the UI gets it back.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkCheck {
    pub id: String,
    pub status: LinkStatus,
    pub message: String,
}

fn outcome(f: &jobs::ProbeFailure) -> (LinkStatus, String) {
    if f.browser_only {
        return (
            LinkStatus::Dead,
            "Needs a browser — the site blocks Prism".into(),
        );
    }
    match f.status {
        Some(403) => (LinkStatus::Dead, "Forbidden (403)".into()),
        Some(404) => (LinkStatus::Dead, "Not found (404)".into()),
        Some(410) => (LinkStatus::Dead, "Gone (410)".into()),
        Some(s) => (LinkStatus::Unknown, format!("Server answered {s}")),
        None if f.needs_auth => (LinkStatus::Unknown, "Needs sign-in".into()),
        None => (LinkStatus::Unknown, f.message.clone()),
    }
}

impl Core {
    /// Probe `ids` (or every checkable item when `None`) and persist the
    /// outcome on each item. Returns one result per checked link.
    ///
    /// Only queued file downloads are checked: active and finished items are
    /// left alone, and video pages and torrents can't be probed this way.
    pub async fn check_links(&self, ids: Option<Vec<String>>) -> Vec<LinkCheck> {
        let targets: Vec<(String, NewItem)> = {
            let st = self.0.state.lock();
            let mut v = Vec::new();
            for item in st.items.values() {
                if item.trashed {
                    continue;
                }
                if let Some(ids) = &ids
                    && !ids.contains(&item.id) {
                    continue;
                }
                if !matches!(
                    item.status,
                    Status::Idle | Status::Queued | Status::Paused | Status::Failed
                ) {
                    continue;
                }
                if !matches!(
                    item.kind,
                    ItemKind::Http | ItemKind::Hls | ItemKind::Dash
                ) {
                    continue;
                }
                let url = item
                    .final_url
                    .clone()
                    .unwrap_or_else(|| item.url.clone());
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    continue;
                }
                // The cookies the item was added with (e.g. from the browser):
                // the stored request is a serialized `DownloadRequest`.
                let cookies = item
                    .request
                    .as_ref()
                    .and_then(|r| r.get("cookies"))
                    .and_then(|c| c.as_str())
                    .map(str::to_string);
                v.push((
                    item.id.clone(),
                    NewItem {
                        url,
                        file_name: Some(item.file_name.clone()),
                        referer: item.referer.clone(),
                        cookies,
                        ..Default::default()
                    },
                ));
            }
            v
        };
        let sem = Arc::new(Semaphore::new(CONCURRENCY));
        let jobs = targets.into_iter().map(|(id, n)| {
            let core = self.clone();
            let sem = sem.clone();
            async move {
                let _permit = sem.acquire().await.ok();
                let settings = core.settings();
                let checked_at = now_ms();
                let (status, message) = match jobs::probe(&core, &n, &settings).await {
                    Ok(_) => (LinkStatus::Ok, "Link is alive".to_string()),
                    Err(f) => outcome(&f),
                };
                let updated = core.with_item(&id, |item| {
                    item.link_status = Some(status);
                    item.link_checked_at = Some(checked_at as u64);
                    item.clone()
                });
                if let Some(item) = updated {
                    core.emit(CoreEvent::ItemUpdated(item));
                }
                LinkCheck {
                    id,
                    status,
                    message,
                }
            }
        });
        join_all(jobs).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manager::capture::StartMode;
    use crate::manager::CoreOptions;

    #[tokio::test]
    async fn check_links_flags_a_404_and_passes_a_live_file() {
        let dir = tempfile::tempdir().unwrap();
        let core = Core::start(CoreOptions {
            data_dir: dir.path().to_path_buf(),
            ipc: false,
            clipboard: false,
        })
        .await
        .unwrap();
        let server =
            prism_testserver::TestServer::start(prism_testserver::ServerConfig::default())
                .await
                .unwrap();
        let alive = server.url("/blob/alive.bin");
        let gone = server.url("/blob/no-such-file.bin");
        server.put_blob("alive.bin", b"alive".to_vec(), "application/octet-stream");
        let items = core
            .add_items(
                vec![
                    NewItem {
                        url: alive,
                        ..Default::default()
                    },
                    NewItem {
                        url: gone,
                        ..Default::default()
                    },
                ],
                StartMode::Later,
            )
            .unwrap();
        let results = core.check_links(None).await;
        assert_eq!(results.len(), 2);
        let by_id: std::collections::HashMap<_, _> =
            results.into_iter().map(|r| (r.id.clone(), r)).collect();
        assert_eq!(
            by_id[&items[0].id].status,
            LinkStatus::Ok,
            "live file must pass"
        );
        assert_eq!(
            by_id[&items[1].id].status,
            LinkStatus::Dead,
            "missing file must be flagged dead"
        );
        // The outcome is persisted on the items, not just returned.
        let st = core.0.state.lock();
        for item in &items {
            let stored = st.items.get(&item.id).unwrap();
            assert_eq!(stored.link_status, Some(by_id[&item.id].status));
            assert!(stored.link_checked_at.is_some());
        }
    }
}
