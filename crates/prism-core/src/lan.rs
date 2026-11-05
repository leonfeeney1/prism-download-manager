//! LAN sharing: serve completed downloads to other devices on the local network.
//!
//! Opt-in only (off by default). When on, a small axum server binds the machine's
//! LAN address — never a public one — and serves a plain listing plus the files
//! themselves. The server never serves by path: every request resolves an item id
//! through a [`LanSource`], which only ever returns completed, user-shared files,
//! so in-progress `.part` files, settings and the database can never leak out.
//! Stopping is a drop: taking the server out of the manager shuts it down.

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{self, HeaderMap};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio_util::io::ReaderStream;

/// One file the LAN server may serve.
#[derive(Debug, Clone, PartialEq)]
pub struct SharedFile {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub size: u64,
}

/// Something that lists the files currently shared (implemented by the manager).
/// The server re-asks on every request, so unsharing or deleting a file takes
/// effect immediately.
pub trait LanSource: Send + Sync + 'static {
    fn shared_files(&self) -> Vec<SharedFile>;
}

/// Status shown in the UI.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LanStatus {
    pub enabled: bool,
    pub running: bool,
    pub address: Option<String>,
    pub port: u16,
    pub shared_count: usize,
    /// Address of the phone remote control page, when the server is running.
    pub remote_url: Option<String>,
}

/// One download, as the phone remote control sees it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RemoteItem {
    pub id: String,
    pub name: String,
    pub status: String,
    pub size: Option<u64>,
    pub downloaded: u64,
}

/// What the phone remote control may do. Implemented by the manager; the
/// LAN server only ever talks to the app through this trait, never the disk.
pub trait RemoteControl: Send + Sync + 'static {
    fn remote_items(&self) -> Vec<RemoteItem>;
    /// Pause a download; `false` when the id is unknown.
    fn remote_pause(&self, id: &str) -> bool;
    /// Resume (start) a download; `false` when the id is unknown.
    fn remote_resume(&self, id: &str) -> bool;
    /// Add a download from an address; the new item id, or `None` when the
    /// address isn't one Prism can download.
    fn remote_add(&self, url: &str) -> Option<String>;
}

pub struct LanServer {
    pub addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

/// Everything the server's handlers share.
struct ServerState {
    source: Arc<dyn LanSource>,
    control: Arc<dyn RemoteControl>,
    token: String,
}

impl LanServer {
    /// Bind `addr` and start serving. Returns once the socket is bound.
    ///
    /// `control` is what the phone remote control (`/remote` and `/api/*`)
    /// talks to; `token` is its bearer token (empty means the API stays
    /// locked: every `/api/*` call answers 401).
    pub async fn start(
        source: Arc<dyn LanSource>,
        control: Arc<dyn RemoteControl>,
        token: String,
        addr: SocketAddr,
    ) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let bound = listener.local_addr()?;
        let state = Arc::new(ServerState {
            source,
            control,
            token,
        });
        let app = Router::new()
            .route("/", get(index))
            .route("/files/{id}", get(file))
            .route("/remote", get(remote_page))
            .route("/api/status", get(api_status))
            .route("/api/items", get(api_items).post(api_add))
            .route("/api/items/{id}/pause", post(api_pause))
            .route("/api/items/{id}/resume", post(api_resume))
            .with_state(state);
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let server = axum::serve(listener, app).with_graceful_shutdown(async {
                let _ = rx.await;
            });
            if let Err(e) = server.await {
                tracing::error!("lan server stopped: {e}");
            }
        });
        tracing::info!(%bound, "lan server listening");
        Ok(Self {
            addr: bound,
            shutdown: Some(tx),
        })
    }
}

impl Drop for LanServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

/// The machine's LAN address, if it has one. A UDP "connect" sends no packets;
/// it only asks the OS which interface would carry traffic, which is exactly
/// the address another device on the network reaches us on.
pub fn local_lan_ip() -> Option<IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("8.8.8.8:80").ok()?;
    let ip = sock.local_addr().ok()?.ip();
    if ip.is_loopback() || ip.is_unspecified() {
        None
    } else {
        Some(ip)
    }
}

fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

fn human_size(size: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = size as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{size} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

async fn index(State(state): State<Arc<ServerState>>) -> Response {
    let files = state.source.shared_files();
    let mut rows = String::new();
    for f in &files {
        rows.push_str(&format!(
            "<li><a href=\"/files/{}\">{} ({})</a></li>",
            escape_html(&f.id),
            escape_html(&f.name),
            human_size(f.size)
        ));
    }
    if rows.is_empty() {
        rows.push_str("<li>No files shared yet.</li>");
    }
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>Shared downloads</title></head>\
        <body><h1>Shared downloads</h1><ul>{rows}</ul>\
        <p>Files shared from Prism on this local network.</p>\
        <p><a href=\"/remote\">Remote control</a> — pause, resume and add downloads from your phone.</p></body></html>"
    );
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response()
}

