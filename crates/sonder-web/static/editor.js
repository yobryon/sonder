// In-browser scene editor: a drawer with the scene TOML, live validation, apply (restart the
// engine with the text), fork a built-in, save/load named scenes in localStorage, share
// link, and a hook for the LLM composer (compose.js, when present).
//
//   const ed = createEditor(root, {
//     validate: async (toml) => string[],      // [] when valid
//     apply:    async (toml) => void,          // restart with this scene (throws on failure)
//     share:    async (toml) => string,        // returns the link it put on the clipboard
//     builtins: () => [{name, title, toml}],   // for "fork"
//   });
//   ed.toggle(); ed.show(); ed.hide(); ed.setToml(text); ed.getToml(); ed.flash(msg)

export const STORE_KEY = 'sonder.scenes';

export function loadSaved() {
  try { return JSON.parse(localStorage.getItem(STORE_KEY) || '{}') || {}; } catch (_) { return {}; }
}
export function storeSaved(map) {
  try { localStorage.setItem(STORE_KEY, JSON.stringify(map)); } catch (_) { /* quota / private mode */ }
}

/** `name = "…"` from a scene TOML, or the fallback. */
export function sceneTitle(toml, fallback = 'custom scene') {
  const m = /^\s*name\s*=\s*"([^"]*)"/m.exec(toml || '');
  return m && m[1] ? m[1] : fallback;
}

const esc = (s) => String(s).replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));

