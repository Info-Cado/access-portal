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

### The screen: Windows' own H.264 (`src/gfx.rs`)

Sessions use Windows' graphics pipeline (MS-RDPEGFX). Windows encodes the
screen as H.264 on the host's graphics card (NVIDIA, Intel or AMD encoder) at up
to 60 frames a second, and the portal passes it to the browser untouched; the
browser decodes it with its own hardware decoder (WebCodecs). The portal only
decodes the few lossless updates Windows sends (ClearCodec, planar) and passes
fills, copies and cache commands through, so each browser applies exactly what
Windows drew, in order.

- Windows on this kind of host sends H.264 as AVC444: two 4:2:0 pictures per
  frame in one stream. The browser decodes both and draws the main view, a full
  4:2:0 picture of the screen.
- Each picture is drawn once, clipped to the areas Windows says changed.
- Frames are acknowledged to Windows only after the browser drew them, so a slow
  link gets fewer frames rather than a growing delay.
- The session loop serves the browser before the host, so input and
  acknowledgements never queue behind screen data.

Measured on the development laptop (RTX 3050 Ti, 1920x1080, dragging a window):
50-57 frames a second, input to next frame from Windows about 11 ms, the browser
decoding a picture in about 1 ms and drawing a frame in 2-3 ms, about 3-4 Mbit/s.

On start the app sets the Windows side up for this: H.264 encoding and session
rendering on the graphics card, and 60 frames a second (`tune_remote_desktop` in
`src/host.rs`). Sessions already open keep their old setup until their user
signs out, and the frame rate applies after a reboot.

Browsers that cannot decode H.264 (`?vd=2` is not sent) get the older path
below, and `EGFX=off` turns the pipeline off.

### Screen updates without H.264 (`src/tiles.rs`)

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
| display control | the session's size (Fit to window, Quality) and a second monitor |
| clipboard | text, pictures and files, both directions |
| drive redirection | the per-user transfer folder, as `\\tsclient\P` (Windows portal hosts only) |
| printer redirection | "Portal printer" in every print dialog (`src/print.rs`) |
| audio | the session's sound, AAC when the browser can decode it (`src/sound.rs`) |
| audio input, camera | the user's microphone and camera for apps in the session (`src/devices.rs`); Windows Pro/Enterprise hosts only |

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
**administrator** (it changes Windows accounts and Remote Desktop settings).

**Setting up a new computer:** open **Settings → This computer** and click
**Fix everything** (`src/host_setup.ps1`). It checks and, where needed, sets up:

- Remote Desktop on, with network-level sign-in;
- RDP Wrapper, so several people can be connected at once on Windows Home or
  Pro (the official v1.6.2 release, checked against its known checksum, with a
  Microsoft Defender exclusion for its folder) and its settings for this
  Windows build, fetched from the portal's update service first and then from
  community sources; after a Windows update breaks it, the same button fixes it;
- the "Portal RDP Users" group and its permission to sign in over Remote Desktop;
- the Miracle launcher (it finds `miracle.exe`);
- Windows' PDF printer driver;
- the Remote Desktop settings the portal is tuned for.

Fixing RDP Wrapper restarts Remote Desktop (connected users drop for a moment;
it asks first). A banner in the app points here while anything is missing.

**Releasing an update:** `.\scripts\release.ps1 -Version 0.2.1 -Notes "..."
-TokenFile <Cloudflare token>` builds, signs (with
`admin-app/src-tauri/updater.key`, not committed: keep a copy somewhere safe,
installs only accept updates signed with it) and publishes to the update
service. Installed copies check every 6 hours and install when nobody is
connected (Settings → Updates; `"Access Portal.exe" --update` installs at once).

## Where things live

Everything the portal keeps is in `C:\ProgramData\AccessPortal`, which only
administrators can open (it holds the key to the stored Windows passwords):

