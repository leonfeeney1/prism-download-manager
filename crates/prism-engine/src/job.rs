//! State shared between a running job and its [`DownloadHandle`]s.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

use crate::error::EngineError;
use crate::limiter::RateLimiter;
use crate::media::MediaProgress;
use crate::progress::{Completed, DownloadEvent, DownloadState, ProgressSnapshot};
use crate::resume::{RESUME_STATE_VERSION, ResumeKind, ResumeState};
use crate::segmented::table::SegmentTable;
use crate::segmented::writer::Writer;
use crate::speed::SpeedMeter;

/// Control messages from handles to the job task.
#[derive(Debug)]
pub(crate) enum Command {
    SetConnections(u8),
    ChangeUrl(Url, oneshot::Sender<Result<(), EngineError>>),
    /// Validate and add a mirror to a running download (same-file guard applies; the
    /// source joins the rotation for new requests only, live streams are untouched).
    AddSource(Url, oneshot::Sender<Result<(), EngineError>>),
}

/// Why a job is being stopped early.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopReason {
    Pause,
    Cancel { delete_file: bool },
}

/// Mutable metadata of a job.
#[derive(Debug)]
pub(crate) struct JobMeta {
    pub state: DownloadState,
    pub kind: ResumeKind,
    pub url: Url,
    pub final_url: Option<Url>,
    pub part_path: Option<PathBuf>,
    pub final_path: Option<PathBuf>,
    pub total: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub resumable: bool,
    pub speed: u64,
    pub eta: Option<Duration>,
    pub active: u32,
    pub target: u32,
    meter: SpeedMeter,
}

/// Where progress information comes from once the transfer started.
#[derive(Clone)]
pub(crate) enum ProgressSource {
    Segmented {
        table: Arc<SegmentTable>,
        writer: Arc<Writer>,
    },
    Media(Arc<MediaProgress>),
}

/// Shared job state.
pub(crate) struct JobShared {
    pub id: Uuid,
    pub started: Instant,
    pub events: broadcast::Sender<DownloadEvent>,
    first_rx: Mutex<Option<broadcast::Receiver<DownloadEvent>>>,
    pub cmd_tx: mpsc::UnboundedSender<Command>,
    pub limiter: Arc<RateLimiter>,
    pub stop: CancellationToken,
    stop_reason: Mutex<Option<StopReason>>,
    pub meta: Mutex<JobMeta>,
    source: Mutex<Option<ProgressSource>>,
    /// Live source list of an HTTP job (set once its context exists); `None` for other jobs.
    sources: Mutex<Option<Arc<crate::http::Sources>>>,
    final_state: Mutex<Option<ResumeState>>,
    result: watch::Sender<Option<Result<Completed, EngineError>>>,
}

impl JobShared {
    pub(crate) fn new(
        url: Url,
        kind: ResumeKind,
        speed_limit: u64,
    ) -> (Arc<JobShared>, mpsc::UnboundedReceiver<Command>) {
        let (events, first_rx) = broadcast::channel(512);
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (result, _) = watch::channel(None);
        let job = Arc::new(JobShared {
            id: Uuid::new_v4(),
            started: Instant::now(),
            events,
            first_rx: Mutex::new(Some(first_rx)),
            cmd_tx,
            limiter: Arc::new(RateLimiter::new(speed_limit)),
            stop: CancellationToken::new(),
            stop_reason: Mutex::new(None),
            meta: Mutex::new(JobMeta {
                state: DownloadState::Probing,
                kind,
                url,
                final_url: None,
                part_path: None,
                final_path: None,
                total: None,
                etag: None,
                last_modified: None,
                resumable: false,
                speed: 0,
                eta: None,
                active: 0,
                target: 0,
                meter: SpeedMeter::default(),
            }),
            source: Mutex::new(None),
            sources: Mutex::new(None),
            final_state: Mutex::new(None),
            result,
        });
        (job, cmd_rx)
    }

