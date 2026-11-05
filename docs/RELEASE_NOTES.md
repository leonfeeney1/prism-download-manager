# Prism 1.5.0 — Fork release: dead-link checker, phone remote, torrent streaming.

A fast, open-source download manager for Windows, Linux and macOS, with browser integration
and video downloads.

## What's new in 1.5.0

* **Dead-link checker.** A "Check links" button in the toolbar scans the whole list with the existing probe and flags links that 404, 403 or are gone, so a dead queue never wastes a night. Dead links get a "Dead link" chip on the item, and a single link can be checked from its right-click menu. Inconclusive results are never marked dead.
* **Phone remote control.** The LAN server now serves a small remote API: open the address shown in Settings → LAN sharing on your phone, type the remote token once, and you can pause, resume and add downloads from your phone. The token is generated once and shown in Settings; without it the API answers 401.
* **Torrent sequential mode.** Open a torrent's details and press Stream on a video or audio file to watch it in your browser while it still downloads — pieces are fetched in playback order instead of waiting for the whole file.
