// Sonder — the DJ pill in the header: which rule chose the scene, why, and how long it
// holds, with an on/off switch. Pure helpers are exported for node tests; `initDj` wires
// the DOM. Shown only when the engine runs a DJ (`EngineSnapshot.dj` present).

/** `m:ss` from seconds. */
export const mmss = (s) => {
  const t = Math.max(0, Math.floor(Number(s) || 0));
  return `${Math.floor(t / 60)}:${String(t % 60).padStart(2, '0')}`;
};

/** The pill's text for a `DjSnapshot` (null/undefined → ''). */
export function djText(dj) {
  if (!dj) return '';
  if (!dj.enabled) return dj.reason && dj.reason !== 'off' ? `DJ off · ${dj.reason}` : 'DJ off';
  const parts = [dj.rule ? `DJ · ${dj.rule}` : 'DJ · watching'];
  if (dj.reason && dj.reason !== 'watching') parts.push(dj.reason);
  if (dj.candidate) parts.push(`next: ${dj.candidate}`);
  parts.push(mmss(dj.hold_remaining));
  return parts.join(' · ');
}

/** The command that flips the DJ. */
export const toggleCommand = (dj) => ({ cmd: 'set_dj', enabled: !(dj && dj.enabled) });

/**
 * Wire the pill. `root` is an element (hidden until a snapshot carries `dj`);
 * `send(cmd)` talks to the engine. Returns `{ update(snapshot) }`.
 */
export function initDj({ root, send }) {
  if (!root) return { update() {} };
  let last = null;
  let lastText = '';
  const text = document.createElement('span');
  text.className = 'dj-text';
  const btn = document.createElement('button');
  btn.className = 'btn ghost dj-toggle';
  btn.title = 'let the DJ switch scenes on world events (D)';
  btn.addEventListener('click', () => send(toggleCommand(last)));
  root.replaceChildren(text, btn);
  return {
    update(snap) {
      const dj = snap && snap.dj;
      root.hidden = !dj;
      if (!dj) { last = null; return; }
      last = dj;
      const t = djText(dj);
      if (t !== lastText) { lastText = t; text.textContent = t; }
      root.classList.toggle('on', !!dj.enabled);
      const label = dj.enabled ? 'off' : 'on';
      if (btn.textContent !== label) btn.textContent = label;
    },
  };
}