    pub(crate) fn meta(&self) -> std::sync::MutexGuard<'_, JobMeta> {
        self.meta.lock().expect("job meta lock")
    }

    pub(crate) fn emit(&self, ev: DownloadEvent) {
        let _ = self.events.send(ev);
    }

    pub(crate) fn set_state(&self, state: DownloadState) {
        let changed = {
            let mut m = self.meta();
            let changed = m.state != state;
            m.state = state;
            changed
        };
        if changed {
            self.emit(DownloadEvent::StateChanged { state });
        }
    }

    pub(crate) fn set_source(&self, s: ProgressSource) {
        *self.source.lock().expect("source lock") = Some(s);
    }

    pub(crate) fn set_sources(&self, s: Arc<crate::http::Sources>) {
        *self.sources.lock().expect("sources lock") = Some(s);
    }

    pub(crate) fn source(&self) -> Option<ProgressSource> {
        self.source.lock().expect("source lock").clone()
    }

    pub(crate) fn request_stop(&self, reason: StopReason) {
        let mut r = self.stop_reason.lock().expect("stop lock");
        // Cancel wins over pause; the first cancel's delete flag sticks unless upgraded.
        let next = match (*r, reason) {
            (
                Some(StopReason::Cancel { delete_file: a }),
                StopReason::Cancel { delete_file: b },
            ) => StopReason::Cancel {
                delete_file: a || b,
            },
            (Some(c @ StopReason::Cancel { .. }), StopReason::Pause) => c,
            (_, x) => x,
        };
        *r = Some(next);
        drop(r);
        self.stop.cancel();
    }

    pub(crate) fn stop_reason(&self) -> Option<StopReason> {
        *self.stop_reason.lock().expect("stop lock")
    }

    /// Recomputes speed/ETA from the byte counter (called by the job's tick).
    pub(crate) fn update_rates(
        &self,
        downloaded: u64,
        total: Option<u64>,
        active: u32,
        target: u32,
    ) {
        let mut m = self.meta();
        let speed = m.meter.update(downloaded, Instant::now());
        m.speed = speed;
        m.total = total.or(m.total);
        m.active = active;
        m.target = target;
        m.eta = match (m.total, speed) {
            (Some(t), s) if s > 0 => Some(Duration::from_secs_f64(
                t.saturating_sub(downloaded) as f64 / s as f64,
            )),
            _ => None,
        };
    }

    /// Builds a resume state from metadata and the given segments.
    pub(crate) fn build_resume(
        &self,
        segments: Vec<crate::resume::SegmentState>,
    ) -> Option<ResumeState> {
        let m = self.meta();
        Some(ResumeState {
            version: RESUME_STATE_VERSION,
            kind: m.kind,
            url: m.url.clone(),
            final_url: m.final_url.clone(),
            part_path: m.part_path.clone()?,
            final_path: m.final_path.clone()?,
            total_size: m.total,
            etag: m.etag.clone(),
            last_modified: m.last_modified.clone(),
            resumable: m.resumable,
            segments,
            media: None,
        })
    }

    /// In-memory resume state (not necessarily durable).
    pub(crate) fn resume_state(&self) -> Option<ResumeState> {
        if let Some(s) = self.final_state.lock().expect("final state lock").clone() {
            return Some(s);
        }
        match self.source()? {
            ProgressSource::Segmented { table, .. } => self.build_resume(table.durable_states()),
            ProgressSource::Media(m) => m.resume_state(self),
        }
    }

    /// Durable resume state: flushes and fsyncs first.
    pub(crate) async fn checkpoint(&self) -> Option<ResumeState> {
        if self.result.borrow().is_some() {
            return self.final_state.lock().expect("final state lock").clone();
        }
        match self.source()? {
            ProgressSource::Segmented { writer, .. } => {
                let segs = writer.flush(true).await.ok()?;
                self.build_resume(segs)
            }
            ProgressSource::Media(m) => m.resume_state(self),
        }
    }

    pub(crate) fn snapshot(&self) -> ProgressSnapshot {
        let source = self.source();
        let (downloaded, segments, media_total) = match &source {
            Some(ProgressSource::Segmented { table, .. }) => {
                (table.downloaded(), table.views(), None)
            }
            Some(ProgressSource::Media(m)) => (m.downloaded(), m.views(), m.total_estimate()),
            None => (0, Vec::new(), None),
        };
        let m = self.meta();
        ProgressSnapshot {
            state: m.state,
            downloaded,
            total: media_total.or(m.total),
            speed: m.speed,
            eta: m.eta,
            active_connections: m.active,
            target_connections: m.target,
            resumable: m.resumable,
            file_name: m
                .final_path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned()),
            path: m.final_path.clone(),
            final_url: m.final_url.clone(),
            elapsed: self.started.elapsed(),
            segments,
            sources: self
                .sources
                .lock()
                .expect("sources lock")
                .as_ref()
                .map(|s| s.snapshot())
                .unwrap_or_default(),
        }
    }

    /// Publishes the final result. `final_state` is kept for `resume_state()` after the end.
    pub(crate) fn finish(
        &self,
        result: Result<Completed, EngineError>,
        final_state: Option<ResumeState>,
    ) {
        *self.final_state.lock().expect("final state lock") = final_state;
        let state = match &result {
            Ok(_) => DownloadState::Completed,
            Err(EngineError::Paused) => DownloadState::Paused,
            Err(EngineError::Cancelled) => DownloadState::Cancelled,
            Err(_) => DownloadState::Failed,
        };
        {
            let mut m = self.meta();
            m.speed = 0;
            m.eta = None;
            m.active = 0;
        }
        self.set_state(state);
        match &result {
            Ok(c) => self.emit(DownloadEvent::Completed(c.clone())),
            Err(EngineError::Paused) | Err(EngineError::Cancelled) => {}
            Err(e) => self.emit(DownloadEvent::Failed { error: e.clone() }),
        }
        self.result.send_replace(Some(result));
    }

    pub(crate) fn is_finished(&self) -> bool {
        self.result.borrow().is_some()
    }
}