/// Parse a single `Range: bytes=...` header. Returns (start, end_inclusive).
pub(crate) fn parse_range(value: &str, len: u64) -> Option<(u64, u64)> {
    let spec = value.strip_prefix("bytes=")?.trim();
    if spec.contains(',') {
        return None; // multi-range: not supported
    }
    let (start_s, end_s) = spec.split_once('-')?;
    if start_s.is_empty() {
        // Suffix range: the last N bytes.
        let n: u64 = end_s.parse().ok()?;
        if n == 0 || len == 0 {
            return None;
        }
        let n = n.min(len);
        return Some((len - n, len - 1));
    }
    let start: u64 = start_s.parse().ok()?;
    if start >= len {
        return None;
    }
    let end = if end_s.is_empty() {
        len - 1
    } else {
        end_s.parse::<u64>().ok()?.min(len - 1)
    };
    if end < start {
        return None;
    }
    Some((start, end))
}

async fn file(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(shared) = state.source.shared_files().into_iter().find(|f| f.id == id) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let Ok(mut f) = tokio::fs::File::open(&shared.path).await else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let Ok(meta) = f.metadata().await else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    if !meta.is_file() {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let len = meta.len();
    let safe_name: String = shared
        .name
        .chars()
        .map(|c| if c == '"' || c == '\\' || c.is_control() { '_' } else { c })
        .collect();
    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    resp_headers.insert(
        header::CONTENT_TYPE,
        "application/octet-stream".parse().unwrap(),
    );
    resp_headers.insert(
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"{safe_name}\"")
            .parse()
            .unwrap_or_else(|_| "attachment".parse().unwrap()),
    );
    let (start, end, status) = match headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
    {
        Some(range) => match parse_range(range, len) {
            Some((s, e)) => {
                resp_headers.insert(
                    header::CONTENT_RANGE,
                    format!("bytes {s}-{e}/{len}").parse().unwrap(),
                );
                (s, e, StatusCode::PARTIAL_CONTENT)
            }
            None => {
                resp_headers.insert(
                    header::CONTENT_RANGE,
                    format!("bytes */{len}").parse().unwrap(),
                );
                return (StatusCode::RANGE_NOT_SATISFIABLE, resp_headers, "range not satisfiable")
                    .into_response();
            }
        },
        None => (0, len.saturating_sub(1), StatusCode::OK),
    };
    let count = end.saturating_sub(start) + 1;
    if len == 0 {
        resp_headers.insert(header::CONTENT_LENGTH, "0".parse().unwrap());
        return (status, resp_headers, Body::empty()).into_response();
    }
    if f.seek(std::io::SeekFrom::Start(start)).await.is_err() {
        return (StatusCode::INTERNAL_SERVER_ERROR, "seek failed").into_response();
    }
    let stream = ReaderStream::new(f.take(count)).map(|r| r.map_err(std::io::Error::other));
    resp_headers.insert(
        header::CONTENT_LENGTH,
        count.to_string().parse().unwrap(),
    );
    (status, resp_headers, Body::from_stream(stream)).into_response()
}

/// The remote control's bearer token check. An empty configured token locks
/// the whole API: a token is always generated when LAN sharing is switched
/// on, so empty here means something went wrong, and failing closed is right.
fn authorized(state: &ServerState, headers: &HeaderMap) -> bool {
    if state.token.is_empty() {
        return false;
    }
    let Some(v) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    else {
        return false;
    };
    let Some(given) = v.strip_prefix("Bearer ") else {
        return false;
    };
    given == state.token
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({ "error": "wrong or missing token" })),
    )
        .into_response()
}

async fn api_status(State(state): State<Arc<ServerState>>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    let items = state.control.remote_items();
    let mut downloading = 0;
    let mut waiting = 0;
    let mut completed = 0;
    let mut failed = 0;
    for i in &items {
        match i.status.as_str() {
            "connecting" | "downloading" | "retrying" | "processing" => downloading += 1,
            "idle" | "queued" | "paused" => waiting += 1,
            "completed" => completed += 1,
            "failed" => failed += 1,
            _ => {}
        }
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "total": items.len(),
            "downloading": downloading,
            "waiting": waiting,
            "completed": completed,
            "failed": failed,
        })),
    )
        .into_response()
}

