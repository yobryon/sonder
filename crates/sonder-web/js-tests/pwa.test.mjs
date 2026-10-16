// PWA glue: precache list builder, service-worker routing, hidden-tab rate, media metadata.
import { test } from 'node:test';
import assert from 'node:assert/strict';

const { SHELL, shellPath, precacheList, requestKind, cacheName, snapshotHz, mediaMetadata, isStandalone, media, install } =
  await import('../static/pwa.js');

test('shellPath normalises to ./ form', () => {
  assert.equal(shellPath(''), './');
  assert.equal(shellPath('.'), './');
  assert.equal(shellPath('/app.js'), './app.js');
  assert.equal(shellPath('./pkg/x.wasm'), './pkg/x.wasm');
  assert.equal(shellPath('sources/usgs.js'), './sources/usgs.js');
});

test('precacheList merges the shell with the on-disk list, de-duplicates and excludes live paths', () => {
  const extra = ['app.js', 'sources/usgs.js', './pkg/sonder_wasm_bg.wasm', 'sw.js', 'records/2026-10-15/a.mp3', 'api/snapshot', '.nojekyll', 'catalog.json'];
  const list = precacheList(extra);
  assert.deepEqual(list, [...new Set(list)], 'no duplicates');
  assert.deepEqual(list, [...list].sort(), 'sorted');
  for (const s of SHELL) assert.ok(list.includes(shellPath(s)), `shell file ${s} present`);
  assert.ok(list.includes('./sources/usgs.js'));
  assert.ok(!list.includes('./sw.js'));
  assert.ok(!list.some((p) => p.startsWith('./records/')));
  assert.ok(!list.includes('./api/snapshot'));
  assert.ok(!list.includes('./.nojekyll'));
  assert.equal(list.filter((p) => p === './app.js').length, 1);
  assert.equal(precacheList().length, SHELL.length, 'default list is just the shell');
});

test('the sw.js template carries the markers the build script replaces', async () => {
  const fs = await import('node:fs');
  const src = fs.readFileSync(new URL('../static/sw.js', import.meta.url), 'utf8');
  assert.ok(src.includes("'__BUILD__'"));
  assert.ok(src.includes('/*__PRECACHE__*/[]'));
});

test('requestKind routes shell / network / bypass under a sub-path scope', () => {
  const scope = 'https://user.github.io/sonder/';
  const shell = new Set(precacheList(['sources/usgs.js', 'pkg/sonder_wasm_bg.wasm']));
  assert.equal(requestKind('https://user.github.io/sonder/', scope, shell), 'shell');
  assert.equal(requestKind('https://user.github.io/sonder/app.js?x=1', scope, shell), 'shell');
  assert.equal(requestKind('https://user.github.io/sonder/pkg/sonder_wasm_bg.wasm', scope, shell), 'shell');
  assert.equal(requestKind('https://user.github.io/sonder/sources/usgs.js', scope, shell), 'shell');
  assert.equal(requestKind('https://user.github.io/sonder/fonts/x.woff2', scope, shell), 'network');
  assert.equal(requestKind('https://user.github.io/sonder/api/snapshot', scope, shell), 'bypass');
  assert.equal(requestKind('https://user.github.io/sonder/ws', scope, shell), 'bypass');
  assert.equal(requestKind('https://user.github.io/sonder/audio', scope, shell), 'bypass');
  assert.equal(requestKind('https://user.github.io/sonder/records/2026/a.mp3', scope, shell), 'bypass');
  assert.equal(requestKind('https://user.github.io/other/app.js', scope, shell), 'bypass');
  assert.equal(requestKind('https://earthquake.usgs.gov/feed.json', scope, shell), 'bypass');
  assert.equal(requestKind('wss://ws-feed.exchange.coinbase.com/', scope, shell), 'bypass');
  assert.equal(requestKind('not a url', scope, shell), 'bypass');
});

test('cache name follows the build; snapshot rate halves when hidden', () => {
  assert.equal(cacheName('abc123'), 'sonder-abc123');
  assert.equal(cacheName(''), 'sonder-dev');
  assert.equal(snapshotHz(true), 2);
  assert.equal(snapshotHz(false), 25);
});

test('media metadata names the scene and the artist', () => {
  const m = mediaMetadata('Night Freeway');
  assert.equal(m.title, 'Night Freeway');
  assert.equal(m.artist, 'Sonder');
  assert.ok(m.artwork.length >= 2);
  assert.equal(mediaMetadata('').title, 'Sonder');
});

test('isStandalone and the page helpers never throw without a browser', () => {
  assert.equal(isStandalone(undefined), false);
  assert.equal(isStandalone({ matchMedia: () => { throw new Error('nope'); } }), false);
  assert.equal(isStandalone({ matchMedia: () => ({ matches: true }) }), true);
  assert.equal(isStandalone({ matchMedia: () => ({ matches: false }), navigator: { standalone: true } }), true);
  media.setTitle('x'); media.setPlaying(true); media.setPlaying(false);
  assert.doesNotThrow(() => install({ transport: { mode: 'worker' }, installButton: null, onPlay() {}, onPause() {} }));
});
