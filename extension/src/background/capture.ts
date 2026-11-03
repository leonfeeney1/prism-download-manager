/**
 * Download capture.
 *
 * Chromium: `downloads.onDeterminingFilename` fires after the response headers
 * and before the Save-As dialog; we hold the download (return true), ask the
 * app, and on acceptance cancel + erase it. If the app is unreachable or
 * declines we call `suggest()` and the browser downloads normally (fail open).
 * One exception: when the download itself can't be captured but the page's
 * sniffer holds exactly one video/stream link different from the download's
 * own links, that sniffed link is handed to Prism instead (Fetchly-style:
 * the browser download is cancelled only after Prism accepts it).
 *
 * Firefox (no onDeterminingFilename): blocking `webRequest.onHeadersReceived`
 * cancels `Content-Disposition: attachment` navigations before the download
 * starts; `downloads.onCreated` + cancel/erase is the backstop for the rest.
 */
import { BypassTracker } from '../shared/bypass';
import { api } from '../shared/api';
import type { NativeError } from '../shared/correlator';
import { type MediaItem, visibleMedia } from '../shared/media';
import type { DownloadRequest } from '../shared/protocol';
import { type CaptureCandidate, decideCapture, filenameFromContentDisposition, filenameFromUrl, isAttachment } from '../shared/rules';
import { showUnreachable } from './badge';
import { actionTimeout, cookieHeaderFor, tabInfo, userAgent } from './context';
import { native } from './native';
import { type RequestInfo, requestByUrl } from './requests';
import { getTabMedia } from './sniffer';
import { getState } from './state';
import { downloadMedia } from './video';

export const bypass = new BypassTracker();

/** Timing of the last hand-off (exposed for diagnostics / e2e). */
export const lastCapture: { url?: string; decidedMs?: number; deliveredMs?: number; result?: string } = {};

function basename(p: string | undefined): string | undefined {
  if (!p) return undefined;
  const b = p.split(/[\\/]/).pop();
  return b || undefined;
}

function sameOrigin(a: string, b: string): boolean {
  try {
    return new URL(a).origin === new URL(b).origin;
  } catch {
    return false;
  }
}

interface HandOffInput {
  candidate: CaptureCandidate;
  req?: RequestInfo;
  incognito: boolean;
}

/** What the hand-off decided: `skipped` carries decideCapture's reason, `unreachable` the native error code. */
export interface HandOffResult {
  status: 'accepted' | 'declined' | 'skipped' | 'unreachable';
  reason?: string;
}

/**
 * When a download the browser started can't be captured itself, the page's sniffer list may
 * still hold the working media link. Only when the failure is about the download's own URL
 * (its scheme or type can't be handed off, it is smaller than the capture minimum, or the app
 * declined it): never when the user paused capture, held the bypass key, excluded the site,
 * or the app is unreachable (a video send would fail there too, and only stall the download).
 */
export function fallbackEligible(result: HandOffResult): boolean {
  if (result.status === 'declined') return true;
  if (result.status !== 'skipped' || !result.reason) return false;
  return (
    result.reason === 'unsupported URL scheme' ||
    result.reason.startsWith('type not in capture list') ||
    result.reason.startsWith('smaller than ')
  );
}

/**
 * The tab's single sniffed video/stream whose link differs from the download's own links,
 * if exactly one qualifies: with more than one, guessing would risk the wrong file, and with
 * the same link the app would only hit the same refusal again.
 */
export function pickSniffedFallback(
  media: MediaItem[],
  downloadUrl: string | undefined,
  finalUrl: string | undefined,
): MediaItem | undefined {
  const candidates = visibleMedia(media).filter(
    (m) => (m.kind === 'video' || m.kind === 'hls' || m.kind === 'dash') && m.url !== downloadUrl && m.url !== finalUrl,
  );
  return candidates.length === 1 ? candidates[0] : undefined;
}

/**
 * Decides and, if captured, hands the download to the app.
 * Resolves `accepted` only when the app accepted it (the caller then cancels the browser download).
 */
