# bambuspa

A web bridge for controlling Bambu Lab P1S printers from a phone browser.

Browsers can't talk to the printer directly (MQTT over TLS on :8883, a
proprietary camera stream on :6000), so bambuspa runs on a machine on the
same network, holds those connections, and serves a mobile web page.

## Features

**Printers**

- Any number of printers, each with a name you choose and can change. A
  switcher at the top of the page picks which one the dashboard, camera and
  controls are about; each chip shows whether that printer is connected and
  how far its print has got.
- Every printer stays connected whichever one is shown, so each keeps
  its camera and its timelapse list and sending its notifications. Once there is more than
  one, notifications start with the printer's name.

**Controls**

- Bed down (absolute move to Z200), Home (`G28`), Extrude and Unload
- Bed-drying and nozzle temperature sliders with material presets
- Pause, Resume and Stop for a running print (Stop asks for a second tap)
- Chamber lamp toggle

Bed, temperature and filament actions are refused server-side unless the
printer is idle (`IDLE`, `FINISH` or `FAILED`), and print controls are refused
unless they match the printer's state.

**Status and camera**

- Connection, printer state, bed/nozzle temperatures, progress, job name,
  layer and time remaining
- AMS trays (colour, material, nozzle range) and desiccant grade (A–E)
- Printer errors shown as readable messages, translated from HMS and
  `print_error` codes
- Live chamber camera at ~1 fps.
- The timelapses the printer records on its SD card are listed under the
  camera, with their thumbnails, and play in the camera view with a scrub bar.
  The printer records one when timelapse is turned on in Bambu Studio for that
  print. Once the timelapses on the card pass a size set in Settings (4 GB by
  default), the oldest are deleted until they are under 90% of it; the newest
  is always kept.

bambuspa keeps the camera connection open the whole time it runs. The
printer serves only one camera client, so Bambu Studio's camera view won't
work alongside it.

**Automation**

- Heaters turn off automatically after a delay (bed 24h, nozzle 15m by
  default)
- The chamber lamp turns on when a print starts or a heater is set, and off
  8h (by default) after the printer goes idle. The manual toggle is never
  overridden.

**Notifications**

Web push to subscribed devices: print started, finished or failed; printer
errors (including filament runout); and a repeating reminder while the bed is
hot with no print running. Each device chooses which it wants. Requires HTTPS;
on iPhone/iPad it also requires iOS 16.4+ and the page added to the Home
Screen.

**Settings and diagnostics**

- A Settings screen for the printers, notifications, how much
  space timelapses may take on the SD card (4 GB by default), auto-off delays, event log size,
  login length, theme (light/dark/system), and which status sections are shown
  and in what order. Changes apply when saved.
- Saving a printer first checks it answers, and says what is wrong if it
  doesn't (address, access code or serial).
- An Events screen listing commands sent, printer acknowledgements and
  reports, and notifications sent, each with the raw message. It is stored in
  the database (64 MB by default) so it survives restarts.
- Can be added to the iOS Home Screen as a full-screen app.

## Printer setup

Recent P1 firmware rejects third-party G-code unless **LAN Only Mode** and
**Developer Mode** are both enabled on the printer. Status and camera work
without them; the controls need them.

You will need each printer's IP address and access code (printer screen,
Settings → WLAN) and its serial number (Settings → Device). These are entered
under Printers on the Settings page, not in the environment.

## Running

