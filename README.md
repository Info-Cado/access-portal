# access-portal

A self-hosted remote access portal, in Rust. It's the login + management layer
you put in front of a **Windows RDP** host so that each person signs in, sees
only the apps they're allowed to use, and opens them in a browser.

The portal is a single program. It contains its own browser RDP client, so
nothing else (no Guacamole, no Docker) has to be installed alongside it.

It does **not** include anything that unlocks extra sessions on Windows
Home/Pro. Running several users against one Windows machine needs the right
Windows licensing (see the end of this file).

## What it does

- **Login** with Argon2-hashed passwords.
- **Two-step verification** (TOTP) per user, optional.
- **Lockout** after too many failed attempts.
- **Per-user app permissions** — admins pick which apps each user can open.
- **Browser launch** — the user opens their app without ever knowing the Windows
  password; it stays on the server.
- **Straight in** — a user with exactly one app lands in it right after login.
- **Efficient streaming** — only changed screen areas are sent, losslessly, over
  a direct peer-to-peer link when one can be made.
- **Clipboard** — text copies and pastes in both directions.
- **Files** — each user has a transfer folder. Dropping files on the page (or
  using the Files panel) uploads into it; inside the session it is the drive
  `\\tsclient\P`; anything saved there can be downloaded from the Files panel.

## How the pieces fit

```
browser ──HTTPS + WebSocket──> access-portal (login, 2FA, permissions, RDP client)
                                     │ RDP with NLA
                                     ▼
                               Windows RDP host (your apps)
```

The portal is the RDP client (`src/rdp.rs`, built on IronRDP). It signs in to the
host with the stored Windows credentials, decodes the screen, and streams what
changed to the browser; mouse and keyboard travel back the same way. The page is
`src/session.html`.

### Screen updates (`src/tiles.rs`)

The screen is a grid of 256-pixel tiles. Only the part of a tile that really
differs from what the browser already shows is sent:

- text and UI (up to 256 colours) as palette PNG: lossless and very small;
- photo-like areas as JPEG first and a lossless PNG once things go quiet, unless
  the lossless tile is nearly as small, in which case it is sent straight away.

An idle screen sends nothing. Updates go in small batches, and the browser
acknowledges each one; the portal never has more than two on the way. On a slow
link, changes therefore wait as flags instead of queued data, and the newest
pixels are what gets sent next.

The session is 1920x1080 by default and scaled to fit the browser window. Set
`SESSION_SIZE=auto` to make the session follow the window size instead.

### Motion hybrid (`src/video.rs`)

Still areas go out as crisp tiles; an area that keeps changing — a scrolling
report, a dragged window — switches to an H.264 stream, which is far cheaper for
continuous motion than resending images. When the motion stops, a final crisp
tile is sent and the area returns to tiles, so text stays sharp and an idle
screen still costs nothing.

The screen is a grid of 512-pixel blocks (aligned to the tile grid). A block that
stays busy for a few frames gets its own H.264 encoder (`openh264`) and is sent as
a keyframe followed by delta frames, at up to ~33 fps; a block that goes quiet is
torn down. Each packet carries the block, a generation (so a stale packet from a
block's previous run is ignored), and a sequence number (the unordered channel is
reassembled in order). The browser decodes each block with its own WebCodecs
`VideoDecoder` and draws it; a browser without WebCodecs H.264 simply gets the
tile stream. Set `VIDEO=off` to disable it.

### Peer-to-peer link (`src/transport.rs`)

The session page first tries a direct WebRTC data channel to the portal (UDP,
peer to peer). Signalling is one HTTPS request to `/rtc/<app>`. If no direct path
comes up within a few seconds it falls back to a WebSocket on `/ws/<app>`,
carrying exactly the same messages. Hovering the Files button shows which one is
in use.

The direct channel is **unordered**, so a lost packet is resent without holding
up the tiles behind it — the picture keeps moving on a lossy link instead of
freezing. Tiles carry a batch sequence number, and the browser tracks the newest
batch drawn into each 256-pixel cell, so a tile that arrives late (after a newer
update to the same area) is skipped rather than overwriting fresher pixels. Up to
`MAX_IN_FLIGHT` batches are sent before waiting for the browser to acknowledge
them, which keeps a high-latency link busy without letting data pile up.

Screen changes are sent the instant the host finishes drawing them (no batching
timer), so the only delay the portal adds to a redraw is its own encoding, under
a millisecond.

