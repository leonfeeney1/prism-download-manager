//! Torrent streaming: watch a torrent video while it still downloads.
//!
//! A tiny server on loopback only (never the LAN): a player's requests open
//! librqbit's `FileStream` for the file, which fetches pieces in playback
//! order, so the video plays instead of waiting for the whole file. The
//! server binds an ephemeral port and only lives while a stream address was
//! asked for.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Result;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::header::{self, HeaderMap};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures_util::StreamExt;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt};
use tokio_util::io::ReaderStream;

/// A readable + seekable stream (one trait object: Rust only allows a single
/// non-auto trait in a `dyn` type, so the read/seek pair lives behind this).
pub trait StreamRead: AsyncRead + AsyncSeek + Unpin + Send {}
impl<T: AsyncRead + AsyncSeek + Unpin + Send> StreamRead for T {}

/// One file a stream can be opened for.
pub struct StreamFile {
    pub name: String,
    pub len: u64,
    pub content_type: &'static str,
    pub stream: Box<dyn StreamRead>,
}

/// Something that can open a torrent file for streaming. Implemented by the
/// manager; the server only ever talks to the app through this trait.
pub trait StreamSource: Send + Sync + 'static {
    fn open_stream(
        &self,
        item_id: &str,
        file_index: usize,
    ) -> Pin<Box<dyn Future<Output = Result<StreamFile>> + Send + '_>>;
}

/// Whether a torrent file can be streamed to a player (video or audio).
pub fn is_streamable(name: &str) -> bool {
    matches!(
        name.rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "mp4" | "m4v" | "mkv" | "webm" | "mov" | "avi" | "mp3" | "flac" | "wav"
    )
}

/// Content type for a file the browser or player is about to play.
pub fn content_type_for(name: &str) -> &'static str {
    let ext = name
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

pub struct StreamServer {
    pub addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
}

impl StreamServer {
    /// Bind loopback on an ephemeral port and start serving.
    pub async fn start(source: Arc<dyn StreamSource>) -> std::io::Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let bound = listener.local_addr()?;
        let app = Router::new()
            .route("/stream/{item_id}/{file_index}", get(stream))
            .with_state(source);
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let server = axum::serve(listener, app).with_graceful_shutdown(async {
                let _ = rx.await;
            });
            if let Err(e) = server.await {
                tracing::error!("stream server stopped: {e}");
            }
        });
        tracing::info!(%bound, "stream server listening");
        Ok(Self {
            addr: bound,
            shutdown: Some(tx),
        })
    }
}

impl Drop for StreamServer {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
    }
}

async fn stream(
    State(source): State<Arc<dyn StreamSource>>,
    Path((item_id, file_index)): Path<(String, usize)>,
    headers: HeaderMap,
) -> Response {
    let file = match source.open_stream(&item_id, file_index).await {
        Ok(f) => f,
        Err(_) => return (StatusCode::NOT_FOUND, "not found").into_response(),
    };
    let len = file.len;
    let mut stream = file.stream;
    let safe_name: String = file
        .name
        .chars()
        .map(|c| {
            if c == '"' || c == '\\' || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut resp_headers = HeaderMap::new();
    resp_headers.insert(header::ACCEPT_RANGES, "bytes".parse().unwrap());
    resp_headers.insert(
        header::CONTENT_TYPE,
        file.content_type.parse().unwrap(),
    );
    resp_headers.insert(
        header::CONTENT_DISPOSITION,
        format!("inline; filename=\"{safe_name}\"")
            .parse()
            .unwrap_or_else(|_| "inline".parse().unwrap()),
    );
    let (start, end, status) = match headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
    {
        Some(range) => match crate::lan::parse_range(range, len) {
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
                return (
                    StatusCode::RANGE_NOT_SATISFIABLE,
                    resp_headers,
                    "range not satisfiable",
                )
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
    if stream
        .seek(std::io::SeekFrom::Start(start))
        .await
        .is_err()
    {
        return (StatusCode::INTERNAL_SERVER_ERROR, "seek failed").into_response();
    }
    let body = ReaderStream::new(stream.take(count)).map(|r| r.map_err(std::io::Error::other));
    resp_headers.insert(
        header::CONTENT_LENGTH,
        count.to_string().parse().unwrap(),
    );
    (status, resp_headers, Body::from_stream(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    struct Static(Vec<u8>);

    impl StreamSource for Static {
        fn open_stream(
            &self,
            item_id: &str,
            _file_index: usize,
        ) -> Pin<Box<dyn Future<Output = Result<StreamFile>> + Send + '_>> {
            let data = self.0.clone();
            let known = item_id == "known";
            Box::pin(async move {
                if !known {
                    return Err(anyhow::anyhow!("unknown download"));
                }
                let len = data.len() as u64;
                Ok(StreamFile {
                    name: "clip.mp4".to_string(),
                    len,
                    content_type: "video/mp4",
                    stream: Box::new(Cursor::new(data)),
                })
            })
        }
    }

    async fn serve(data: Vec<u8>) -> (StreamServer, reqwest::Client, String) {
        let server = StreamServer::start(Arc::new(Static(data)))
            .await
            .unwrap();
        let base = format!("http://127.0.0.1:{}", server.addr.port());
        (server, reqwest::Client::new(), base)
    }

    #[tokio::test]
    async fn stream_serves_the_whole_file_and_ranges() {
        let (_srv, client, base) = serve(b"0123456789abcdef".to_vec()).await;
        let whole = client
            .get(format!("{base}/stream/known/0"))
            .send()
            .await
            .unwrap();
        assert_eq!(whole.status(), 200);
        assert_eq!(whole.headers()["Content-Type"], "video/mp4");
        assert_eq!(whole.bytes().await.unwrap().as_ref(), b"0123456789abcdef");
        let range = client
            .get(format!("{base}/stream/known/0"))
            .header("Range", "bytes=4-9")
            .send()
            .await
            .unwrap();
        assert_eq!(range.status(), 206);
        assert_eq!(range.headers()["Content-Range"], "bytes 4-9/16");
        assert_eq!(range.bytes().await.unwrap().as_ref(), b"456789");
        let bad = client
            .get(format!("{base}/stream/known/0"))
            .header("Range", "bytes=99-100")
            .send()
            .await
            .unwrap();
        assert_eq!(bad.status(), 416);
    }

    #[tokio::test]
    async fn unknown_downloads_404() {
        let (_srv, client, base) = serve(b"0123456789abcdef".to_vec()).await;
        let resp = client
            .get(format!("{base}/stream/nope/0"))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 404);
    }
}