export async function maybeHandOff({ candidate, req, incognito }: HandOffInput): Promise<HandOffResult> {
  const t0 = performance.now();
  const st = await getState();
  const isBypass = bypass.isActive([candidate.url, candidate.finalUrl, req?.originalUrl]);
  const decision = decideCapture(candidate, st.settings, { paused: st.paused, bypass: isBypass });
  lastCapture.url = candidate.url;
  lastCapture.decidedMs = performance.now() - t0;
  delete lastCapture.deliveredMs;
  lastCapture.result = decision.capture ? 'pending' : undefined;
  if (!decision.capture) {
    lastCapture.result = `skipped: ${decision.reason}`;
    console.debug('[prism] not captured:', decision.reason, candidate.url);
    return { status: 'skipped', reason: decision.reason };
  }
  const url = candidate.url;
  const finalUrl = candidate.finalUrl && candidate.finalUrl !== url ? candidate.finalUrl : undefined;
  // The page that started the download, as the browser's Referer names it: a download another site
  // started must not gain the SameSite=Strict cookies the browser withheld from it.
  const capture = { initiator: req?.referer || candidate.referrer || undefined };
  const [cookies, finalCookies] = await Promise.all([
    cookieHeaderFor(url, incognito, req && req.url === url ? req.cookie : undefined, capture),
    finalUrl && !sameOrigin(url, finalUrl) ? cookieHeaderFor(finalUrl, incognito, req?.cookie, capture) : Promise.resolve(undefined),
  ]);
  const headers: Record<string, string> = {};
  if (req?.origin) headers.Origin = req.origin;
  const msg: DownloadRequest = {
    type: 'download',
    url,
    ...(finalUrl ? { finalUrl } : {}),
    ...(req?.referer || candidate.referrer ? { referrer: req?.referer || candidate.referrer } : {}),
    ...(candidate.pageUrl ? { pageUrl: candidate.pageUrl } : {}),
    ...(candidate.filename ? { filename: candidate.filename } : {}),
    ...(candidate.fileSize && candidate.fileSize > 0 ? { fileSize: candidate.fileSize } : {}),
    ...(candidate.mime ? { mime: candidate.mime } : {}),
    method: 'GET',
    ...(Object.keys(headers).length ? { headers } : {}),
    cookies,
    ...(finalCookies !== undefined ? { finalUrlCookies: finalCookies } : {}),
    userAgent: userAgent(),
    incognito,
    source: 'capture',
  };
  try {
    const reply = await native.request({ ...msg, type: 'download' }, { timeoutMs: actionTimeout() });
    lastCapture.deliveredMs = performance.now() - t0;
    lastCapture.result = reply.accepted === false ? 'declined by app' : 'handed off';
    return reply.accepted === false ? { status: 'declined' } : { status: 'accepted' };
  } catch (e) {
    const err = e as NativeError;
    lastCapture.deliveredMs = performance.now() - t0;
    lastCapture.result = `fail-open: ${err.code}`;
    console.warn('[prism] hand-off failed, letting the browser download:', err.code, err.message);
    showUnreachable(true, `download kept in browser (${err.code})`);
    return { status: 'unreachable', reason: err.code };
  }
}

async function cancelAndErase(id: number): Promise<void> {
  try {
    await api.downloads.cancel(id);
  } catch {
    /* already gone */
  }
  try {
    await api.downloads.erase({ id });
  } catch {
    /* ignore */
  }
}

// ---- Chromium -------------------------------------------------------------

async function onDetermining(item: chrome.downloads.DownloadItem, suggest: (s?: chrome.downloads.FilenameSuggestion) => void): Promise<void> {
  let released = false;
  const release = () => {
    if (released) return;
    released = true;
    try {
      suggest();
    } catch {
      /* the download may be gone already */
    }
  };
  try {
    if (item.byExtensionId && item.byExtensionId === api.runtime.id) return release();
    const req = requestByUrl(item.finalUrl) ?? requestByUrl(item.url);
    const tab = await tabInfo(req?.tabId);
    const pageUrl = tab?.url && /^https?:/i.test(tab.url) ? tab.url : item.referrer || undefined;
    const size = item.totalBytes > 0 ? item.totalBytes : item.fileSize > 0 ? item.fileSize : undefined;
    const filename = basename(item.filename) || filenameFromUrl(item.finalUrl || item.url) || undefined;
    const result = await maybeHandOff({
      candidate: {
        url: item.url,
        finalUrl: item.finalUrl,
        ...(filename ? { filename } : {}),
        ...(item.mime ? { mime: item.mime } : {}),
        ...(size ? { fileSize: size } : {}),
        ...(pageUrl ? { pageUrl } : {}),
        ...(item.referrer ? { referrer: item.referrer } : {}),
        ...(req?.method ? { method: req.method } : {}),
      },
      ...(req ? { req } : {}),
      incognito: item.incognito,
    });
    if (result.status === 'accepted') {
      // Cancel before releasing so no Save-As dialog ever appears.
      await cancelAndErase(item.id);
      return release();
    }
    // The download itself couldn't be captured, but the page's sniffer may hold the working
    // media link: hand that to Prism instead, and cancel the browser download only after
    // Prism accepts it. Anything else (no single link, Prism declines it too) falls through
    // to the browser download, exactly as before.
    if (fallbackEligible(result) && req?.tabId !== undefined) {
      try {
        const pick = pickSniffedFallback(await getTabMedia(req.tabId), item.url, item.finalUrl);
        if (pick) {
          await downloadMedia(req.tabId, pick.key);
          await cancelAndErase(item.id);
          return release();
        }
      } catch {
        /* the sniffed link didn't take either: the browser download stands */
      }
    }
    release();
  } catch (e) {
    console.error('[prism] capture error', e);
    release();
  }
}

