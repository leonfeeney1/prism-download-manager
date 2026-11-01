//! LAN sharing: serve completed downloads to other devices on the local network.
//!
//! Opt-in only (off by default). When on, a small axum server binds the machine's
//! LAN address — never a public one — and serves a plain listing plus the files
//! themselves. The server never serves by path: every request resolves an item id
//! through a [`LanSource`], which only ever returns completed, user-shared files,
//! so in-progress `.part` files, settings and the database can never leak out.
//! Stopping is a drop: taking the server out of the manager shuts it down.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{self, HeaderMap};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
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
}

pub struct LanServer {
    pub addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl LanServer {
    /// Bind `addr` and start serving. Returns once the socket is bound.
    pub async fn start(
        source: Arc<dyn LanSource>,
        addr: SocketAddr,
    ) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind(addr).await?;
        let bound = listener.local_addr()?;
        let app = Router::new()
            .route("/", get(index))
            .route("/files/{id}", get(file))
            .with_state(source);
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

async fn index(State(source): State<Arc<dyn LanSource>>) -> Response {
    let files = source.shared_files();
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
        <p>Files shared from Prism on this local network.</p></body></html>"
    );
    (StatusCode::OK, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response()
}

/// Parse a single `Range: bytes=...` header. Returns (start, end_inclusive).
fn parse_range(value: &str, len: u64) -> Option<(u64, u64)> {
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
    State(source): State<Arc<dyn LanSource>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let Some(shared) = source.shared_files().into_iter().find(|f| f.id == id) else {
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

    async fn serve(files: Vec<SharedFile>) -> (LanServer, reqwest::Client, String) {
        let server = LanServer::start(
            Arc::new(Static(files)),
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
}
