// A scene lives in a link: `#s=<base64url(deflate-raw(toml))>`.
//
// Encoding uses the browser's CompressionStream('deflate-raw'); where that is missing the
// payload is plain base64url of the UTF-8 text, marked with a leading `~` so decoding never
// has to guess. Pure functions, no DOM: node runs the tests against this file.

const PLAIN = '~';

export function b64urlEncode(bytes) {
  let bin = '';
  for (let i = 0; i < bytes.length; i += 0x8000) bin += String.fromCharCode.apply(null, bytes.subarray(i, i + 0x8000));
  return btoa(bin).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

export function b64urlDecode(s) {
  const b64 = s.replace(/-/g, '+').replace(/_/g, '/') + '='.repeat((4 - (s.length % 4)) % 4);
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

async function through(bytes, stream) {
  const body = new Blob([bytes]).stream().pipeThrough(stream);
  return new Uint8Array(await new Response(body).arrayBuffer());
}

export const hasDeflate = () => typeof CompressionStream === 'function' && typeof DecompressionStream === 'function';

/** TOML → hash payload (without the `#s=`). `deflate: false` forces the plain fallback. */
export async function encodeScene(toml, { deflate = hasDeflate() } = {}) {
  const utf8 = new TextEncoder().encode(toml);
  if (deflate) {
    try {
      return b64urlEncode(await through(utf8, new CompressionStream('deflate-raw')));
    } catch (_) { /* fall through to plain */ }
  }
  return PLAIN + b64urlEncode(utf8);
}

/** Hash payload → TOML. Throws on garbage. */
export async function decodeScene(payload) {
  const dec = new TextDecoder();
  if (payload.startsWith(PLAIN)) return dec.decode(b64urlDecode(payload.slice(1)));
  const bytes = b64urlDecode(payload);
  if (!hasDeflate()) throw new Error('this browser cannot inflate the scene link');
  return dec.decode(await through(bytes, new DecompressionStream('deflate-raw')));
}

/** The `s` payload in a location hash (`#s=…`, possibly among other `&`-separated fields). */
export function scenePayloadFromHash(hash) {
  const h = (hash || '').replace(/^#/, '');
  for (const part of h.split('&')) {
    if (part.startsWith('s=')) return decodeURIComponent(part.slice(2));
  }
  return null;
}

/** Build the shareable link for `toml` on top of `href` (query dropped, hash replaced). */
export async function sceneLink(href, toml, opts) {
  const u = new URL(href);
  u.search = '';
  u.hash = `s=${await encodeScene(toml, opts)}`;
  return u.href;
}