- `ICE_SERVERS` lists the STUN/TURN servers used to find a path (default: a
  public STUN server). STUN alone is enough when at least one side can be reached
  directly. Behind two strict NATs a direct path is impossible and the session
  needs a **TURN relay**: run one (e.g. coturn) on a host with a public address
  and point `ICE_SERVERS`, `ICE_USERNAME` and `ICE_CREDENTIAL` at it. Without a
  TURN server those clients fall back to the WebSocket.
- `P2P=off` disables the direct link.
- The web page and signalling still need to be reachable by the user; only the
  session traffic goes direct.

Channels used on the RDP connection:

| Channel | Purpose |
|---|---|
| display control | resize the session with the browser window (only with `SESSION_SIZE=auto`) |
| clipboard | text, both directions |
| drive redirection | the per-user transfer folder, as `\\tsclient\P` (Windows portal hosts only) |
| audio | joined silently; Windows only starts drive redirection when it is present |

Not supported yet: sound, printing, copying files through the clipboard, and
clipboard content other than text.

## Prerequisites

- **Rust** (stable). Install from https://rustup.rs.
- A **Windows RDP** host reachable from the portal, with NLA enabled, the apps
  installed and a Windows account per user.

## Run it

```sh
# 1. Configure
cp .env.example .env
#   - set PORTAL_SECRET to a long random string

# 2. Load env vars, then run
#    bash:
set -a; . ./.env; set +a
cargo run

#    PowerShell:
#    Get-Content .env | ForEach-Object { if ($_ -match '^\s*([^#=]+)=(.*)$') { Set-Item "env:$($matches[1].Trim())" $matches[2] } }
#    cargo run
```

On first run it creates `portal.db`, seeds an admin (`SEED_ADMIN_USER` /
`SEED_ADMIN_PASS`), and a sample "Desktop" app pointing at `127.0.0.1:3389`.
Open http://127.0.0.1:8088, sign in, go to **Admin**, and change the admin's
details.

## Adding an app

In **Admin → Add app**, enter a name, a URL key, the RDP host's address
(`127.0.0.1` when the portal runs on the RDP host itself) and port (`3389`).
Give each portal user their Windows login under **Admin → Users → Save Windows
login**, then tick the app for them.

An app is an RDP connection to a host as the user's Windows account. To make an
account open one program instead of a desktop, set that up on the Windows side
(for example by replacing the account's shell with a launcher).

## Files

`FILES_DIR` (default `./files`) holds one folder per portal user. That folder is

- what the Files panel in the session page lists, uploads to and downloads from;
- what the session sees as `\\tsclient\P` (also under Network → tsclient).

The drive is named with a single letter because Windows currently shows only
the first character of longer names sent by the RDP library.

Uploads are limited by `MAX_UPLOAD_MB` (default 2048). Existing files are never
overwritten; a second `report.pdf` becomes `report (1).pdf`.

## Security notes (read before exposing it)

- **Only expose HTTPS (443).** Put the portal behind a reverse proxy with TLS and
  set `COOKIE_SECURE=true`. The proxy must pass WebSocket upgrades through.
- **Block RDP (3389) at the firewall** except from the portal host, so nobody
  bypasses the portal and connects to Windows directly.
- Windows passwords are encrypted at rest with `PORTAL_SECRET`. Keep it secret and
  back it up — losing it means re-entering every Windows password.
- The host's RDP certificate is not verified (RDP hosts use self-signed
  certificates); keep the portal and the host on a trusted network or the same
  machine.
- Files in `FILES_DIR` are readable by whoever can read that folder on the portal
  host. Each portal user can only reach their own folder through the portal.
- This is an MVP. Before production: add CSRF tokens on state-changing forms, rate
  limiting at the proxy, audit logging, and session cleanup for expired rows.

## Licensing reminder

Running multiple users against Windows requires the right Windows licensing
(Windows Server + RDS CALs). This portal assumes you have that. It is not a way to
get around per-user Windows licensing.

## Layout

| File | Purpose |
|---|---|
| `src/main.rs` | startup, DB pool, migrations, server |
| `src/config.rs` | env configuration |
| `src/models.rs` | `User`, `App` |
| `src/db.rs` | all SQL |
| `src/auth.rs` | hashing, tokens, at-rest crypto, TOTP |
| `src/rdp.rs` | built-in RDP client: connect, sign in, input, clipboard, resize, shared drive |
| `src/tiles.rs` | turns still screen changes into small tile batches |
| `src/video.rs` | H.264 motion blocks for areas that keep changing |
| `src/transport.rs` | link to the browser: WebRTC data channel, or WebSocket fallback |
| `src/session.html` | the browser page for a session |
| `src/web.rs` | router, auth extractors, pages, handlers, file transfer endpoints |
| `migrations/` | SQLite schema |
