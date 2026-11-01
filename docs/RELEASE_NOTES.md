# Prism 1.3.0 — Fork release: new features, Hugging Face removed.

A fast, open-source download manager for Windows, Linux and macOS, with browser integration
and video downloads.

## What's new in 1.3.0

* **Hugging Face removed.** The model downloader, its window, settings, and commands are gone from this fork.
* **Multi-source downloads.** One download now pulls segments from all of its sources at once, with failing mirrors benched and their segments reassigned instead of killing the download. Extra sources can be added while a download runs.
* **Auto-extract and organize.** Finished archives (zip, tar, tar.gz, tar.xz) extract automatically into a folder named after the archive, and loose files move into their category folder. Failures never touch the original archive.
* **LAN share.** Completed downloads can be shared with other devices on the local network (off by default, configurable port). Only completed, explicitly-shared items are ever served.
* **Natural-language grabber.** Describe what to grab in plain English ("download all PDFs from example.com") and review an editable preview before the crawl starts. Parsed locally — no AI service, no key.
* **Video subtitles + translate.** Requested subtitle languages are satisfied by manual subtitles or the platform's own translated captions where offered, embedded or as sidecar files; missing languages are shown as a warning instead of finishing silently.