async fn api_items(State(state): State<Arc<ServerState>>, headers: HeaderMap) -> Response {
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    (StatusCode::OK, Json(state.control.remote_items())).into_response()
}

async fn api_pause(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    if state.control.remote_pause(&id) {
        (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "unknown download" })),
        )
            .into_response()
    }
}

async fn api_resume(
    State(state): State<Arc<ServerState>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    if state.control.remote_resume(&id) {
        (StatusCode::OK, Json(serde_json::json!({ "ok": true }))).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "unknown download" })),
        )
            .into_response()
    }
}

#[derive(Deserialize)]
struct AddRequest {
    url: String,
}

async fn api_add(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    Json(body): Json<AddRequest>,
) -> Response {
    if !authorized(&state, &headers) {
        return unauthorized();
    }
    match state.control.remote_add(body.url.trim()) {
        Some(id) => (
            StatusCode::CREATED,
            Json(serde_json::json!({ "id": id })),
        )
            .into_response(),
        None => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "not a downloadable address" })),
        )
            .into_response(),
    }
}

/// The phone page: one self-contained page (no build step, no external
/// assets) that lists downloads and lets the phone pause, resume and add
/// them. The token is asked for once and kept in the phone's local storage.
const REMOTE_PAGE: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>Prism remote</title>
<style>
body { font-family: system-ui, sans-serif; margin: 0 auto; max-width: 640px; padding: 16px; background: #101418; color: #e8ecef; }
h1 { font-size: 20px; margin: 0 0 4px; }
.sub { color: #9aa5ad; margin: 0 0 16px; font-size: 14px; }
form { display: flex; gap: 8px; margin-bottom: 16px; }
input { flex: 1; padding: 10px; border-radius: 8px; border: 1px solid #333; background: #1a2027; color: inherit; font-size: 15px; }
button { padding: 10px 14px; border-radius: 8px; border: 0; background: #f0a92e; color: #181203; font-weight: 600; font-size: 15px; }
button.ghost { background: #232a31; color: #e8ecef; padding: 6px 10px; font-size: 13px; }
.row { border-top: 1px solid #232a31; padding: 12px 0; }
.row .nm { font-weight: 600; word-break: break-all; }
.row .meta { color: #9aa5ad; font-size: 13px; margin: 4px 0 8px; }
.err { color: #ff8a8a; }
.muted { color: #9aa5ad; }
</style></head>
<body>
<h1>Prism remote</h1>
<p class="sub" id="status">Loading…</p>
<form id="addform"><input id="url" type="url" placeholder="Paste a download link" autocomplete="off"><button type="submit">Add</button></form>
<div id="list"></div>
<p><button class="ghost" id="forget">Forget token</button></p>
<script>
var token = localStorage.getItem('prism-remote-token') || '';
function headers() { return { 'Authorization': 'Bearer ' + token, 'Content-Type': 'application/json' }; }
function askToken() {
  var t = prompt('Remote token (shown in Prism Settings, under LAN sharing):');
  if (t) { token = t.trim(); localStorage.setItem('prism-remote-token', token); return true; }
  return false;
}
function pct(it) {
  if (!it.size) return '';
  return ' · ' + Math.floor(100 * it.downloaded / it.size) + '%';
}
function render(items) {
  var list = document.getElementById('list');
  list.innerHTML = '';
  if (!items.length) { list.innerHTML = '<p class="muted">No downloads yet.</p>'; return; }
  items.forEach(function (it) {
    var row = document.createElement('div');
    row.className = 'row';
    var nm = document.createElement('div');
    nm.className = 'nm';
    nm.textContent = it.name;
    var meta = document.createElement('div');
    meta.className = 'meta';
    meta.textContent = it.status + pct(it);
    var btn = document.createElement('button');
    btn.className = 'ghost';
    var pausable = it.status === 'downloading' || it.status === 'connecting' || it.status === 'queued' || it.status === 'retrying';
    btn.textContent = pausable ? 'Pause' : 'Resume';
    btn.onclick = function () { act(it.id, pausable ? 'pause' : 'resume'); };
    row.appendChild(nm);
    row.appendChild(meta);
    row.appendChild(btn);
    list.appendChild(row);
  });
}
function load() {
  fetch('/api/items', { headers: headers() }).then(function (r) {
    if (r.status === 401) { if (askToken()) load(); else document.getElementById('status').textContent = 'Wrong token.'; return null; }
    return r.json();
  }).then(function (items) {
    if (!items) return;
    document.getElementById('status').textContent = items.length + ' download(s)';
    render(items);
  }).catch(function () { document.getElementById('status').textContent = 'Could not reach Prism.'; });
}
function act(id, what) {
  fetch('/api/items/' + encodeURIComponent(id) + '/' + what, { method: 'POST', headers: headers() })
    .then(function () { load(); });
}
document.getElementById('addform').addEventListener('submit', function (e) {
  e.preventDefault();
  var url = document.getElementById('url').value.trim();
  if (!url) return;
  fetch('/api/items', { method: 'POST', headers: headers(), body: JSON.stringify({ url: url }) })
    .then(function (r) {
      if (r.status === 401) { if (askToken()) load(); return; }
      if (!r.ok) { document.getElementById('status').textContent = 'That address could not be added.'; return; }
      document.getElementById('url').value = '';
      load();
    });
});
document.getElementById('forget').addEventListener('click', function () {
  localStorage.removeItem('prism-remote-token');
  token = '';
  load();
});
load();
setInterval(load, 5000);
</script>
</body></html>"#;

async fn remote_page() -> Response {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        REMOTE_PAGE,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct Static(Vec<SharedFile>);
    impl LanSource for Static {
        fn shared_files(&self) -> Vec<SharedFile> {
            self.0.clone()
        }
    }

    struct FakeControl {
        items: std::sync::Mutex<Vec<RemoteItem>>,
        paused: std::sync::Mutex<Vec<String>>,
        resumed: std::sync::Mutex<Vec<String>>,
        added: std::sync::Mutex<Vec<String>>,
    }

    impl FakeControl {
        fn with(items: Vec<RemoteItem>) -> Self {
            Self {
                items: std::sync::Mutex::new(items),
                paused: std::sync::Mutex::new(Vec::new()),
                resumed: std::sync::Mutex::new(Vec::new()),
                added: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl RemoteControl for FakeControl {
        fn remote_items(&self) -> Vec<RemoteItem> {
            self.items.lock().unwrap().clone()
        }

        fn remote_pause(&self, id: &str) -> bool {
            let known = self.items.lock().unwrap().iter().any(|i| i.id == id);
            if known {
                self.paused.lock().unwrap().push(id.to_string());
            }
            known
        }

        fn remote_resume(&self, id: &str) -> bool {
            let known = self.items.lock().unwrap().iter().any(|i| i.id == id);
            if known {
                self.resumed.lock().unwrap().push(id.to_string());
            }
            known
        }

        fn remote_add(&self, url: &str) -> Option<String> {
            if !url.contains("://") {
                return None;
            }
            self.added.lock().unwrap().push(url.to_string());
            let id = "added-1".to_string();
            self.items.lock().unwrap().push(RemoteItem {
                id: id.clone(),
                name: url.to_string(),
                status: "idle".to_string(),
                size: None,
                downloaded: 0,
            });
            Some(id)
        }
    }

    const TOKEN: &str = "test-token";

    async fn serve(files: Vec<SharedFile>) -> (LanServer, reqwest::Client, String) {
        serve_with(files, Arc::new(FakeControl::with(vec![]))).await
    }

    async fn serve_with(
        files: Vec<SharedFile>,
        control: Arc<FakeControl>,
    ) -> (LanServer, reqwest::Client, String) {
        let server = LanServer::start(
            Arc::new(Static(files)),
            control,
            TOKEN.to_string(),
            SocketAddr::from(([127, 0, 0, 1], 0)),
        )
        .await
        .unwrap();
        let base = format!("http://127.0.0.1:{}", server.addr.port());
        (server, reqwest::Client::new(), base)
    }

    fn temp_file(name: &str, bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        drop(f);
        (dir, path)
    }

    #[tokio::test]
    async fn index_lists_only_shared_files_and_file_downloads_exact() {
        let (_d1, p1) = temp_file("shared.bin", b"hello lan world");
        let (_d2, p2) = temp_file("private.bin", b"secret bytes");
        let files = vec![SharedFile {
            id: "abc".into(),
            name: "shared.bin".into(),
            path: p1.clone(),
            size: 15,
        }];
        let (_srv, client, base) = serve(files).await;
        let index = client.get(format!("{base}/")).send().await.unwrap();
        assert_eq!(index.status(), 200);
        let text = index.text().await.unwrap();
        assert!(text.contains("shared.bin"));
        assert!(!text.contains("private.bin"));
        // The unshared file is unreachable even by guessing its path.
        let missing = client.get(format!("{base}/files/private")).send().await.unwrap();
        assert_eq!(missing.status(), 404);
        let got = client.get(format!("{base}/files/abc")).send().await.unwrap();
        assert_eq!(got.status(), 200);
        assert_eq!(got.bytes().await.unwrap().as_ref(), b"hello lan world");
        // A .part file is never served: the source decides what is shared, and
        // the test source above never lists it, so it 404s like any other id.
        let _ = p2;
    }

    #[tokio::test]
    async fn range_requests_serve_the_requested_slice() {
        let (_d, p) = temp_file("range.bin", b"0123456789abcdef");
        let files = vec![SharedFile {
            id: "r".into(),
            name: "range.bin".into(),
            path: p,
            size: 16,
        }];
        let (_srv, client, base) = serve(files).await;
        let resp = client
            .get(format!("{base}/files/r"))
            .header("Range", "bytes=4-9")
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 206);
        assert_eq!(resp.headers()["Content-Range"], "bytes 4-9/16");
        assert_eq!(resp.bytes().await.unwrap().as_ref(), b"456789");
        let bad = client
            .get(format!("{base}/files/r"))
            .header("Range", "bytes=99-100")
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 416);
    }

    #[tokio::test]
    async fn unshared_or_missing_files_404() {
        let (_d, p) = temp_file("gone.bin", b"gone");
        let files = vec![SharedFile {
            id: "g".into(),
            name: "gone.bin".into(),
            path: p.clone(),
            size: 4,
        }];
        let (_srv, client, base) = serve(files).await;
        std::fs::remove_file(&p).unwrap();
        let resp = client.get(format!("{base}/files/g")).send().await.unwrap();
        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn remote_api_needs_the_token() {
        let control = Arc::new(FakeControl::with(vec![]));
        let (_srv, client, base) = serve_with(vec![], control).await;
        // No token, and a wrong token, are both locked out.
        let no_token = client
            .get(format!("{base}/api/status"))
            .send()
            .await
            .unwrap();
        assert_eq!(no_token.status(), 401);
        let wrong = client
            .get(format!("{base}/api/status"))
            .header("authorization", "Bearer nope")
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), 401);
        let ok = client
            .get(format!("{base}/api/status"))
            .header("authorization", format!("Bearer {TOKEN}"))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), 200);
    }

    #[tokio::test]
    async fn remote_api_lists_pauses_resumes_and_adds() {
        let control = Arc::new(FakeControl::with(vec![RemoteItem {
            id: "one".into(),
            name: "file.bin".into(),
            status: "downloading".into(),
            size: Some(100),
            downloaded: 40,
        }]));
        let (_srv, client, base) = serve_with(vec![], control.clone()).await;
        let auth = ("authorization", format!("Bearer {TOKEN}"));
        let items: Vec<RemoteItem> = client
            .get(format!("{base}/api/items"))
            .header(auth.0, auth.1.clone())
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].name, "file.bin");
        // Pausing an unknown id is a 404, not a silent success.
        let missing = client
            .post(format!("{base}/api/items/nope/pause"))
            .header(auth.0, auth.1.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), 404);
        let paused = client
            .post(format!("{base}/api/items/one/pause"))
            .header(auth.0, auth.1.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(paused.status(), 200);
        assert_eq!(control.paused.lock().unwrap().as_slice(), ["one"]);
        let resumed = client
            .post(format!("{base}/api/items/one/resume"))
            .header(auth.0, auth.1.clone())
            .send()
            .await
            .unwrap();
        assert_eq!(resumed.status(), 200);
        assert_eq!(control.resumed.lock().unwrap().as_slice(), ["one"]);
        // Adding a bad address is a 400; a good one is a 201 with an id.
        let bad = client
            .post(format!("{base}/api/items"))
            .header(auth.0, auth.1.clone())
            .json(&serde_json::json!({ "url": "not a link" }))
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 400);
        let added: serde_json::Value = client
            .post(format!("{base}/api/items"))
            .header(auth.0, auth.1.clone())
            .json(&serde_json::json!({ "url": "https://example.com/new.bin" }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(added["id"], "added-1");
        assert_eq!(
            control.added.lock().unwrap().as_slice(),
            ["https://example.com/new.bin"]
        );
    }

    #[tokio::test]
    async fn remote_page_serves_the_phone_page() {
        let (_srv, client, base) = serve(vec![]).await;
        // The page itself needs no token (the token is asked for on the page);
        // the API behind it is what is locked.
        let resp = client.get(format!("{base}/remote")).send().await.unwrap();
        assert_eq!(resp.status(), 200);
        let text = resp.text().await.unwrap();
        assert!(text.contains("Prism remote"));
        assert!(text.contains("/api/items"));
    }
}
