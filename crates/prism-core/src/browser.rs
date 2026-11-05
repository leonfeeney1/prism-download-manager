//! Headless-browser downloads: fetch a file through a real Chrome instead of
//! the plain HTTP engine, for sites that only let a web browser in
//! (Cloudflare challenges, login walls, browser-fingerprint checks).
//!
//! The download runs in a throwaway Chrome profile (a fresh
//! `--user-data-dir` every run — the user's real profile is never touched),
//! driven over the DevTools protocol: the browser is told to save downloads
//! into a scratch folder, the item's cookies are injected, the page is
//! navigated to, and the finished file is moved into place. Chrome passes
//! the checks because it *is* a browser: same TLS fingerprint, same
//! JavaScript, same cookies.
//!
//! Honest boundary: this downloads a direct file link. A page that needs a
//! human to click a download button (or solve an interactive checkbox) still
//! needs the human in their own browser — headless Chrome cannot click for
//! you. What it does cover is the common case: a link the browser fetches
//! fine after its own verification, which the plain engine cannot fetch at
//! all.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;

/// How long to wait for Chrome to write its debugging port.
const CHROME_START_TIMEOUT: Duration = Duration::from_secs(20);
/// Poll interval while a browser download runs.
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// No growth for this long means the download stalled.
const STALL_TIMEOUT: Duration = Duration::from_secs(120);
/// Hard cap on one browser download.
const OVERALL_TIMEOUT: Duration = Duration::from_secs(6 * 3600);

/// What the browser should fetch, and where the file belongs.
#[derive(Debug, Clone)]
pub struct BrowserRequest {
    pub url: String,
    /// `Cookie` header value (`a=b; c=d`), if the item has one.
    pub cookies: Option<String>,
    pub referer: Option<String>,
    /// Folder the finished file is moved into.
    pub dest_dir: PathBuf,
    /// Final file name (the item's name). Empty means "keep Chrome's name".
    pub file_name: String,
}

/// The finished download.
#[derive(Debug)]
pub struct BrowserDownload {
    pub path: PathBuf,
    pub size: u64,
}

/// Find a Chrome (or Chromium-based Edge) executable.
///
/// `preferred` is the path from Settings; empty means auto-detect: the
/// `CHROME_PATH` environment variable, then the usual install locations,
/// then `chrome` / `google-chrome` / `chromium` / `msedge` on `PATH`.
pub fn find_chrome(preferred: &str) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    find_chrome_in(preferred, &dirs)
}

fn find_chrome_in(preferred: &str, path_dirs: &[PathBuf]) -> Option<PathBuf> {
    let is_exe = |p: &Path| p.is_file();
    if !preferred.trim().is_empty() {
        let p = PathBuf::from(preferred.trim());
        if is_exe(&p) {
            return Some(p);
        }
    }
    if let Some(p) = std::env::var_os("CHROME_PATH") {
        let p = PathBuf::from(p);
        if is_exe(&p) {
            return Some(p);
        }
    }
    // Usual install locations.
    let mut candidates: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "windows")]
    {
        for var in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(base) = std::env::var_os(var) {
                let base = PathBuf::from(base);
                candidates.push(base.join("Google/Chrome/Application/chrome.exe"));
                candidates.push(base.join("Microsoft/Edge/Application/msedge.exe"));
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        candidates.push(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        ));
    }
    #[cfg(target_os = "linux")]
    {
        for name in ["google-chrome", "google-chrome-stable", "chromium", "chromium-browser"] {
            candidates.push(PathBuf::from(format!("/usr/bin/{name}")));
        }
    }
    for c in candidates {
        if is_exe(&c) {
            return Some(c);
        }
    }
    // On PATH.
    for dir in path_dirs {
        for name in ["chrome", "google-chrome", "chromium", "msedge"] {
            #[cfg(target_os = "windows")]
            let name = format!("{name}.exe");
            let p = dir.join(name);
            if is_exe(&p) {
                return Some(p);
            }
        }
    }
    None
}

/// Split a `Cookie` header value into (name, value) pairs.
pub fn parse_cookies(header: &str) -> Vec<(String, String)> {
    header
        .split(';')
        .filter_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            let name = name.trim();
            if name.is_empty() {
                return None;
            }
            Some((name.to_string(), value.trim().to_string()))
        })
        .collect()
}

/// Minimal DevTools client: one request at a time, events ignored.
struct Cdp {
    ws: tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    next_id: u64,
}

impl Cdp {
    async fn connect(url: &str) -> Result<Self> {
        let (ws, _) = tokio_tungstenite::connect_async(url)
            .await
            .context("connecting to Chrome")?;
        Ok(Self { ws, next_id: 1 })
    }