| | |
|---|---|
| `portal.env` | settings, created on first start with a fresh `PORTAL_SECRET` (the variables are listed in `.env.example`) |
| `portal.db` | users, apps, permissions, access rules, sign-in sessions, app settings, the activity log |
| `files\` | one transfer folder per user (`files\.clipboard\` holds files being pasted into sessions) |
| `portal.log` | the server's log |
| `backups\` | daily backups (database, settings, public link details) |
| `install-id`, `tunnel.json` | this install's public link (keep private: the tunnel token is in it) |

Restart the app (tray icon → Quit, then open it) after changing `portal.env`.

## Using it

The app asks for its own sign-in when it opens, after it is closed to the tray,
after 15 minutes without use, and when **Lock** is clicked. It starts as
**admin / admin**; a banner asks you to change it under **Settings → App
sign-in**. The portal keeps serving users while the app is locked. (Forgot it?
Delete the `admin_username` and `admin_password` rows of the `settings` table in
`portal.db` to go back to admin / admin.)

The window has four screens:

- **Users** — create a user (portal username and password, the Windows account
  their sessions run as, the apps they may open, optional two-step
  verification). Click a user to change any of that, unlock them after too many
  wrong passwords, or delete them. **Message everyone** shows a message on top
  of every open session.
- **Apps** — an app is an RDP connection (host and port, `127.0.0.1:3389` for
  this machine) opened as the user's Windows account; its key is used in the
  link. Add or delete apps here.
- **Activity** — sign-ins (and failed ones), sessions opened and closed (with
  why), print jobs, uploads and downloads, clipboard file transfers, and every
  admin action, newest first; filter by user, search, **Export CSV** (saved to
  Downloads). Kept for 180 days (`src/activity.rs`).
- **Settings**:
  - *This computer* — what the computer needs, with one-click fixes (above);
  - *Sessions* — disconnect idle sessions after a time without keyboard or
    mouse input (default 1 hour; a one-minute warning first; apps keep
    running); sign out Windows sessions left disconnected (default never;
    apps close; Windows administrator accounts never); ask before viewing a
    user's screen (default on);
  - *Phone alerts* — a notification (ntfy app, free) when the computer goes
    offline, the public link is down for 10 minutes, sessions keep failing to
    start, Remote Desktop stops accepting connections or the disk is almost
    full, and again when it's fixed. The app checks every minute; the
    "offline" alert comes from the update service when the app's heartbeats
    stop for 15 minutes. Default server `ntfy.envs.net` (ntfy.sh limits
    messages from Cloudflare's shared addresses);
  - *Backups* — made daily and kept 14 (configurable), optionally copied to a
    second folder (another drive, OneDrive…); **Back up now**; **Restore** puts
    one back and restarts the app (what was there is saved first). Users'
    files aren't included;
  - *Updates* — the version, **Check now**, and automatic installation;
  - *App sign-in*.

Each user also has a **Full desktop** switch (`src/host.rs`). It changes the
user's Windows account on this machine: off, the account's shell is the Miracle
launcher and Run, the command prompt, PowerShell, Task Manager, the registry
editor, Control Panel and drive C: are blocked; on, exactly those settings are
removed and sessions get the normal desktop. Windows administrator accounts are
never limited. A change applies from the user's next session. New users get a
Windows account of their own by default (a standard account in the "Portal RDP
Users" group); deleting a user leaves the Windows account unless you tick the
box.

**When and where they can sign in** (per user, `src/access.rs`): days of the
week, a time window (this computer's clock; 22:00–06:00 works), network
addresses or ranges (`203.0.113.0/24`) and countries (two-letter codes, from
Cloudflare). Outside them, sign-in is refused with the reason, and an open
session ends when its hours run out (its apps keep running). Someone signing in
on this computer itself is never refused for their address or country.

**Connected now** in a user's panel lists their open sessions (app, device,
address, how long). For each one:

- **View screen** / **Take control** opens Windows' own viewer (Remote Desktop
  "shadowing") on this computer's screen; close its window to stop. The user is
  told on their screen, and with *Ask before viewing* on, Windows asks them to
  allow it first.
- **Message** shows a message on top of that session until they click OK.
- **Disconnect** ends that browser session. The user's Windows session and apps
  keep running, and the page tells them their administrator disconnected them
  (it does not reconnect by itself).
- **Sign out of Windows** (for the user as a whole) disconnects every session,
  ends their portal sign-in and signs their Windows account out, which closes
  their apps. Unsaved work is lost, so it asks first.

## The session page

The buttons at the top right of a session:

- **Files** — the transfer folder (upload, download). The dot shows the link
  state.
- **Full screen**.
- **Connection** — round-trip time (green under 80 ms, amber under 200 ms,
  red above or no reply); hover for frames per second, data rate and link type.
- **Menu** — *Disconnect* (leave the apps running and come back later),
  *Sign out of Windows* (close the apps and end the Windows session),
  *Sound*, *Printing* (print dialog straight away, or a notice), *Fit to
  window*, *Second screen*, *Quality*, *Microphone*, *Camera*, *Change
  password* and *Sign out of the portal*.

**Sound.** The session's sound plays in the browser: AAC (about 0.1 Mbit/s)
when the browser can decode it, else plain PCM (1.4 Mbit/s), and only while
something plays. Browsers start audio after the first click or key press.

**Size and quality.** By default the session is 1920×1080 and the browser
scales it. *Fit to window* (on by default on phones and tablets) makes the
Windows session exactly the size of the browser window, following it when it
changes. *Quality*: *Auto* lowers the resolution by a third after 10 seconds
of a slow link (over 200 ms, or no replies) and restores it after a minute of a
good one; *Sharp* never lowers it; *Fast* always does.

**Second screen.** Opens a second window showing the session's second monitor
(the session gets two monitors side by side). In Chrome and Edge it opens on
the other screen once the browser's window-management permission is allowed;
its *Full screen* button fills that screen.

**Phones and tablets.** The screen works as a trackpad: slide to move the
pointer, tap to click, tap twice to double-click, tap then hold and slide to
drag, hold still for a right-click, two fingers to scroll (up, down and
sideways), pinch to zoom, two-finger tap to right-click. *Direct* mode (in the
key bar) clicks where the finger is. The key bar has the keyboard, Esc, Tab,
arrows, Ctrl/Alt/Shift and F1–F12.

**Microphone and camera.** When an app in the session starts recording or opens
the camera, the browser asks the user once and then sends its microphone (as
the PCM format Windows asked for) or camera (JPEG, about 15 frames a second); a
red marker in the toolbar shows it. Windows Home doesn't allow either in
Remote Desktop sessions (its licence turns them off in services RDP Wrapper
doesn't touch), so this needs a Windows Pro or Enterprise host; the app's
*This computer* list says which.

**Your password.** Users change their password from the menu (or `/account`).
Someone who forgot it can set a new one at *Forgot your password?* on the
sign-in page with a code from their authenticator app (two-step verification
must be on); otherwise the administrator sets one.

**Copy and paste.** Text, pictures and files go both ways:

- Copy on your computer, then Ctrl+V in the session: text is pasted directly; a
  picture (a screenshot, "Copy image") or files copied in Explorer are uploaded
  first and then pasted where the cursor is (files into a folder, pictures into
  Paint, Word, a chat …).
- Copy text or a picture in the session and it is on your computer's clipboard
  too. Copy files in the session and the page offers **Save to this computer**:
  they are copied into your Files folder and downloaded.

**Printing.** Every session has a printer called **Portal printer**. Print to it
from any app and the job is saved as a PDF in the user's transfer folder
(`Printed <date time>.pdf`) and opens straight in the browser's print dialog,
to print on the user's own printer (turn that off in the menu to get a notice
with **Open to print** and **Download** instead). The host needs Windows'
built-in "Microsoft Print To PDF" driver, which Windows 11 has.

**Full screen** sends the Windows key, Alt+Tab and other shortcuts to the
session (Chrome and Edge); hold Esc to leave. Ctrl+Alt+Del always stays on the
user's computer.

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
- The admin panel asks for its own sign-in; change the default (admin / admin).
  A Windows administrator on the host can still read the database, so lock the
  host too.
- This is an MVP. Before production: add CSRF tokens on state-changing forms and
  rate limiting at the proxy.

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
| `src/gfx.rs` | Windows' graphics pipeline, passed through to the browser |
| `src/video.rs` | H.264 motion blocks for the older path |
| `src/transport.rs` | link to the browser: WebRTC data channel, or WebSocket fallback |
| `src/session.html` | the browser page for a session |
| `src/web.rs` | router, auth extractors, pages, handlers, file transfer endpoints |
| `src/api.rs` | admin API, called in-process by the app |
| `src/host.rs` | Windows accounts: create/delete, Full desktop / Miracle-only switch, sign-out; Windows sessions (list, sign out, view); host tuning |
| `src/presence.rs` | who is connected now; disconnecting a session or messaging it from the app |
| `src/print.rs` | the "Portal printer": print jobs saved as PDFs in the user's folder |
| `src/sound.rs` | the session's sound, passed to the browser |
| `src/clipimage.rs` | copied pictures: Windows bitmaps to PNG and back |
| `src/access.rs` | when and from where each user may sign in |
| `src/activity.rs` | the activity log |
| `src/settings.rs` | settings changed from the app (timeouts, screen viewing, the app's sign-in) |
| `src/clock.rs` | this computer's local time |
| `src/backup.rs` | daily backups and restoring one |
| `src/devices.rs` | the user's microphone and camera (MS-RDPEAI, MS-RDPECAM) |
| `src/host_setup.ps1` | checks and sets up a computer for the portal |
| `scripts/release.ps1` | builds, signs and publishes an update |
| `provision/` | the sign-up service (Cloudflare Worker) and its deploy script |
| `admin-app/` | the Access Portal app (Tauri): `src-tauri/` starts the portal, tray icon; `src/` the window's page (React + shadcn/ui) |
| `migrations/` | SQLite schema |
