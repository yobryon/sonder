// Mode selection: ?mode= wins; otherwise a Sonder server behind the page means ws.
import { test } from 'node:test';
import assert from 'node:assert/strict';

globalThis.location = { protocol: 'https:', href: 'https://user.github.io/sonder/' };
const { chooseMode, WS_PROTO } = await import('../static/transport.js');

test('auto-detect', () => {
  assert.equal(chooseMode(undefined, true), 'ws');
  assert.equal(chooseMode(undefined, false), 'worker');
  assert.equal(chooseMode(null, false), 'worker');
});

test('explicit override beats the probe', () => {
  assert.equal(chooseMode('worker', true), 'worker');
  assert.equal(chooseMode('ws', false), 'ws');
  assert.equal(chooseMode('bogus', true), 'ws');
});

test('ws protocol follows the page', () => {
  assert.equal(WS_PROTO, 'wss:');
});
