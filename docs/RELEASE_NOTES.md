# Prism 1.6.0 — Fork release: headless-browser downloads, download agent, all-networks-at-once.

A fast, open-source download manager for Windows, Linux and macOS, with browser integration
and video downloads.

## What's new in 1.6.0

* **Headless-browser downloads.** When a site only lets a web browser download a file — Cloudflare checks, login walls, fingerprint checks — Prism now retries the download in a real headless Chrome, with the same cookies and address, and takes the file it saves. On by default in Settings → Downloads → Browser downloads; a failed download can also be retried from its right-click menu ("Retry in browser"). A throwaway profile is used every time, so your real browser profile is never touched. Direct file links only: a page that needs a human click still needs the human.
* **Download agent.** Ask Prism in plain words — "the latest episode of Severance in 1080p" — and it searches, picks the best match, chooses the quality you asked for (or the best one), and starts the download. A pasted video link skips the search; a pasted playlist link downloads its entries. Find it in the toolbar ("Ask Prism"). Local and rule-based: no AI model, no API key.
* **Use all networks at once.** Settings → Connection → "Use all networks at once" spreads a download's segments across every usable network — Wi-Fi, Ethernet and a hotspot together — instead of one network. An interface that cannot reach the server simply loses its segments to the working ones.