/// Control and observation handle of one download job. Cheap to clone; dropping all handles
/// does not stop the job.
#[derive(Clone)]
pub struct DownloadHandle {
    pub(crate) shared: Arc<JobShared>,
}

impl std::fmt::Debug for DownloadHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DownloadHandle")
            .field("id", &self.shared.id)
            .finish()
    }
}

impl DownloadHandle {
    /// Unique id of this job run.
    pub fn id(&self) -> Uuid {
        self.shared.id
    }

    /// Requests a graceful pause without waiting: workers stop, buffers are flushed and the
    /// part file is fsynced. [`DownloadHandle::wait`] then returns `Err(EngineError::Paused)`.
    pub fn request_pause(&self) {
        self.shared.request_stop(StopReason::Pause);
    }

    /// Pauses gracefully and waits until the job has stopped. Returns the final, durable resume
    /// state (`None` if the job had not created its file yet or already finished otherwise).
    pub async fn pause(&self) -> Option<ResumeState> {
        if !self.shared.is_finished() {
            self.request_pause();
        }
        let _ = self.wait().await;
        self.shared.resume_state()
    }

    /// Stops the job. With `delete_file` the partial file (or media work directory) is removed.
    pub fn cancel(&self, delete_file: bool) {
        self.shared.request_stop(StopReason::Cancel { delete_file });
    }

    /// Changes the number of connections live (capped by the engine).
    pub fn set_connections(&self, n: u8) {
        let _ = self.shared.cmd_tx.send(Command::SetConnections(n));
    }

    /// Validates a new source for a running download (same size, range support) and adds
    /// it to the rotation for new requests. Fails for jobs without a running HTTP
    /// transfer, and for a source that is already listed.
    pub async fn add_source(&self, url: Url) -> Result<(), EngineError> {
        let (tx, rx) = oneshot::channel();
        self.shared
            .cmd_tx
            .send(Command::AddSource(url, tx))
            .map_err(|_| EngineError::internal("the download is not running"))?;
        rx.await.map_err(|_| {
            EngineError::internal("the download stopped before the source could be added")
        })?
    }

    /// Changes the per-download speed limit live (bytes/s, 0 = unlimited).
    pub fn set_speed_limit(&self, bytes_per_sec: u64) {
        self.shared.limiter.set_rate(bytes_per_sec);
    }

    /// Current per-download speed limit.
    pub fn speed_limit(&self) -> u64 {
        self.shared.limiter.rate()
    }

    /// Replaces the download address of a running download ("refresh address"). The new URL
    /// is validated (same size; same strong ETag when both have one) and then used by every
    /// connection. For a paused download use [`ResumeState::with_url`] instead.
    pub async fn change_url(&self, url: Url) -> Result<(), EngineError> {
        let (tx, rx) = oneshot::channel();
        self.shared
            .cmd_tx
            .send(Command::ChangeUrl(url, tx))
            .map_err(|_| EngineError::internal("the download is not running"))?;
        rx.await.map_err(|_| {
            EngineError::internal("the download stopped before the URL could be changed")
        })?
    }

    /// Current progress.
    pub fn snapshot(&self) -> ProgressSnapshot {
        self.shared.snapshot()
    }

    /// Lifecycle state.
    pub fn state(&self) -> DownloadState {
        self.shared.meta().state
    }

    /// Consistent in-memory resume state: only bytes already handed to the OS are claimed, but
    /// they are not necessarily fsynced. Use [`DownloadHandle::checkpoint`] for crash safety.
    pub fn resume_state(&self) -> Option<ResumeState> {
        self.shared.resume_state()
    }

    /// Flushes buffered data, fsyncs the part file and returns a resume state that only claims
    /// bytes durably on disk. Persist this one (it stays valid after a crash or power loss).
    pub async fn checkpoint(&self) -> Option<ResumeState> {
        self.shared.checkpoint().await
    }

    /// Event stream. The first call returns a receiver that has buffered every event since
    /// `start` (so `Probed` is never missed); later calls subscribe from now on.
    pub fn events(&self) -> broadcast::Receiver<DownloadEvent> {
        if let Some(rx) = self.shared.first_rx.lock().expect("events lock").take() {
            return rx;
        }
        self.shared.events.subscribe()
    }

    /// `true` once the job has stopped (completed, failed, paused or cancelled).
    pub fn is_finished(&self) -> bool {
        self.shared.is_finished()
    }

    /// Waits for the job to end. Pausing yields `Err(EngineError::Paused)`, cancelling
    /// `Err(EngineError::Cancelled)`.
    pub async fn wait(&self) -> Result<Completed, EngineError> {
        let mut rx = self.shared.result.subscribe();
        let r = rx.wait_for(|r| r.is_some()).await;
        match r {
            Ok(v) => v.clone().expect("checked by wait_for"),
            Err(_) => Err(EngineError::internal("job dropped")),
        }
    }
}
