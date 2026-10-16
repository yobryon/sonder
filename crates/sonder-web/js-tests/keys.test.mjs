// Play-along: computer-keyboard piano layout, Web MIDI parsing, snapshot queries (keys.js).
import { test } from 'node:test';
import assert from 'node:assert/strict';

const {
  KEY_OFFSETS, STRIP_KEYS, keyOffset, keyToMidi, lowC, clampOctave, humanNote, isBlack, codesFor, codeLabel,
  parseMidiMessage, midiToCommand, humanLayers, lastHumanNote, stripStatus, initKeys,
} = await import('../static/keys.js');

test('DAW layout: two octaves by key code, C4 = 60 at the default octave', () => {
  assert.equal(keyOffset('KeyZ'), 0);
  assert.equal(keyOffset('KeyS'), 1);
  assert.equal(keyOffset('KeyM'), 11);
  assert.equal(keyOffset('Comma'), 12);
  assert.equal(keyOffset('KeyQ'), 12, 'the top row starts where the bottom row ends');
  assert.equal(keyOffset('Digit2'), 13);
  assert.equal(keyOffset('KeyU'), 23);
  assert.equal(keyOffset('KeyI'), 24);
  assert.equal(keyOffset('KeyA'), null, 'a sits between keys');
  assert.equal(keyOffset('Digit1'), null);
  assert.equal(keyOffset('toString'), null, 'prototype names are not keys');
  assert.equal(keyToMidi('KeyZ'), 60);
  assert.equal(keyToMidi('KeyQ'), 72);
  assert.equal(keyToMidi('KeyI'), 84);
  assert.equal(keyToMidi('KeyZ', 2), 36);
  assert.equal(keyToMidi('KeyI', 8), 127, 'clamped');
  assert.equal(keyToMidi('Space'), null);
  assert.equal(lowC(4), 60);
  assert.equal(clampOctave(-3), 0);
  assert.equal(clampOctave(12), 8);
  // Every offset 0..24 is reachable, and the layout is one-to-one except for the shared C.
  const offsets = Object.values(KEY_OFFSETS);
  for (let o = 0; o <= 24; o++) assert.ok(offsets.includes(o), `offset ${o}`);
  assert.equal(offsets.length, STRIP_KEYS + 1);
  assert.deepEqual(codesFor(72).sort(), ['Comma', 'KeyQ']);
  assert.deepEqual(codesFor(61), ['KeyS']);
  assert.equal(codeLabel('KeyZ'), 'z');
  assert.equal(codeLabel('Digit2'), '2');
  assert.equal(codeLabel('Comma'), ',');
  assert.ok(isBlack(61) && !isBlack(60) && isBlack(70) && !isBlack(71));
});

test('human_note commands: velocity 0 on release', () => {
  assert.deepEqual(humanNote(60, 0.75, true), { cmd: 'human_note', key: 60, velocity: 0.75, on: true });
  assert.deepEqual(humanNote(60, 0.75, false), { cmd: 'human_note', key: 60, velocity: 0, on: false });
});

test('Web MIDI bytes decode like the native human.midi adapter', () => {
  assert.deepEqual(parseMidiMessage(new Uint8Array([0x90, 60, 127])), { type: 'note_on', channel: 1, key: 60, velocity: 1 });
  assert.deepEqual(parseMidiMessage([0x91, 60, 0]), { type: 'note_off', channel: 2, key: 60 }, 'velocity 0 is off');
  assert.deepEqual(parseMidiMessage([0x80, 60, 64]), { type: 'note_off', channel: 1, key: 60 });
  assert.deepEqual(parseMidiMessage([0xB3, 64, 127]), { type: 'cc', channel: 4, number: 64, value: 1 });
  assert.deepEqual(parseMidiMessage([0xD0, 127]), { type: 'pressure', channel: 1, value: 1 });
  assert.equal(parseMidiMessage([0xF8]), null, 'clock');
  assert.equal(parseMidiMessage([0xE0, 0, 64]), null, 'pitch bend ignored');
  assert.equal(parseMidiMessage([0x90, 60]), null, 'truncated');
  assert.equal(parseMidiMessage([]), null);
  assert.equal(parseMidiMessage(null), null);
  assert.deepEqual(midiToCommand(parseMidiMessage([0x90, 64, 64])), { cmd: 'human_note', key: 64, velocity: 64 / 127, on: true });
  assert.deepEqual(midiToCommand(parseMidiMessage([0x80, 64, 0])), { cmd: 'human_note', key: 64, velocity: 0, on: false });
  assert.equal(midiToCommand(parseMidiMessage([0xB0, 1, 50])), null, 'cc has no HumanNote form');
  assert.equal(midiToCommand(null), null);
});

test('snapshot queries: human layers, last human note, strip status', () => {
  const snap = {
    layers: [{ id: 0, name: 'you', role: 'human' }, { id: 1, name: 'duet', role: 'lead' }],
    notes: [
      { at: 1, layer: 0, midi: 61, reason: 'human' },
      { at: 2, layer: 1, midi: 69, reason: 'answer' },
      { at: 3, layer: 0, midi: 66, reason: 'human' },
    ],
  };
  assert.deepEqual(humanLayers(snap).map((l) => l.name), ['you']);
  assert.deepEqual(humanLayers(null), []);
  assert.deepEqual(humanLayers({ layers: [{ role: 'pad' }] }), []);
  assert.equal(lastHumanNote(snap).midi, 66);
  assert.equal(lastHumanNote({ notes: [] }), null);
  assert.equal(stripStatus(snap, 2), '→ F#4 · 2 held');
  assert.equal(stripStatus(snap, 0), '→ F#4');
  assert.equal(stripStatus({}, 0), '');
});

test('initKeys needs a DOM and returns null without one', () => {
  assert.equal(initKeys({ root: null, send: () => {}, getSnap: () => null }), null);
  assert.equal(initKeys({ root: {}, send: () => {}, getSnap: () => null, win: null, doc: null }), null);
});
