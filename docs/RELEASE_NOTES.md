# Prism 1.3.2 — Fork release: videos the browser starts now auto-capture from the sniffer.

A fast, open-source download manager for Windows, Linux and macOS, with browser integration
and video downloads.

## What's new in 1.3.2

* **Video downloads auto-capture from the sniffer.** When the browser starts a video download that Prism can't capture itself — the link is bot-checked, uses a scheme Prism can't hand off, or isn't a file Prism's capture list covers — Prism now falls back to the page's single sniffed video link when it differs from the download's own link, and cancels the browser download only after accepting it. No more clicking the extension popup entry and cancelling the browser download by hand.
* **Careful by design.** The fallback only fires when exactly one sniffed video or stream qualifies — with more than one, Prism guesses nothing — and it never overrides a paused capture, a held bypass key, or an excluded site. If Prism declines the sniffed link too, the browser download stands exactly as before.
* **This fix lives in the browser extension.** It reaches you through the fork's extension in this release (prism-chromium.zip / prism-firefox.zip), loaded unpacked — the store extension is upstream's and is not updated by fork releases.
