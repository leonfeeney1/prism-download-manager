# Prism 1.3.1 — Fork release: bot-checked downloads hand back to the browser.

A fast, open-source download manager for Windows, Linux and macOS, with browser integration
and video downloads.

## What's new in 1.3.1

* **Bot-checked downloads hand back to the browser.** When a site lets only a web browser download a file (a Cloudflare-style bot check) and Prism only finds out after taking the download, it now opens the link in the browser instead of leaving a failed item behind — no more dead downloads, no more racing the browser's own downloader.
* **Upstream 1.2.4 changes, now in a fork release.** The November sync brought in upstream's Linux-only download sounds, the disk-space guard that refuses a merge when the drive is too full, and the 1.2.4 site updates. They reach a release for the first time here.