From source, with [Rust](https://rustup.rs) installed (the version is pinned in
`rust-toolchain.toml` and fetched automatically). The page is a
[Dioxus](https://dioxuslabs.com) app compiled to WebAssembly and built into
the binary, so build it first with the Dioxus CLI:

```sh
rustup target add wasm32-unknown-unknown
cargo install dioxus-cli@0.7.10 --locked
(cd web && dx bundle --platform web --release --out-dir dist)
AUTH_DISABLED=true cargo run --release
```

Then open `http://<host>:8081`, go to Settings and add your printers.

Prebuilt binaries for Linux, macOS and Windows (amd64 and arm64) are attached
to each [GitHub release](https://github.com/brhelwig-dev/bambuspa/releases).

A container image is published for linux/arm64:

```sh
mkdir -p data
docker run -d -p 8081:8081 --user "$(id -u):$(id -g)" \
  -e AUTH_DISABLED=true -e DATA_DIR=/data \
  -v "$PWD/data:/data" \
  ghcr.io/brhelwig-dev/bambuspa:latest
```

The image runs as a non-root user, so the mounted directory must be writable
by the user it runs as.

| Tag | Meaning |
|---|---|
| `latest` | The newest build of `main` |
| `YY.DOY.M` | A release, e.g. `26.279.907`: year, day of year, minute of day (UTC) |
| `<commit sha>` | The build of that commit on `main` |

Every commit to `main` that passes CI is released.

## Configuration

Everything except the following is set on the Settings page and stored in the
database.

| Variable | Default | Description |
|---|---|---|
| `LISTEN_ADDR` | `:8081` | Address to listen on |
| `DATA_DIR` | `./data` | Directory for the SQLite database |
| `OIDC_ISSUER` | | OpenID Connect issuer URL, e.g. `https://id.example.com` |
| `OIDC_CLIENT_ID` | | OIDC client ID |
| `OIDC_CLIENT_SECRET` | | OIDC client secret |
| `PUBLIC_URL` | | The URL the app is reached at, e.g. `https://printer.example.com` (scheme and host only) |
| `AUTH_DISABLED` | | Set to `true` to run without a login |

**The app will not start until authentication is configured.** Either set all
four of `OIDC_ISSUER`, `OIDC_CLIENT_ID`, `OIDC_CLIENT_SECRET` and `PUBLIC_URL`,
or set `AUTH_DISABLED=true`.

### Login

Any OpenID Connect provider should work; it was developed against
[Pocket ID](https://pocket-id.org). Register a confidential client with the
redirect URI `<PUBLIC_URL>/auth/callback`. Anyone the provider lets log in to
that client gets in, so restrict access at the provider (in Pocket ID, with
the client's allowed user groups).

The issuer is checked at startup, so a wrong URL fails immediately. Cookies
are marked `Secure` when `PUBLIC_URL` is `https`. Logins last 14 days from
last use by default (configurable in Settings).

### Data

`DATA_DIR` holds `bambuspa.sqlite`: the event log, settings
(including the printer's access code), notification subscriptions and pending
auto-off timers. Put it on a persistent volume: losing it also loses the push
signing key, which unsubscribes every device. The database uses WAL mode, so
back up the `-wal` and `-shm` files along with it.

## Security

With OIDC configured, everything requires a login except `/healthz` and the
few static files a phone fetches before logging in (service worker, manifest,
icons). With
`AUTH_DISABLED=true`, anyone who can reach the port can control the printer and
watch the camera, so only run it that way on a trusted network (LAN or
tailnet).

The printer's access code is stored in the database, so treat `DATA_DIR` as
sensitive. The page is never sent the code back, only whether one is set.

## Development

```sh
cargo test --workspace --exclude bambuspa-web     # everything, against a fake printer
cargo clippy --workspace --exclude bambuspa-web --all-targets
cargo clippy -p bambuspa-web --target wasm32-unknown-unknown
cargo run --example fake_printer
```

The server embeds `web/dist/public`, so build the page (see Running) before any
of these. A debug build reads those files from disk, so after rebuilding the
page a reload shows it without restarting the server.

The workspace has three crates: the server at the root, `api` with the JSON
shapes the server and the page share, and `web`, the page.

The fake printer serves the printer's MQTT, camera and FTP ports on 127.0.0.1 with
a print under way. Run the app alongside it (`AUTH_DISABLED=true cargo run`) and
set the printer to `127.0.0.1`, serial `FAKE0001`, access code `12345678`. Its FTP
server listens on port 990, which on Linux needs
`sudo sysctl net.ipv4.ip_unprivileged_port_start=990` first.

The page gets its live data — status, the camera, the timelapse list — over one
websocket at `/api/live`; everything else is plain HTTP.

## Protocol notes

- **MQTT:** TLS on :8883, username `bblp`, password is the LAN access code,
  self-signed certificate. Status arrives on `device/<serial>/report`. After
  the initial `pushall` the printer only sends changed fields, so reports are
  merged into a cached state.
- **Camera:** TLS on :6000. The client sends an 80-byte auth packet (`0x40`,
  `0x3000`, then the username and access code, each zero-padded to 32 bytes).
  The printer then sends JPEG frames, each preceded by a 16-byte header whose
  first four bytes are the little-endian image size. Based on
  [ha-bambulab](https://github.com/greghesp/ha-bambulab)'s chamber-image
  client.
- **SD card:** FTP with implicit TLS on :990, username `bblp`, password is the
  LAN access code, and TLS on each passive data connection. Timelapses are
  `/timelapse/video_YYYY-MM-DD_HH-MM-SS.avi` (motion-JPEG), with thumbnails at
  `/timelapse/thumbnail/<same name>.jpg`. The server answers `502` to `FEAT`,
  `MLSD`, `STAT`, `SITE` and every free-space command, and accepts five logins
  at once.