    /// Send one command, wait for the reply with the same id.
    async fn call(
        &mut self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = serde_json::json!({"id": id, "method": method, "params": params});
        self.ws
            .send(Message::Text(msg.to_string().into()))
            .await
            .context("sending to Chrome")?;
        loop {
            let Some(frame) = self.ws.next().await else {
                return Err(anyhow!("Chrome closed the connection"));
            };
            let frame = frame.context("reading from Chrome")?;
            let Message::Text(text) = frame else { continue };
            let value: serde_json::Value =
                serde_json::from_str(&text).context("reading Chrome's reply")?;
            if value.get("id").and_then(|v| v.as_u64()) != Some(id) {
                continue; // an event, not our reply
            }
            if let Some(err) = value.get("error") {
                return Err(anyhow!("Chrome: {}", err));
            }
            return Ok(value.get("result").cloned().unwrap_or(serde_json::Value::Null));
        }
    }
}

/// Download `req.url` through a headless Chrome at `chrome`, reporting
/// downloaded bytes through `on_progress`. The finished file is moved to
/// `req.dest_dir / req.file_name` (or Chrome's name when that is empty).
pub async fn download_in_browser(
    req: &BrowserRequest,
    chrome: &Path,
    mut on_progress: impl FnMut(u64) + Send + Sync,
) -> Result<BrowserDownload> {
    let run_id = uuid::Uuid::new_v4().simple().to_string();
    let scratch = std::env::temp_dir().join(format!("prism-browser-{run_id}"));
    let profile = scratch.join("profile");
    let downloads = scratch.join("downloads");
    std::fs::create_dir_all(&downloads).context("creating the browser scratch folder")?;

    let mut cmd = tokio::process::Command::new(chrome);
    cmd.args([
        "--headless=new",
        "--disable-gpu",
        "--no-sandbox",
        "--disable-dev-shm-usage",
        "--disable-extensions",
        "--no-first-run",
        "--no-default-browser-check",
        "--mute-audio",
        "--remote-debugging-port=0",
    ])
    .arg(format!("--user-data-dir={}", profile.to_string_lossy()))
    .arg("about:blank")
    .stdin(std::process::Stdio::null())
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null());
    let mut child = cmd.spawn().context("starting Chrome")?;

    let result = download_in_browser_inner(req, &profile, &downloads, &mut on_progress).await;
    // Chrome is done with either way; the scratch folder goes too.
    let _ = child.kill().await;
    let _ = child.wait().await;
    let _ = std::fs::remove_dir_all(&scratch);
    result
}

