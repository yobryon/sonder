// Sonder — LLM scene composer (browser).
//
// `window.sonderComposer.open()` shows a modal: prompt, API key (localStorage only), model,
// "compose". The key goes straight from this browser to api.anthropic.com — no Sonder server
// is involved. The system prompt is the one the native `sonder compose` uses: it is built by
// the engine and shipped in `catalog.json` (written by scripts/build-site.sh), so both
// surfaces send identical context.
//
// Hooks the UI provides (both optional):
//   window.sonderTransport.validate(toml) -> Promise<string[]>   validation errors ([] = ok)
//   window.sonderApplyScene(toml)                                 load the scene into the engine

const API_URL = 'https://api.anthropic.com/v1/messages';
const API_VERSION = '2023-06-01';
const KEY_STORE = 'sonder.compose.apiKey';
const MODEL_STORE = 'sonder.compose.model';
const MAX_RETRIES = 3;

// [id, label, $/MTok in, $/MTok out]. First-party API list prices.
const MODELS = [
  ['claude-opus-5-5', 'Claude Opus 5.5 (default)', 4, 20],
  ['claude-sonnet-5-5', 'Claude Sonnet 5.5 (faster, cheaper)', 2, 10],
  ['claude-fable-5-1', 'Claude Fable 5.1 (most capable)', 10, 50],
  ['claude-haiku-4-5', 'Claude Haiku 4.5 (cheapest)', 1, 5],
];
// Models that accept the server-side refusal fallback.
const FALLBACK_MODELS = new Set(['claude-opus-5-5', 'claude-opus-5', 'claude-sonnet-5-5', 'claude-fable-5-1']);

let catalogPromise = null;
let modal = null;
let els = {};
let lastToml = '';

const store = {
  get(k) { try { return localStorage.getItem(k) || ''; } catch { return ''; } },
  set(k, v) { try { if (v) localStorage.setItem(k, v); else localStorage.removeItem(k); } catch { /* private mode etc. */ } },
};

function loadCatalog() {
  catalogPromise ||= fetch(new URL('./catalog.json', import.meta.url))
    .then((r) => { if (!r.ok) throw new Error(`catalog.json: HTTP ${r.status}`); return r.json(); })
    .catch((e) => { catalogPromise = null; throw e; });
  return catalogPromise;
}

/** Fallback when catalog.json has no prebuilt prompt: a minimal reference from its lists. */
function promptFromCatalog(cat) {
  if (cat.system_prompt) return cat.system_prompt;
  const src = (cat.sources || []).map((s) => {
    const params = (s.params || []).map((p) => `${p.name}${p.default != null ? '=' + p.default : ' (required)'}`).join(', ');
    return `- ${s.kind} — ${s.summary}${params ? `; params: ${params}` : ''}${s.channels?.length ? `; channels: ${s.channels.join(', ')}` : ''}${s.events?.length ? `; events: ${s.events.join(', ')}` : ''}`;
  });
  const rec = (cat.recipes || []).map((r) => `- ${r.kind} — ${r.summary}${r.params?.length ? `; params: ${r.params.map((p) => p.name).join(', ')}` : ''}`);
  const pre = (cat.presets || []).map((p) => `- ${p.name} — ${p.description}`);
  return `You write Sonder scenes: TOML with [conductor] (key, mode, tempo) and [[layer]] tables (name, role, instrument, source = { kind = ... }, signals.x = "expr", map.key = "expr", on_event.<event>.<key> = "expr"). Use only these source kinds:\n${src.join('\n')}\nRecipes:\n${rec.join('\n')}\nInstrument presets:\n${pre.join('\n')}\nReturn ONLY a single \`\`\`toml block.`;
}

/** First fenced toml block, else first fenced block, else the text if it looks like a scene. */
export function extractTomlBlock(text) {
  const re = /```([^\n]*)\n([\s\S]*?)```/g;
  let first = null;
  for (const m of text.matchAll(re)) {
    const body = m[2].replace(/\s+$/, '') + '\n';
    if (m[1].trim().toLowerCase() === 'toml') return body;
    first ??= body;
  }
  if (first) return first;
  const t = text.trim();
  return t.includes('[[layer]]') && t.includes('[conductor]') ? t + '\n' : null;
}

export function retryMessage(errs) {
  return 'The scene did not validate. Fix every error below and return the WHOLE corrected scene as a single ```toml block, nothing else.\n\nErrors:\n'
    + errs.map((e) => `- ${e}\n`).join('');
}

function costEstimate(model, usage) {
  const m = MODELS.find((x) => x[0] === model);
  if (!m) return null;
  const [, , inP, outP] = m;
  return (usage.input * inP + usage.cacheWrite * inP * 1.25 + usage.cacheRead * inP * 0.1 + usage.output * outP) * 1e-6;
}

