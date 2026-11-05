// Context menu for one or more downloads (right-click, "…" button).

import { actions } from "../lib/actions";
import { api } from "../lib/api";
import { t } from "../lib/i18n";
import { GIF_MAX_SECONDS, PRESETS, presetLabel, quickTargets, TARGET_NAMES } from "../lib/media";
import { canCheckLink, canPause, canResume, canRetryBrowser, displayName } from "../lib/present";
import { useApp } from "../lib/store";
import { mediaPath, openTools, quickConvert } from "../lib/tools";
import { type MenuEntry, attempt, toast, useUi } from "../lib/ui";

export function itemMenu(ids: string[]): MenuEntry[] {
  const st = useApp.getState();
  const list = ids.map((id) => st.items[id]).filter(Boolean);
  if (!list.length) return [];
  const one = list.length === 1 ? list[0] : null;
  const ui = useUi.getState();
  const trash = list.every((i) => i.trashed);

  if (trash) {
    return [
      { label: t("Restore"), icon: "retry", onSelect: () => actions.restore(ids) },
      { separator: true },
      { label: t("Delete permanently…"), icon: "delete", danger: true, kbd: "Del", onSelect: () => ui.openDialog({ kind: "delete", ids, trash: true }) },
    ];
  }

  const done = one?.status === "completed";
  const inQueue = list.some((i) => i.queueId);
  const queueEntries: MenuEntry[] = [
    ...st.queues.map((q) => ({
      label: displayName(q.name),
      checked: list.every((i) => i.queueId === q.id),
      onSelect: () => actions.moveToQueue(ids, q.id),
    })),
    { separator: true },
    { label: t("Remove from queue"), disabled: !inQueue, onSelect: () => actions.moveToQueue(ids, null) },
  ];

  // A finished picture, song or video: convert it, or make it smaller for a size limit.
  const media = mediaPath(one);
  const tools: MenuEntry[] =
    media && one
      ? [
          { separator: true },
          {
            label: t("Convert to"),
            icon: "convert",
            submenu: [
              ...quickTargets(one.fileName)
                // A GIF only for short clips (when the length is known).
                .filter((x) => x !== "gif" || (one.media?.duration ?? 0) <= GIF_MAX_SECONDS)
                .map((x) => ({ label: TARGET_NAMES[x], onSelect: () => void quickConvert(one, x) })),
              { separator: true },
              { label: t("More…"), onSelect: () => openTools({ tab: "convert", paths: [media] }) },
            ],
          },
          {
            label: t("Make smaller"),
            icon: "shrink",
            submenu: [
              ...PRESETS.map((p) => ({ label: presetLabel(p.id), onSelect: () => openTools({ tab: "shrink", paths: [media], preset: p.id }) })),
              { separator: true },
              { label: t("Custom size…"), onSelect: () => openTools({ tab: "shrink", paths: [media], preset: "custom" }) },
            ],
          },
        ]
      : [];

  // A finished archive that was not extracted yet (same formats the backend extracts).
  const isArchive = one && /\.(zip|tar|tgz|txz|tar\.gz|tar\.xz)$/i.test(one.fileName);
  const extractEntry: MenuEntry[] =
    one && done && isArchive && !one.extractedDir
      ? [{ label: t("Extract"), icon: "folder-open", onSelect: () => actions.extract(one.id) }]
      : [];

  return [
    { label: t("Open"), icon: "open-file", kbd: "Enter", disabled: !done, onSelect: () => one && actions.open(one.id) },
    { label: t("Show in folder"), icon: "folder-open", disabled: !one, onSelect: () => one && actions.reveal(one.id) },
    ...extractEntry,
    ...tools,
    { separator: true },
    { label: t("Resume"), icon: "resume", disabled: !list.some((i) => canResume(i.status) || i.status === "queued"), onSelect: () => actions.resume(ids) },
    { label: t("Pause"), icon: "pause", kbd: "Space", disabled: !list.some((i) => canPause(i.status)), onSelect: () => actions.pause(ids) },
    { label: t("Redownload"), icon: "retry", disabled: !one, onSelect: () => one && actions.redownload(one.id) },
    { label: t("Retry in browser"), icon: "browser", disabled: !one || !canRetryBrowser(one), onSelect: () => one && void attempt(api.retryBrowser(one.id), t("Could not retry in browser")) },
    { label: t("Check link"), icon: "link", disabled: !one || !canCheckLink(one), onSelect: () => one && void checkLink(one.id) },
    { label: t("Refresh download address…"), icon: "refresh-url", disabled: !one || done, onSelect: () => one && ui.openDialog({ kind: "refreshUrl", id: one.id }) },
    { label: t("Speed limit…"), icon: "speed-limiter", onSelect: () => ui.openDialog({ kind: "speedLimit", ids }) },
    { separator: true },
    { label: t("Move to queue"), icon: "queue", submenu: queueEntries },
    { label: t("Move up in queue"), icon: "chevron-up", disabled: !inQueue, onSelect: () => actions.moveInQueue(ids, -1) },
    { label: t("Move down in queue"), icon: "chevron-down", disabled: !inQueue, onSelect: () => actions.moveInQueue(ids, 1) },
    { separator: true },
    { label: ids.length > 1 ? t("Copy links") : t("Copy link"), icon: "copy", kbd: "Ctrl C", onSelect: () => void actions.copyLinks(ids) },
    { label: t("Details"), icon: "activity", disabled: !one, onSelect: () => one && actions.details(one.id) },
    { label: t("Properties…"), icon: "properties", kbd: "F2", disabled: !one, onSelect: () => one && ui.openDialog({ kind: "properties", id: one.id }) },
    { separator: true },
    { label: t("Delete"), icon: "delete", kbd: "Del", danger: true, onSelect: () => actions.requestDelete(ids) },
    { label: t("Delete with files…"), icon: "delete-completed", danger: true, onSelect: () => actions.requestDelete(ids, true) },
  ];
}

/** Probe one link and say what the check found. */
async function checkLink(id: string) {
  const results = await attempt(api.checkLinks([id]), t("Could not check link"));
  const r = results?.[0];
  if (!r) return;
  if (r.status === "dead") {
    toast(t("Dead link"), { kind: "error", body: r.message });
  } else if (r.status === "ok") {
    toast(t("Link is alive"));
  } else {
    toast(t("Could not tell"), { body: r.message });
  }
}