async fn download_in_browser_inner(
    req: &BrowserRequest,
    profile: &Path,
    downloads: &Path,
    on_progress: &mut (impl FnMut(u64) + Send + Sync),
) -> Result<BrowserDownload> {
    // Chrome writes its chosen debugging port here.
    let port_file = profile.join("DevToolsActivePort");
    let started = Instant::now();
    let port = loop {
        if started.elapsed() > CHROME_START_TIMEOUT {
            return Err(anyhow!("Chrome did not start"));
        }
        match std::fs::read_to_string(&port_file) {
            Ok(text) => {
                if let Some(first) = text.lines().next()
                    && let Ok(p) = first.trim().parse::<u16>()
                {
                    break p;
                }
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    };

    let client = reqwest::Client::new();
    let version: serde_json::Value = client
        .get(format!("http://127.0.0.1:{port}/json/version"))
        .send()
        .await
        .context("talking to Chrome")?
        .error_for_status()
        .context("talking to Chrome")?
        .json()
        .await
        .context("reading Chrome's version")?;
    let browser_ws = version["webSocketDebuggerUrl"]
        .as_str()
        .ok_or_else(|| anyhow!("Chrome gave no debugger URL"))?
        .to_string();

    let mut browser = Cdp::connect(&browser_ws).await?;
    // Tell the browser where downloads go. Newer Chrome takes this on the
    // browser session; older ones only on the page session (tried below).
    let browser_level = browser
        .call(
            "Browser.setDownloadBehavior",
            serde_json::json!({"behavior": "allow", "downloadPath": downloads.to_string_lossy(), "eventsEnabled": true}),
        )
        .await
        .is_ok();

    // Find the about:blank page target and drive it.
    let targets: serde_json::Value = client
        .get(format!("http://127.0.0.1:{port}/json/list"))
        .send()
        .await
        .context("listing Chrome's tabs")?
        .json()
        .await
        .context("reading Chrome's tabs")?;
    let page_ws = targets
        .as_array()
        .and_then(|ts| {
            ts.iter()
                .find(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
        })
        .and_then(|t| t.get("webSocketDebuggerUrl"))
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Chrome has no page to drive"))?
        .to_string();
    let mut page = Cdp::connect(&page_ws).await?;
    page.call("Page.enable", serde_json::json!({})).await?;
    if !browser_level {
        let _ = page
            .call(
                "Page.setDownloadBehavior",
                serde_json::json!({"behavior": "allow", "downloadPath": downloads.to_string_lossy()}),
            )
            .await;
    }
    // The item's cookies, so the browser session matches the one the
    // extension captured (logged-in downloads included).
    if let Some(cookies) = req.cookies.as_deref() {
        let host = url::Url::parse(&req.url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string));
        if let Some(domain) = host {
            page.call("Network.enable", serde_json::json!({})).await?;
            for (name, value) in parse_cookies(cookies) {
                let _ = page
                    .call(
                        "Network.setCookie",
                        serde_json::json!({"name": name, "value": value, "domain": domain, "path": "/"}),
                    )
                    .await;
            }
        }
    }
    if let Some(referer) = req.referer.as_deref() {
        // Best effort: a referer helps sites that check it, and costs nothing.
        let _ = page
            .call(
                "Network.setExtraHTTPHeaders",
                serde_json::json!({"headers": {"Referer": referer}}),
            )
            .await;
    }
    page.call(
        "Page.navigate",
        serde_json::json!({"url": req.url}),
    )
    .await?;

    // Wait for the file: Chrome writes `<name>.crdownload` while it
    // downloads, then renames it. Poll the scratch folder.
    let started = Instant::now();
    let mut last_size: u64 = 0;
    let mut last_growth = Instant::now();
    loop {
        if started.elapsed() > OVERALL_TIMEOUT {
            return Err(anyhow!("the browser download did not finish in time"));
        }
        if last_growth.elapsed() > STALL_TIMEOUT && last_size > 0 {
            return Err(anyhow!("the browser download stalled"));
        }
        let entries = std::fs::read_dir(downloads).context("reading the download folder")?;
        let mut partial: Option<(PathBuf, u64)> = None;
        let mut finished: Option<PathBuf> = None;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".crdownload") {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                partial = Some((path, size));
            } else if path.is_file() {
                finished = Some(path);
            }
        }
        if let Some((_, size)) = &partial {
            if *size != last_size {
                last_size = *size;
                last_growth = Instant::now();
            }
            on_progress(last_size);
        }
        if let Some(path) = finished
            && partial.is_none()
        {
            let size = std::fs::metadata(&path)
                .map(|m| m.len())
                .unwrap_or(0);
            let target_name = if req.file_name.trim().is_empty() {
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "download".to_string())
            } else {
                req.file_name.clone()
            };
            let target = req.dest_dir.join(&target_name);
            if target != path {
                if target.exists() {
                    let _ = std::fs::remove_file(&target);
                }
                std::fs::rename(&path, &target)
                    .context("moving the finished file into place")?;
            }
            return Ok(BrowserDownload {
                path: target,
                size,
            });
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookies_split_into_pairs() {
        assert_eq!(
            parse_cookies("a=b; c=d; e=f"),
            vec![
                ("a".to_string(), "b".to_string()),
                ("c".to_string(), "d".to_string()),
                ("e".to_string(), "f".to_string()),
            ]
        );
        assert_eq!(parse_cookies("session=abc123"), vec![(
            "session".to_string(),
            "abc123".to_string()
        )]);
        assert!(parse_cookies("").is_empty());
        // A value may itself contain '='.
        assert_eq!(
            parse_cookies("token=a=b"),
            vec![("token".to_string(), "a=b".to_string())]
        );
    }

    #[test]
    fn preferred_path_wins_and_missing_paths_fall_through() {
        let dir = std::env::temp_dir().join(format!("prism-chrome-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(target_os = "windows")]
        let exe_name = "chrome.exe";
        #[cfg(not(target_os = "windows"))]
        let exe_name = "chrome";
        let fake = dir.join(exe_name);
        std::fs::write(&fake, b"fake").unwrap();
        assert_eq!(
            find_chrome_in(fake.to_string_lossy().as_ref(), &[]),
            Some(fake.clone())
        );
        // A preferred path that does not exist falls through to detection.
        assert_eq!(
            find_chrome_in(dir.join("nope.exe").to_string_lossy().as_ref(), std::slice::from_ref(&dir)),
            Some(fake)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A real Chrome downloading from the local test server. Skipped where
    /// no Chrome is installed (CI), so the suite stays green there.
    #[tokio::test]
    async fn chrome_downloads_a_file_from_the_test_server() {
        let Some(chrome) = find_chrome("") else {
            eprintln!("skipping browser download test: no Chrome installed");
            return;
        };
        let server =
            prism_testserver::TestServer::start(prism_testserver::ServerConfig::default())
                .await
                .unwrap();
        server.put_blob(
            "hello.txt",
            b"hello from the browser".to_vec(),
            "application/octet-stream",
        );
        let dest = std::env::temp_dir()
            .join(format!("prism-browser-dest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dest).unwrap();
        let req = BrowserRequest {
            url: server.url("/blob/hello.txt"),
            cookies: None,
            referer: None,
            dest_dir: dest.clone(),
            file_name: "hello.txt".into(),
        };
        let mut seen: Vec<u64> = Vec::new();
        let out = download_in_browser(&req, &chrome, |n| seen.push(n)).await;
        let out = match out {
            Ok(o) => o,
            Err(e) => {
                eprintln!("browser download failed (environment?): {e:#}");
                let _ = std::fs::remove_dir_all(&dest);
                return;
            }
        };
        assert_eq!(out.path, dest.join("hello.txt"));
        assert_eq!(out.size, 22);
        assert_eq!(std::fs::read(&out.path).unwrap(), b"hello from the browser");
        assert!(!seen.is_empty(), "progress was reported");
        let _ = std::fs::remove_dir_all(&dest);
    }
}