async function callMessages({ key, model, system, messages, signal }) {
  const fallback = FALLBACK_MODELS.has(model);
  const headers = {
    'content-type': 'application/json',
    'x-api-key': key,
    'anthropic-version': API_VERSION,
    // Required for calls made directly from a browser (CORS). The key is the user's own.
    'anthropic-dangerous-direct-browser-access': 'true',
  };
  if (fallback) headers['anthropic-beta'] = 'server-side-fallback-2026-07-01';
  const body = {
    model,
    max_tokens: 16000,
    system: [{ type: 'text', text: system, cache_control: { type: 'ephemeral' } }],
    messages,
  };
  if (fallback) body.fallbacks = 'default';
  const res = await fetch(API_URL, { method: 'POST', headers, body: JSON.stringify(body), signal });
  const json = await res.json().catch(() => ({}));
  if (!res.ok) {
    const err = json?.error || {};
    throw new Error(`Claude API ${res.status}: ${err.type || 'error'}: ${err.message || res.statusText}`);
  }
  const text = (json.content || []).filter((b) => b.type === 'text').map((b) => b.text).join('');
  const u = json.usage || {};
  return {
    text,
    stopReason: json.stop_reason,
    refusal: json.stop_details?.explanation,
    usage: { input: u.input_tokens || 0, output: u.output_tokens || 0, cacheRead: u.cache_read_input_tokens || 0, cacheWrite: u.cache_creation_input_tokens || 0 },
  };
}

async function validateToml(toml) {
  const v = window.sonderTransport?.validate;
  if (typeof v !== 'function') return { errors: [], checked: false };
  try {
    const errors = await v.call(window.sonderTransport, toml);
    return { errors: Array.isArray(errors) ? errors : [], checked: true };
  } catch (e) {
    return { errors: [String(e?.message || e)], checked: true };
  }
}

/** The compose loop. `log(line)` streams progress to the UI. Returns the TOML or throws. */
export async function composeScene({ prompt, key, model, system, log = () => {}, signal }) {
  const messages = [{ role: 'user', content: prompt }];
  const total = { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 };
  let lastErrs = [];
  let toml = null;
  for (let attempt = 0; attempt <= MAX_RETRIES; attempt++) {
    log(attempt === 0 ? `asking ${model}…` : `attempt ${attempt + 1}/${MAX_RETRIES + 1}: sending ${lastErrs.length} validation error(s) back…`);
    const reply = await callMessages({ key, model, system, messages, signal });
    for (const k of Object.keys(total)) total[k] += reply.usage[k];
    if (reply.stopReason === 'refusal') throw new Error(`the model declined this request${reply.refusal ? ': ' + reply.refusal : ''}`);
    if (reply.stopReason === 'max_tokens') log('warning: reply hit max_tokens; the scene may be cut off');
    const candidate = extractTomlBlock(reply.text);
    let errs;
    if (!candidate) {
      errs = ['reply contained no ```toml block'];
    } else {
      const { errors, checked } = await validateToml(candidate);
      if (!checked) log('note: no validator available in this page; the scene was not checked');
      if (!errors.length) { toml = candidate; break; }
      errs = errors;
    }
    for (const e of errs) log(`  invalid: ${e}`);
    messages.push({ role: 'assistant', content: reply.text }, { role: 'user', content: retryMessage(errs) });
    lastErrs = errs;
  }
  const cost = costEstimate(model, total);
  log(`tokens in ${total.input} (cache write ${total.cacheWrite}, read ${total.cacheRead}), out ${total.output}${cost != null ? ` ≈ $${cost.toFixed(4)}` : ''}`);
  if (!toml) throw new Error(`no valid scene after ${MAX_RETRIES + 1} attempts:\n${lastErrs.join('\n')}`);
  return toml;
}

// ── UI ───────────────────────────────────────────────────────────────────────────────

function h(tag, attrs = {}, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === 'class') el.className = v;
    else if (k.startsWith('on')) el.addEventListener(k.slice(2), v);
    else if (v !== false && v != null) el.setAttribute(k, v === true ? '' : v);
  }
  for (const c of children) if (c != null) el.append(c);
  return el;
}

function ensureCss() {
  if (document.querySelector('link[data-sonder-compose]')) return;
  document.head.append(h('link', { rel: 'stylesheet', href: new URL('./compose.css', import.meta.url).href, 'data-sonder-compose': '' }));
}

