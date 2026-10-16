# Shared rooms — listen together

A **host** (the native engine) publishes its performance; a tiny **relay** fans it out; **guests**
(browsers) join. Everyone hears the same thing because guests run the *same scene with the
same seed* and are fed the *host's samples* — the relay carries data, not audio, so a room
costs a few kB/s per listener and the guest's own CPU renders the sound.

```
sonder play --room wss://relay/r/<id>  ──▶  sonder-relay  ──▶  browser ?room=wss://relay/r/<id>
   (host: scene, seed, samples,                 (pub/sub,          (worker mode: wasm engine,
    light snapshots, chat)                       presence, chat)     same scene + seed, fed by
                                                                      the host's samples)
```

A browser that opens the host's **own** web UI (`sonder play --web 7777`) is already in a room:
that server streams snapshots and audio. There the room panel mirrors `snapshot.room`
(listeners, chat) and chat goes up the existing `/ws` as `{"cmd":"chat","from":…,"text":…}`.

## Run the relay

```
cargo run --release -p sonder-relay -- --bind 0.0.0.0:8788
```

No engine dependencies, one binary, no state on disk. Routes:

| | |
|---|---|
| `WS /r/<room>/host` | one host at a time; a second host replaces the first (the old one is told) |
| `WS /r/<room>/join` | guests (256 per room) |
| `GET /r/<room>` | `{"id","present","listeners","title","name","public","since","host_time"}` |
| `GET /` | plain-text index of public rooms |

Room ids are `[A-Za-z0-9_-]{1,64}` — pick anything; the first host creates the room. A room
without a host is removed 60 s later (guests get `{"type":"closed"}`); a host that comes back
within that minute keeps its guests.

### Docker

```
docker build -f crates/sonder-relay/Dockerfile -t sonder-relay .
docker run -d --restart unless-stopped -p 8788:8788 sonder-relay
```

### A $5 VPS with Caddy (wss)

Browsers on an `https://` site need `wss://`. Point a DNS name at the box and let Caddy do TLS:

```
# /etc/caddy/Caddyfile
relay.example.com {
    reverse_proxy 127.0.0.1:8788
}
```

Caddy proxies WebSockets out of the box. Run the relay as a service:

```
# /etc/systemd/system/sonder-relay.service
[Service]
ExecStart=/usr/local/bin/sonder-relay --bind 127.0.0.1:8788
Restart=always
[Install]
WantedBy=multi-user.target
```

### Fly.io

```
fly launch --no-deploy --name my-sonder-relay --dockerfile crates/sonder-relay/Dockerfile
fly deploy
```

Set `internal_port = 8788` in `fly.toml`; Fly terminates TLS, so the room URL is
`wss://my-sonder-relay.fly.dev/r/<id>`. The relay is stateless, so one small machine is enough
and a restart only costs guests a reconnect.

### Cloudflare Worker (future)

The protocol is deliberately simple — JSON text frames, one host, broadcast to guests — so it
maps onto a Durable Object per room. Not implemented yet.

## Host

```
sonder play night-freeway --room wss://relay.example.com/r/friday --room-name bryon --room-public
sonder serve quakes --room ws://127.0.0.1:8788/r/test --no-audio          # local relay
sonder radio night-freeway --out icecast://… --room wss://…/r/radio       # a stream with a room
```

- `--room <url>`: `…/r/<id>` (the engine adds `/host`). Reconnects with backoff (1–30 s)
  when the relay drops; samples that arrive while offline are dropped, never queued.
- `--room-name`: your name in the chat (default `$USER`). `--room-public`: listed on `GET /`.
- `--moments <dir>`: keep the last 10 minutes of data in memory; `/moment` in the chat (from
  anyone) writes `<dir>/<timestamp>.sonder.gz` — play it with `sonder replay <file>` or drop it
  on the web UI.

What the host sends: `hello` (scene TOML, seed, `epoch_unix`, name, title, public) on connect
and after every hot reload (`LoadScene`, `--watch`, the DJ); `samples` batches every 100 ms
(`{"t":engine_secs,"s":source,"d":Datum}`; audio datums are dropped); a light `snapshot`
(time, captions, form) at 2 Hz for clients without an engine; `chat` said in its UIs.
What it receives: guests' `chat` and `presence`; both land in `EngineSnapshot.room` (role
`host`, listeners, 32-line chat ring) and chat lines become captions (`name: text`, importance
0.6) in every UI — the TUI, the web UI, the desktop app.

## Guest

Open the static site with `?room=wss://relay.example.com/r/friday` (worker mode; the **join**
button prompts for the URL). The page connects to `/join`, gets the host's `hello`, starts the
wasm engine with that scene TOML and seed (`feed: 'room'` — no browser adapters, no built-in
sims), and pushes every relayed sample into it as it arrives. A `/ping` every 5 s measures the
round trip; the panel shows the estimated offset to the host's engine clock (`host ≈ guest +
offset`, lowest-rtt sample of the last five). Chat lines show in the panel and as captions.
**leave** returns to a normal page.

Note on sameness: the guest's engine starts at time 0 when it joins while the host is
minutes in, so form sections and chord changes line up with the host's *data*, not its
clock — the same world, the same instruments, the same rules, not a bit-identical mix.

## Protocol (JSON text frames)

| direction | frame |
|---|---|
| host → relay | `{"type":"hello","scene_toml":…,"title":…,"seed":u64,"epoch_unix":f64,"engine_time":f64,"name":…,"public":bool}` |
| host → relay | `{"type":"samples","items":[{"t":f64,"s":"source","d":Datum}]}` (~10/s) |
| host → relay | `{"type":"snapshot","time":f64,"scene":…,"captions":[…],"form":{…}}` (2/s) |
| anyone → relay | `{"type":"chat","from":…,"text":…}` (240 chars, one per second per connection; extra ones get `{"type":"error",…}`) |
| guest → relay | `{"type":"ping","sent":ms}` → `{"type":"pong","sent":ms,"relay_ms":ms,"host_time":f64,"host_age_ms":ms,"present":bool}` |
| relay → guests | the latest `hello` on join, every host frame, `{"type":"presence","listeners":n}` on change, `{"type":"chat","at":f64,"from":…,"text":…}`, `{"type":"closed"}` |
| relay → host | `presence`, guests' `chat`, `{"type":"error","error":"replaced by another host"}` |

Buffers are bounded: 512 frames per guest, oldest dropped for a slow one; 8192 samples queued
in the host before the newest are dropped; sample frames up to 4 MiB, guest frames 4 KiB.

## Code map

- `crates/sonder-relay` — the relay (axum); integration test with a fake host and two guests.
- `crates/sonder-engine/src/room.rs` — `RoomConfig`, `RoomLink` (sample sink, hello, chat
  in/out) and the publisher task; a mock-relay test. `moments.rs` — the rolling buffer.
- `crates/sonder-engine/src/control.rs` — `set_room`, `chat_received`, `/moment` requests,
  `EngineSnapshot.room`.
- `crates/sonder-web/static/rooms.js` — guest client, offset math, the room panel (both
  modes); `js-tests/rooms.test.mjs`.
