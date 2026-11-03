import { describe, expect, it } from 'vitest';
import type { MediaItem } from '../src/shared/media';

// capture.ts pulls in the background modules; stub the extension APIs before importing it.
(globalThis as unknown as { chrome: unknown }).chrome = {
  runtime: { id: 'test', getManifest: () => ({ version: '0.0.0' }) },
  storage: {
    local: { get: async () => ({}), set: async () => {} },
    session: { get: async () => ({}), set: async () => {} },
    onChanged: { addListener: () => {} },
  },
  cookies: { getAll: async () => [], getAllCookieStores: async () => [] },
  downloads: {},
  tabs: {},
  action: {},
  webRequest: {},
};

const { fallbackEligible, pickSniffedFallback } = await import('../src/background/capture');

const media = (key: string, url: string, kind: MediaItem['kind']): MediaItem => ({
  key,
  url,
  kind,
  streamKind: kind === 'hls' ? 'hls' : kind === 'dash' ? 'dash' : 'direct',
  name: key,
  ts: 0,
});

describe('fallbackEligible', () => {
  it('allows declines and skips about the download URL itself', () => {
    expect(fallbackEligible({ status: 'declined' })).toBe(true);
    expect(fallbackEligible({ status: 'skipped', reason: 'unsupported URL scheme' })).toBe(true);
    expect(fallbackEligible({ status: 'skipped', reason: 'type not in capture list (video/mp4)' })).toBe(true);
    expect(fallbackEligible({ status: 'skipped', reason: 'smaller than 1048576 bytes' })).toBe(true);
  });

  it('never overrides the user or an unreachable app', () => {
    expect(fallbackEligible({ status: 'accepted' })).toBe(false);
    expect(fallbackEligible({ status: 'unreachable', reason: 'no-host' })).toBe(false);
    expect(fallbackEligible({ status: 'skipped', reason: 'capture disabled' })).toBe(false);
    expect(fallbackEligible({ status: 'skipped', reason: 'capture paused' })).toBe(false);
    expect(fallbackEligible({ status: 'skipped', reason: 'bypass key held' })).toBe(false);
    expect(fallbackEligible({ status: 'skipped', reason: 'excluded site' })).toBe(false);
    expect(fallbackEligible({ status: 'skipped', reason: 'POST request' })).toBe(false);
  });
});

describe('pickSniffedFallback', () => {
  const download = 'https://cdn.example/challenged/file.mp4';
  const direct = 'https://cdn.example/media/file.mp4';

  it('picks the single differing video link', () => {
    expect(pickSniffedFallback([media('a', direct, 'video')], download, undefined)?.key).toBe('a');
    expect(pickSniffedFallback([media('a', direct, 'hls')], download, undefined)?.key).toBe('a');
    expect(pickSniffedFallback([media('a', direct, 'dash')], download, undefined)?.key).toBe('a');
  });

  it('refuses the same link, several links, and non-video media', () => {
    expect(pickSniffedFallback([media('a', download, 'video')], download, undefined)).toBeUndefined();
    expect(pickSniffedFallback([media('a', download, 'video')], 'https://other.example/x', download)).toBeUndefined();
    expect(
      pickSniffedFallback([media('a', direct, 'video'), media('b', 'https://cdn.example/media/other.mp4', 'video')], download, undefined),
    ).toBeUndefined();
    expect(pickSniffedFallback([media('a', direct, 'audio')], download, undefined)).toBeUndefined();
    expect(pickSniffedFallback([media('a', direct, 'segment')], download, undefined)).toBeUndefined();
    expect(pickSniffedFallback([], download, undefined)).toBeUndefined();
  });
});
