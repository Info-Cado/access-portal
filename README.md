# access-portal

A self-hosted remote access portal, in Rust. It's the login + management layer
you put in front of a **Windows RDP** host so that each person signs in, sees
only the apps they're allowed to use, and opens them in a browser.

Everything is one Windows app, **Access Portal** (`admin-app/`), installed on
the RDP host. It runs the portal server for remote users and is the admin panel.
Closing its window hides it to the notification area and the portal keeps
running; it starts with Windows. The portal contains its own browser RDP
client, so nothing else has to be installed alongside it.

It does **not** include anything that unlocks extra sessions on Windows
Home/Pro. Running several users against one Windows machine needs the right
Windows licensing (see the end of this file).

## What it does

- **Login** with Argon2-hashed passwords.
- **Two-step verification** (TOTP) per user, optional.
- **Lockout** after too many failed attempts.
- **Per-user app permissions** — the admin picks which apps each user can open.
- **Admin panel on the host only** — users, apps and permissions are managed in
  the app on the host; there is no admin page on the network.
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

- A **Windows RDP** host with NLA enabled, the apps installed and a Windows
  account per user. Access Portal is installed on that same machine.
- To build: **Rust** (stable, https://rustup.rs), **Node.js**, and the WebView2
  runtime that ships with Windows 11.

## Build and install

```sh
cd admin-app
npm install
npm run build      # installer: src-tauri/target/release/bundle/nsis/Access Portal_<version>_x64-setup.exe
npm run dev        # or run it straight from source
```

Install it on the RDP host and open it. It must run as a Windows
**administrator** for the "Full desktop" switch (it changes other Windows
accounts).

## Where things live

Everything the portal keeps is in `C:\ProgramData\AccessPortal`, which only
administrators can open (it holds the key to the stored Windows passwords):

| | |
|---|---|
| `portal.env` | settings, created on first start with a fresh `PORTAL_SECRET` (the variables are listed in `.env.example`) |
| `portal.db` | users, apps, permissions, sign-in sessions |
| `files\` | one transfer folder per user |
| `portal.log` | the server's log |
| `install-id`, `tunnel.json` | this install's public link (keep private: the tunnel token is in it) |

Restart the app (tray icon → Quit, then open it) after changing `portal.env`.

## Using it

The window has two screens:

- **Users** — create a user (portal username and password, the Windows account
  their sessions run as, the apps they may open, optional two-step
  verification). Click a user to change any of that, unlock them after too many
  wrong passwords, or delete them.
- **Apps** — an app is an RDP connection (host and port, `127.0.0.1:3389` for
  this machine) opened as the user's Windows account; its key is used in the
  link. Add or delete apps here.

Each user also has a **Full desktop** switch (`src/host.rs`). It changes the
user's Windows account on this machine: off, the account's shell is the Miracle
launcher and Run, the command prompt, PowerShell, Task Manager, the registry
editor, Control Panel and drive C: are blocked; on, exactly those settings are
removed and sessions get the normal desktop. Windows administrator accounts are
never limited. A change applies from the user's next session. The app does not
create Windows accounts.

The admin calls (`src/api.rs`) are never served on the network: the app makes
them in-process (`Portal::admin` in `src/lib.rs`).

## Public link

Every install gets its own public HTTPS address, such as
`khush-laptop.infocado.in`, shown at the top of the window with a **Copy link**
button. Remote users open it in any browser, sign in and open their app. Nothing
is needed on the host's router or firewall.

How it works:

1. On first start the app sends a random install id (kept in the data folder)
   to the sign-up service, a Cloudflare Worker in `provision/worker.js`.
2. The service creates a Cloudflare Tunnel for that install, picks a free name
   from the computer's name, points the name at the tunnel and returns the
   tunnel's token. Later starts reuse it (`tunnel.json` in the data folder).
3. The app runs Cloudflare's `cloudflared` (shipped with the app, pinned and
   checksum-checked by `admin-app/scripts/fetch-cloudflared.mjs`) with that
   token and restarts it if it stops. It only makes outgoing connections.

The page, sign-in and connection setup go through the tunnel. The screen
stream first tries a direct peer-to-peer link and only uses the tunnel when no
direct path exists. To suit Cloudflare's limits, sessions ping the browser every
20 seconds (idle connections are closed after about 100), the page reconnects
by itself if the link drops, and uploads go in 16 MB chunks (requests are
limited to 100 MB).

Setting up the sign-up service (once, for the domain the links use):

```powershell
.\provision\deploy.ps1 -TokenFile <file with a Cloudflare API token> -Zone infocado.in
```

The token needs Account → Cloudflare Tunnel → Edit, Account → Workers Scripts →
Edit, Zone → DNS → Edit and Zone → Zone → Read. The script stores it as a
secret of the Worker and writes `admin-app/src-tauri/provision.env` (not
committed) with the service's address and key; builds made after that have the
public link. A build without that file works, just without a public link.

Because the portal is public, it:

- marks sign-in cookies Secure whenever the browser came in over HTTPS;
- refuses form posts and uploads sent from other sites;
- limits failed sign-ins per network address (20 per 15 minutes) on top of the
  per-account lockout;
- clears expired sign-ins hourly.

The window's page is React with shadcn/ui components (Tailwind), built by Vite;
add more components with `npx shadcn@latest add <name>`.

## Files

`FILES_DIR` (default `files\\` in the data folder) holds one folder per portal user. That folder is

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
- Anyone who can use the host's desktop can open the admin panel; lock the host.
- This is an MVP. Before production: add CSRF tokens on state-changing forms, rate
  limiting at the proxy, audit logging, and session cleanup for expired rows.

## Licensing reminder

Running multiple users against Windows requires the right Windows licensing
(Windows Server + RDS CALs). This portal assumes you have that. It is not a way to
get around per-user Windows licensing.

## Layout

| File | Purpose |
|---|---|
| `src/lib.rs` | `start`: settings, database, migrations, server; in-process admin calls |
| `src/config.rs` | settings |
| `src/models.rs` | `User`, `App` |
| `src/db.rs` | all SQL |
| `src/auth.rs` | hashing, tokens, at-rest crypto, TOTP |
| `src/rdp.rs` | built-in RDP client: connect, sign in, input, clipboard, resize, shared drive |
| `src/tiles.rs` | turns still screen changes into small tile batches |
| `src/video.rs` | H.264 motion blocks for areas that keep changing |
| `src/transport.rs` | link to the browser: WebRTC data channel, or WebSocket fallback |
| `src/session.html` | the browser page for a session |
| `src/web.rs` | router, auth extractors, pages, handlers, file transfer endpoints |
| `src/api.rs` | admin API, called in-process by the app |
| `src/host.rs` | Full desktop / Miracle-only switch for a Windows account |
| `provision/` | the sign-up service (Cloudflare Worker) and its deploy script |
| `admin-app/` | the Access Portal app (Tauri): `src-tauri/` starts the portal, tray icon; `src/` the window's page (React + shadcn/ui) |
| `migrations/` | SQLite schema |