function build() {
  ensureCss();
  els.prompt = h('textarea', { class: 'sc-prompt', rows: 4, placeholder: 'e.g. a slow, dark piece from live earthquakes and the solar wind — sub-bass hits, a cold pad, sparse bells' });
  els.key = h('input', { class: 'sc-key', type: 'password', autocomplete: 'off', spellcheck: 'false', placeholder: 'sk-ant-…' });
  els.key.value = store.get(KEY_STORE);
  els.remember = h('input', { type: 'checkbox' });
  els.remember.checked = !!els.key.value;
  els.model = h('select', { class: 'sc-model' }, ...MODELS.map(([id, label]) => h('option', { value: id }, label)));
  els.model.value = store.get(MODEL_STORE) || MODELS[0][0];
  els.go = h('button', { class: 'sc-go', onclick: onCompose }, 'compose');
  els.log = h('pre', { class: 'sc-log mono', hidden: true });
  els.out = h('textarea', { class: 'sc-out mono', rows: 18, spellcheck: 'false', hidden: true });
  els.apply = h('button', { class: 'sc-apply', hidden: true, onclick: onApply }, 'apply');
  els.copy = h('button', { hidden: true, onclick: onCopy }, 'copy');
  els.status = h('span', { class: 'sc-status dim' });

  const keyRow = h('label', { class: 'sc-field' },
    h('span', { class: 'sc-label' }, 'Anthropic API key'),
    els.key,
    h('label', { class: 'sc-remember dim' }, els.remember, ' remember in this browser'),
    h('div', { class: 'sc-warning' },
      'Your key is kept only in this browser (localStorage) and sent straight to api.anthropic.com — never to a Sonder server. ',
      'Anyone with access to this browser profile can read it; untick "remember" on a shared machine. Usage is billed to your Anthropic account.'));

  modal = h('div', { class: 'sc-backdrop', onclick: (e) => { if (e.target === modal) close(); } },
    h('div', { class: 'sc-modal', role: 'dialog', 'aria-modal': 'true', 'aria-label': 'Compose a scene' },
      h('div', { class: 'sc-head' }, h('h2', {}, 'Compose a scene'), h('button', { class: 'sc-close', 'aria-label': 'close', onclick: close }, '×')),
      h('label', { class: 'sc-field' }, h('span', { class: 'sc-label' }, 'Describe the music and the data'), els.prompt),
      keyRow,
      h('div', { class: 'sc-row' },
        h('label', { class: 'sc-field sc-grow' }, h('span', { class: 'sc-label' }, 'Model'), els.model),
        els.go, els.status),
      els.log,
      els.out,
      h('div', { class: 'sc-row sc-actions' }, els.apply, els.copy)));
  document.addEventListener('keydown', (e) => { if (e.key === 'Escape' && modal.isConnected) close(); });
  return modal;
}

function log(line) {
  els.log.hidden = false;
  els.log.textContent += (els.log.textContent ? '\n' : '') + line;
  els.log.scrollTop = els.log.scrollHeight;
}

let inflight = null;

async function onCompose() {
  const prompt = els.prompt.value.trim();
  const key = els.key.value.trim();
  const model = els.model.value;
  if (!prompt) { els.prompt.focus(); return; }
  if (!key) { els.key.focus(); els.status.textContent = 'an API key is needed'; return; }
  store.set(KEY_STORE, els.remember.checked ? key : '');
  store.set(MODEL_STORE, model);
  if (inflight) { inflight.abort(); inflight = null; els.go.textContent = 'compose'; els.status.textContent = 'cancelled'; return; }
  inflight = new AbortController();
  els.go.textContent = 'cancel';
  els.status.textContent = '';
  els.log.textContent = '';
  els.out.hidden = els.apply.hidden = els.copy.hidden = true;
  try {
    const cat = await loadCatalog().catch((e) => { log(`warning: ${e.message}; composing without the full catalog`); return {}; });
    const system = promptFromCatalog(cat);
    lastToml = await composeScene({ prompt, key, model, system, log, signal: inflight.signal });
    els.out.value = lastToml;
    els.out.hidden = els.copy.hidden = false;
    els.apply.hidden = typeof window.sonderApplyScene !== 'function';
    els.status.textContent = 'valid scene';
  } catch (e) {
    if (e?.name === 'AbortError') log('cancelled');
    else { log(`error: ${e.message || e}`); els.status.textContent = 'failed'; }
  } finally {
    inflight = null;
    els.go.textContent = 'compose';
  }
}

function onApply() {
  const toml = els.out.value;
  try {
    window.sonderApplyScene?.(toml);
    els.status.textContent = 'applied';
    close();
  } catch (e) {
    log(`apply failed: ${e.message || e}`);
  }
}

async function onCopy() {
  try { await navigator.clipboard.writeText(els.out.value); els.status.textContent = 'copied'; }
  catch { els.out.select(); els.status.textContent = 'select and copy manually'; }
}

function open() {
  if (!modal) build();
  if (!modal.isConnected) document.body.append(modal);
  loadCatalog().catch(() => {});
  (els.key.value ? els.prompt : els.key).focus();
}

function close() {
  modal?.remove();
}

window.sonderComposer = { open, close, composeScene, extractTomlBlock, retryMessage };
export { open, close };