export function initChromiumCapture(): boolean {
  const ev = api.downloads.onDeterminingFilename;
  if (!ev) return false;
  ev.addListener((item, suggest) => {
    void onDetermining(item, suggest);
    return true; // async suggest
  });
  return true;
}

// ---- Firefox --------------------------------------------------------------

/** URLs already handed off by the webRequest path (so onCreated doesn't double-handle). */
const handedOff = new Map<string, number>();

function markHandedOff(url: string): void {
  const now = Date.now();
  for (const [k, t] of handedOff) if (now - t > 30_000) handedOff.delete(k);
  handedOff.set(url, now);
}

export function initFirefoxCapture(): void {
  const wr = api.webRequest as typeof api.webRequest;
  wr.onHeadersReceived.addListener(
    ((d: chrome.webRequest.OnHeadersReceivedDetails & { incognito?: boolean; documentUrl?: string; originUrl?: string }) => {
      if (d.statusCode !== 200 || d.method !== 'GET') return {};
      const h = (n: string) => d.responseHeaders?.find((x) => x.name.toLowerCase() === n)?.value;
      const cd = h('content-disposition');
      if (!isAttachment(cd)) return {}; // inline responses: left to downloads.onCreated
      const len = Number(h('content-length'));
      const req = requestByUrl(d.url);
      const filename = filenameFromContentDisposition(cd) || filenameFromUrl(d.url) || undefined;
      const pageUrl = d.documentUrl || d.originUrl;
      return maybeHandOff({
        candidate: {
          url: req?.originalUrl ?? d.url,
          finalUrl: d.url,
          ...(filename ? { filename } : {}),
          ...(h('content-type') ? { mime: h('content-type')!.split(';')[0]!.trim() } : {}),
          ...(len > 0 ? { fileSize: len } : {}),
          ...(pageUrl ? { pageUrl } : {}),
          ...(req?.referer ? { referrer: req.referer } : {}),
          method: d.method,
        },
        ...(req ? { req } : {}),
        incognito: !!d.incognito,
      }).then((result) => {
        const accepted = result.status === 'accepted';
        if (accepted) markHandedOff(d.url);
        return accepted ? { cancel: true } : {};
      });
    }) as unknown as (d: chrome.webRequest.OnHeadersReceivedDetails) => chrome.webRequest.BlockingResponse | undefined,
    { urls: ['<all_urls>'], types: ['main_frame', 'sub_frame'] as chrome.webRequest.ResourceType[] },
    ['blocking', 'responseHeaders'] as chrome.webRequest.OnHeadersReceivedOptions[],
  );

  api.downloads.onCreated.addListener((item) => {
    void (async () => {
      if (item.state !== 'in_progress' || handedOff.has(item.url)) return;
      const req = requestByUrl(item.url);
      const filename = basename(item.filename) || filenameFromUrl(item.url) || undefined;
      const size = item.totalBytes > 0 ? item.totalBytes : undefined;
      const result = await maybeHandOff({
        candidate: {
          url: req?.originalUrl ?? item.url,
          finalUrl: item.url,
          ...(filename ? { filename } : {}),
          ...(item.mime ? { mime: item.mime } : {}),
          ...(size ? { fileSize: size } : {}),
          ...(item.referrer ? { referrer: item.referrer, pageUrl: item.referrer } : {}),
          ...(req?.method ? { method: req.method } : {}),
        },
        ...(req ? { req } : {}),
        incognito: item.incognito,
      });
      if (result.status === 'accepted') await cancelAndErase(item.id);
    })();
  });
}
