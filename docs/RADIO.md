# Sonder Radio

`sonder radio` runs a scene headless (silent `Null` audio backend + the engine's audio tap)
and pipes the stereo PCM into a spawned `ffmpeg`, which encodes it to Icecast, to an HLS
directory, or to a file. ffmpeg must be on `PATH`; if it dies (server hiccup, network), it is
restarted and the music keeps going.

```text
sonder radio <scene> --out <where> [--format ogg|mp3] [--bitrate 128k]
             [--icecast-password PW] [--seconds N] [--name "..."] [--source ... -p k=v]
```

| `--out`                                      | What happens                                                                     |
|----------------------------------------------|----------------------------------------------------------------------------------|
| `icecast://source:PW@host:8000/sonder.ogg`   | Streams to an Icecast mount (`--icecast-password` fills in `source:PW@` for you) |
| `./radio/` (a directory)                     | HLS: `index.m3u8` + rolling `.ts` segments + a `radio.html` player next to them  |
| `show.ogg` / `show.mp3`                      | A file (with `--seconds` for a fixed-length capture)                             |

`--format` picks Vorbis/Ogg (default) or MP3 for Icecast and files; HLS segments are AAC
unless `--format mp3`. Everything else about the scene works as in `sonder play`
(`--source`/`--param` overrides, `--seed`).

## Icecast on a $5 VPS in ten lines

```sh
# 1. Any small Debian/Ubuntu box; open TCP 8000 in its firewall.
sudo apt install -y icecast2                          # 2. say "no" to the debconf wizard
sudo sed -i 's|<source-password>.*</source-password>|<source-password>CHANGE_ME</source-password>|' /etc/icecast2/icecast.xml
sudo sed -i 's|<hostname>.*</hostname>|<hostname>radio.example.org</hostname>|' /etc/icecast2/icecast.xml
sudo sed -i 's/ENABLE=false/ENABLE=true/' /etc/default/icecast2 2>/dev/null || true
sudo systemctl enable --now icecast2                 # 3. http://radio.example.org:8000/ shows the status page
# 4. On the same box (or any machine that can reach it), with ffmpeg + sonder installed:
sonder radio planet-tonight --out icecast://radio.example.org:8000/sonder.ogg --icecast-password CHANGE_ME
# 5. Listeners open http://radio.example.org:8000/sonder.ogg in any player or <audio> tag.
# 6. Keep it up: wrap step 4 in a systemd unit with Restart=always (template below).
```

Icecast's own listener limit (`<clients>`) defaults to 100; raise it in `icecast.xml` if you
get popular. For TLS put Caddy or nginx in front of port 8000.

### systemd unit

```ini
# /etc/systemd/system/sonder-radio.service
[Unit]
Description=Sonder Radio
After=network-online.target icecast2.service
[Service]
ExecStart=/usr/local/bin/sonder radio planet-tonight --out icecast://127.0.0.1:8000/sonder.ogg --icecast-password CHANGE_ME
Restart=always
RestartSec=5
[Install]
WantedBy=multi-user.target
```

## Without a server: HLS on any static host

```sh
sonder radio storm --out /var/www/html/radio/
```

writes `index.m3u8`, a rolling window of ~60 s of segments and `radio.html` (Safari plays HLS
natively; other browsers load hls.js from a CDN). Serve the directory with any web server.
This is also how the daily live capture in `.github/workflows/record.yml` is made, just with
`--out site/records/<date>-live.mp3 --seconds 180`.

## Notes

- Audio is the engine's real-time tap at the backend sample rate (48 kHz) as 32-bit float
  stereo; ffmpeg does the encoding, so CPU use is ffmpeg's plus the synth's (a few percent).
- The engine runs at wall-clock pace even with no sound device, so a `--seconds 180` capture
  takes three minutes.
- `sonder radio` exits with an error if ffmpeg is missing or the target is malformed (an
  `icecast://` URL without credentials or a mount). Mid-stream failures restart ffmpeg after
  2 s (5 s if it died right after starting, which usually means the server refused it).