export function createEditor(root, hooks) {
  root.className = 'editor';
  root.innerHTML = `
    <div class="ehead">
      <h2>Scene editor <span class="hint dim">TOML · <kbd>e</kbd> toggles</span></h2>
      <button class="btn ghost eclose" title="close (Esc)">×</button>
    </div>
    <div class="etools">
      <button class="btn primary e-apply" title="validate and restart the engine with this scene">apply</button>
      <button class="btn e-validate" title="check the scene without applying">validate</button>
      <select class="e-fork" title="copy a built-in scene into the editor"><option value="">fork a built-in…</option></select>
      <span class="egap"></span>
      <button class="btn e-share" title="copy a link that carries this scene">share</button>
      <button class="btn e-compose" title="describe a scene; an LLM writes the TOML (needs compose.js and your API key)">compose with AI</button>
    </div>
    <div class="etools">
      <input class="e-name" placeholder="scene name" title="name to save under">
      <button class="btn e-save" title="save to this browser (localStorage)">save</button>
      <select class="e-load" title="your saved scenes"><option value="">load saved…</option></select>
      <button class="btn e-del" title="delete the selected saved scene">delete</button>
    </div>
    <textarea class="e-text mono" spellcheck="false" placeholder="# paste or write a scene TOML here"></textarea>
    <ul class="e-errors"></ul>
    <div class="e-status dim"></div>
    <input class="e-link mono" readonly hidden title="share link">`;

  const q = (sel) => root.querySelector(sel);
  const text = q('.e-text'), errors = q('.e-errors'), status = q('.e-status'), link = q('.e-link');
  const fork = q('.e-fork'), load = q('.e-load'), nameIn = q('.e-name');
  let statusTimer = null;
  let validateTimer = null;
  let lastValidated = '';

  const flash = (msg, bad = false) => {
    status.textContent = msg || '';
    status.className = `e-status ${bad ? 'bad' : 'dim'}`;
    clearTimeout(statusTimer);
    if (msg) statusTimer = setTimeout(() => { status.textContent = ''; }, 6000);
  };
  const showErrors = (errs) => {
    errors.innerHTML = (errs || []).map((e) => `<li>${esc(e)}</li>`).join('');
    root.classList.toggle('invalid', !!(errs && errs.length));
    root.classList.toggle('valid', !!(errs && !errs.length && text.value.trim()));
  };
  const validate = async (quiet = false) => {
    const toml = text.value;
    lastValidated = toml;
    if (!toml.trim()) { showErrors([]); return []; }
    let errs;
    try { errs = await hooks.validate(toml); } catch (e) { errs = [`validation failed: ${e.message || e}`]; }
    if (text.value !== toml) return errs; // stale: a newer edit is on its way
    showErrors(errs);
    if (!quiet) flash(errs.length ? `${errs.length} error${errs.length === 1 ? '' : 's'}` : 'valid');
    return errs;
  };

  const refreshSaved = () => {
    const saved = loadSaved();
    load.innerHTML = '<option value="">load saved…</option>' + Object.keys(saved).sort().map((n) => `<option value="${esc(n)}">${esc(n)}</option>`).join('');
  };
  const refreshForks = () => {
    const list = (hooks.builtins && hooks.builtins()) || [];
    fork.innerHTML = '<option value="">fork a built-in…</option>' + list.map((s) => `<option value="${esc(s.name)}">${esc(s.title || s.name)}</option>`).join('');
  };

  text.addEventListener('input', () => {
    clearTimeout(validateTimer);
    validateTimer = setTimeout(() => validate(true), 400);
  });
  q('.e-validate').addEventListener('click', () => validate());
  q('.e-apply').addEventListener('click', async () => {
    const toml = text.value;
    const errs = await validate(true);
    if (errs.length) { flash('fix the errors first', true); return; }
    try {
      await hooks.apply(toml);
      flash(`applied “${sceneTitle(toml)}”`);
    } catch (e) {
      showErrors([e.message || String(e)]);
      flash('apply failed', true);
    }
  });
  fork.addEventListener('change', () => {
    const s = ((hooks.builtins && hooks.builtins()) || []).find((x) => x.name === fork.value);
    fork.value = '';
    if (!s) return;
    text.value = s.toml.replace(/^(\s*name\s*=\s*")([^"]*)(")/m, '$1$2 (fork)$3');
    nameIn.value = `${s.name}-fork`;
    validate(true);
    flash(`forked ${s.title || s.name}`);
  });
  q('.e-save').addEventListener('click', () => {
    const name = (nameIn.value || sceneTitle(text.value, '')).trim();
    if (!name) { flash('give it a name', true); nameIn.focus(); return; }
    if (!text.value.trim()) { flash('nothing to save', true); return; }
    const saved = loadSaved();
    saved[name] = text.value;
    storeSaved(saved);
    refreshSaved();
    load.value = name;
    flash(`saved “${name}”`);
  });
  load.addEventListener('change', () => {
    const saved = loadSaved();
    if (!load.value || !(load.value in saved)) return;
    text.value = saved[load.value];
    nameIn.value = load.value;
    validate(true);
    flash(`loaded “${load.value}”`);
  });
  q('.e-del').addEventListener('click', () => {
    const saved = loadSaved();
    if (!load.value || !(load.value in saved)) { flash('select a saved scene first', true); return; }
    delete saved[load.value];
    storeSaved(saved);
    const n = load.value;
    refreshSaved();
    flash(`deleted “${n}”`);
  });
  q('.e-share').addEventListener('click', async () => {
    if (!text.value.trim()) { flash('nothing to share', true); return; }
    try {
      const url = await hooks.share(text.value);
      link.value = url; link.hidden = false; link.focus(); link.select();
      flash('link copied to the clipboard');
    } catch (e) { flash(`share failed: ${e.message || e}`, true); }
  });
  q('.e-compose').addEventListener('click', () => {
    const c = window.sonderComposer;
    if (c && typeof c.open === 'function') c.open({ toml: text.value, setToml: api.setToml, apply: hooks.apply });
    else flash('the AI composer (compose.js) is not loaded on this page', true);
  });
  q('.eclose').addEventListener('click', () => api.hide());

  const api = {
    el: root,
    show() { refreshForks(); refreshSaved(); root.hidden = false; if (text.value !== lastValidated) validate(true); },
    hide() { root.hidden = true; },
    toggle() { if (root.hidden) api.show(); else api.hide(); },
    get visible() { return !root.hidden; },
    setToml(toml, name) { text.value = toml || ''; if (name !== undefined) nameIn.value = name; showErrors([]); if (!root.hidden) validate(true); },
    getToml() { return text.value; },
    flash,
    showLink(url) { link.value = url; link.hidden = false; },
  };
  root.hidden = true;
  return api;
}
