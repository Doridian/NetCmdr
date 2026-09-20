# NetCmdr

Cross-platform client for the Tripp-Lite B070-008-19-IP KVM (and possibly others).

The vendor ships a Java client. This project replaces it.

## Status

Verified against real hardware (a B070-008-19-IP on firmware **2.2.1258.1.0**).

- **Management CLI works end to end.** `netcmdr` reads and *writes* every setting
  the device has — network, users, LDAP, RADIUS, security policy, serial ports,
  SNMP, power — over the device's real `/message.pv` API, and can power-control,
  back up, restore and reboot.
- **The KVM session works end to end.** `netcmdr-view` opens a window on a target's
  screen with keyboard and mouse passed through: live video decoded from the
  device's own LZW + tile codec, verified against hardware.
- **The viewer runs in a browser.** `netcmdr-bridge` terminates the device's
  un-negotiable TLS and relays for `netcmdr-web`, which is the same viewer
  compiled to WebAssembly — the device's LZW video decoded in the browser and
  drawn by the same code the desktop window uses.

Virtual media (the session's fourth channel) is not implemented.

The device is a rebadged **Minicom Smart 116 IP** running Windows CE on an Intel
IXP425. See [`research/notes/FINDINGS.md`](research/notes/FINDINGS.md) §5.0 for
the hardware-verified summary and [`research/SOURCES.md`](research/SOURCES.md)
for provenance.

## What works today

```sh
cargo build --release
```

Needs Rust 1.95 or newer (the viewer's egui version sets the floor).

### `netcmdr` — management CLI

```sh
netcmdr --host 192.0.2.10 status     # firmware, LAN, per-port state (+ SIU detection)
netcmdr --host 192.0.2.10 config     # every setting, in one page
netcmdr --host 192.0.2.10 targets    # ports and names
netcmdr --host 192.0.2.10 users      # local accounts
```

Credentials default to the factory `admin` / `access`; override with `--user` /
`--password` or `NETCMDR_USER` / `NETCMDR_PASSWORD`. `--host` also comes from
`NETCMDR_HOST`. The device speaks **TLS 1.2** with a self-signed cert, so the
client does not verify it — treat the management network as trusted or tunnel
it. See [TLS](#tls) for why this is OpenSSL and not rustls.

#### Changing settings

Settings are grouped by area, and every area answers to `show` and `set`:

| Area | What it covers |
|---|---|
| `device` | name, session TCP port |
| `network` | IPv4 and IPv6 addressing, DNS |
| `ssh` | serial-over-SSH passthrough and its ports |
| `security` | lockout policy, password rules, idle timeout |
| `auth` | which authentication sources are tried, in what order |
| `ldap` | directory settings, plus `ldap server add`/`remove` |
| `radius` | realm and accounting, plus `radius server add`/`remove` |
| `snmp` | traps, destination, community |
| `time` | the device clock |
| `serial` | both serial ports' line settings |
| `kvm-manager` | central management appliance |
| `target` | the names of the machines on the KVM ports |
| `user` | accounts: `list`, `add`, `remove`, `set`, `password` |
| `pdu` | power strips: `list`, `models`, `add`, `remove`, `outlet` |

```sh
netcmdr network set --ipv4 static --address 192.0.2.10 --subnet 255.255.255.0 \
                 --gateway 192.0.2.1 --dns-source static --dns 192.0.2.53
netcmdr security set --max-attempts 5 --attempt-window 3m --block-for 30m
netcmdr ldap server add ldap.example.com --protocol ldaps
netcmdr ldap set --mode group --search-base dc=example,dc=com --user-attribute uid
netcmdr auth set --order local,ldap
netcmdr user add alice --new-password-stdin --targets 1-4 --serial 1
netcmdr target set 3 --name ntp-pi
netcmdr time set --now
```

A `set` applies only the options you give it and prints what it changed; with
no options it changes nothing, so it is safe to run just to see what it offers.

One firmware behaviour to know about: hiding a serial port
(`netcmdr serial set 2 --visible off`) also strips that port from every account's
serial access, and showing the port again does **not** put those grants back.
`netcmdr` names the accounts that lose it so you can re-grant them.

Stored secrets — the LDAP bind password, RADIUS shared secrets, the SNMP
community — are masked in every `show` unless you pass `--show-secrets`. The
device keeps them in the clear and hands them back on every fetch, so any
admin-level account can read them.

**One `set` is one whole-configuration write.** The device has no partial
update: its only write verb replaces the entire configuration document, so each
command fetches, edits and posts the lot back (see
[`research/notes/FINDINGS.md`](research/notes/FINDINGS.md) §5.2.1). For several
changes at once — or to keep a configuration in version control — use the
document directly:

```sh
netcmdr export config.xml     # the device's own <config_data>, verbatim
$EDITOR config.xml
netcmdr import config.xml --confirm
```

Changing the network settings or the session port reboots the device; `netcmdr`
says so when it happens.

#### Operating the device

```sh
netcmdr power cycle 3              # power-cycle the target on port 3
netcmdr disconnect 3               # kick whoever is holding port 3
netcmdr lock / netcmdr unlock         # keep everyone else out while you work
netcmdr backup device.bak          # the device's own backup blob
netcmdr restore device.bak --confirm
netcmdr factory-reset --keep-network --confirm
netcmdr certificate --certificate cert.pem --key key.pem --confirm
netcmdr reboot --confirm
netcmdr fetch config               # raw XML (config|status|switch|…)
```

Everything destructive takes `--confirm`, including `factory-reset`, which also
takes `--keep-network` so the device comes back reachable.

The two firmware commands (`netcmdr upgrade`, `netcmdr siu upgrade`) are the
exception: they take **`--please-brick-my-device`** instead, and `--confirm`
deliberately does not work on them. They are built from the vendor client's own
request format and have **never been run against hardware** — nothing has
confirmed the device accepts what they send, and a firmware write that is wrong
or interrupted leaves hardware that cannot be recovered over the network. Do not
use them without a way to reflash out of band.

`netcmdr siu versions` reads the hardware and firmware revision of the Server
Interface Unit on each port. The device answers for **one port per request**, so
a sweep is one round trip per port, at a second or two each; rows print as they
arrive, and a port with nothing plugged in says so.

Everything else above has been run against a live device.

### `netcmdr-view` — KVM viewer

```sh
netcmdr-view --host 192.0.2.10 --target 1      # open a window on target 1
netcmdr-view --host 192.0.2.10 --list          # list targets
netcmdr-view --host 192.0.2.10 -t 1 --auto-adjust   # the device's own adjustment
netcmdr-view --host 192.0.2.10 -t 1 --align    # measure the capture offsets and fix them
netcmdr-view --host 192.0.2.10 -t 1 --snapshot out.ppm   # headless frame capture
```

In the window, keyboard and mouse go to the target and the image scales to the
window with the aspect ratio preserved. Four shortcuts stay local:

| Key | Does |
|---|---|
| **Ctrl+Alt+End** | send Ctrl+Alt+Delete to the target |
| **Ctrl+Alt+Home** | re-sync the remote pointer with yours |
| **Ctrl+Alt+A** | run the device's automatic video adjustment |
| **Ctrl+Alt+Shift+A** | measure the capture offsets from the picture and correct them |

Everything else is on the menu bar, which mirrors the stock client's toolbar
minus virtual media:

| Menu | Offers |
|---|---|
| **Keyboard** | Ctrl+Alt+Delete, and the combinations your own desktop swallows — Alt+Tab, Alt+F4, Ctrl+Shift+Esc, Super+L, Print Screen, Ctrl+Alt+F1… — plus *release held keys* |
| **Mouse** | sync pointer, calibrate, align, and how the local pointer is drawn over the video |
| **Video** | refresh, auto adjust, **re-align capture**, **manual adjustment** (position, phase, brightness, filter), fit-to-window or 1:1, resize the window to the target |
| **Power** | cycle / on / off for the current target's IP-PDU outlet, each behind a confirmation |
| **Session** | **switch target without reconnecting**, reload the target list, exclusive access, disconnect |

Switching target sends the device's own `change_port` command on the live
session, so the sockets, the login and the heartbeat all survive: the picture
just becomes the other machine's, at whatever resolution that machine runs.

The device does not switch instantly — it keeps sending the **old** target's
video for several seconds first (measured: 5 s when the new target was driving
video, 9.6 s when it was not). Showing that would be showing one machine under
another machine's name, so the viewer blanks the screen and puts up a
*Switching to …* notice, the same way it does while the first frame of a fresh
session is still on its way. Video comes back when the device itself says the
switch landed.

The device digitises analog VGA, so its sampling phase and gain drift per target
and per video mode; `--auto-adjust` (or Ctrl+Alt+A, or Video → Auto adjust) is
what re-derives them. It finds the capture *position* too, and keeps what it
finds — with one condition, below.

### Auto adjust, and when to reach past it

The device keeps a sampling offset per video mode, and on a mode whose figure it
has wrong the edge of the picture is simply never captured — measured on a target
driving 1360×768, sampling started 4 px into the active area and cut the first
character off every line. **Ask for an automatic adjustment and the device fixes
that itself**: it measured the same 364 this project measures by hand, wrote it
to its own stored config, and every later login — from any client, including the
vendor's — came up correct. It re-derives the phase and the filter at the same
time, and takes about six seconds.

**The condition: nothing may be set by hand when it runs.** The device measures
against the capture window your set already moved, then corrects against its own
stored figure, and the two errors add — asked for an adjustment with `hoffset`
sitting at 352 it stored 380, which is worse than where it started. Reconnect
first, then adjust. (This is what made earlier versions of this file say the
adjustment *undoes* a correction: every test of it followed a manual set.)

*Video → Re-align capture* (Ctrl+Alt+Shift+A, or `--align` on `netcmdr-view`)
measures the position from the picture instead of asking the device to. Lowering
an offset by one moves the picture one pixel right and brings one lane of the
scan line's blanking into view, so the error counts itself: probe a little below
where the device has it, count the blank lanes that appear, and the sum is the
answer. One probe per axis, about five seconds for both. The result goes in the
bar — `hoffset 368 → 364`. The answer is written to the device, so it is there
at the next login too. Reach for it when the device's own answer is wrong, or
when you would rather see the measurement than take the device's word for it.

It needs something on screen to measure against, and it says so rather than
guessing: blanking and black content are the same pixels to a digitiser, so a
target sitting at a bare console has nothing to anchor a *vertical* measurement
on until the console has scrolled. An answer that falls outside the range the
device declares for the knob is refused for the same reason — that range is
exactly the mode's blanking interval, so a real answer is always inside it.

A run ends in a single write that the device keeps, carrying both axes: the
answer for an axis it measured, and the figure the device started with for one
it could not — so a probe is never left behind. An alignment therefore survives
a logout; what still discards it is the device re-deriving the capture for
itself, and the bar says so when that happens.

Video → *Adjust manually…* opens the controls the stock client keeps in its
Advanced Video dialog, each applied live as you move it, with *Save to device*
for when one is worth keeping:

| Control | What it does |
|---|---|
| **Horizontal position** | where sampling starts on each line, in pixels. **Lower moves the picture right** — this is the one that recovers a cut-off left edge |
| **Vertical position** | which line capture starts at |
| **Phase** | where in each pixel's clock period the signal is sampled: what to reach for when the picture is sharp in places and smeared in others |
| **Brightness / contrast** | the digitiser's gain, so they also move the Noise reading |
| **Filter** | how hard the device filters the signal. *Auto* is not a value — picking it runs the automatic adjustment, exactly as the stock client does |

To find a position by hand, take it *down* until a black band appears along the
edge, then back up until the band is just gone: that band is the blanking
interval, so where it ends is where the picture really begins. *Re-align
capture* does exactly this, by counting the band instead of watching it.

Two things discard a position set by hand or by an alignment: **Auto adjust**,
and picking **Auto** as the filter — which is a request for the same
adjustment. Both replace it with the position the device measures for itself —
so run them from a fresh session, never on top of a set you have just made.

**Moving a control takes effect at once but lasts only for this session**, which
is what the stock client does too: one attribute at a time, which the device
applies to the picture without storing. ***Save to device*** is the other half
— the stock client's *OK* button in all but name. The device stores a
`<video_settings>` element when, and only when, it carries `brightness` and
`contrast`, whatever else rides along with it (`research/notes/FINDINGS.md`
§5.4), so a save sends the window's values as one such element and the next
login comes up on them.

*Restore* puts back everything the window opened with, and saves it, because
that is the only way to reach a value the device is already holding: a session
set whose value matches the stored one is silently dropped, and a restore is
made of exactly those values.

The right-hand end of the bar carries a **Noise** bar next to the resolution:
how much of the picture is changing, per second, the same reading the stock
client buries in its Advanced Video dialog. A target sitting at an idle desktop
should read zero; a steady non-zero level means the digitiser is picking up
noise on the VGA link rather than watching real work, and an auto-adjust will
usually settle it. Only the raw changed-tile count comes off the wire — the
percentage is computed here, exactly as the vendor's native library computes it
(see `research/notes/FINDINGS.md` §5.4.1).

Command results and the device's own messages appear to the left of that
reading and then clear themselves — a few seconds for something that has
happened, longer for work still running, which each progress report renews. The
device never says an auto-adjust or a calibration has finished, so the bar times
those out rather than sitting on the last thing it was told.

### `netcmdr-web` — the viewer in a browser

```sh
tools/build-web.sh                        # build into crates/netcmdr-web/dist
cargo build --release -p netcmdr-bridge      # ... which the bridge embeds
netcmdr-bridge --host 192.0.2.10
```

Then open <http://127.0.0.1:8080>, and log in with the same credentials the
desktop client takes. Everything is the same viewer: the menu bar, the target
switch, the manual video adjustment and the noise meter are `netcmdr-ui`, compiled
once for the desktop and once for WebAssembly. The device's LZW and tile codec
is decoded in the browser, from the bytes the device itself sent — the bridge
re-encodes nothing.

Building it needs the `wasm32-unknown-unknown` target and a `wasm-bindgen` that
matches the `wasm-bindgen` crate in `Cargo.lock`; the script says so, with the
version, if it is missing. `wasm-opt` (from **binaryen**) is used if it is
there and skipped with a note if it is not.

The module is what a browser must have in full before anything appears, so it
is carried in the bridge brotli-compressed and served that way:

| | size |
|---|---|
| raw | 4.3 MiB |
| gzip | 1.3 MiB |
| brotli | **914 KiB** |

`wasm-opt -Oz` is worth about 8% of that, rather than the usual much larger
figure, because the `web` profile in the workspace manifest already builds with
`opt-level = "z"`, fat LTO and one codegen unit.

#### Fonts

egui rasterises glyphs into a texture atlas itself, so the module has to carry
real font files. The fonts installed on the machine are not reachable from
WebAssembly — `queryLocalFonts()` is Chromium-only, wants a secure context and
asks the person's permission — and the page's CSS reaches nothing egui draws.

Carrying egui's four stock faces whole cost 1.3 MiB, close to a quarter of the
module and 673 KiB of the brotli download, to draw a login form, a menu bar and
one submenu arrow. So `default_fonts` is off and `crates/netcmdr-web/fonts` holds
three faces cut down to what can actually appear: ASCII, Latin-1 and the
punctuation the viewer uses, plus the four triangles and the warning sign that
egui's own widgets draw. That is 42 KiB rather than 1.4 MiB. Each face's licence
sits beside it, which is what the Ubuntu, Hack and MIT terms ask for.

Regenerate them after bumping egui, in case upstream has changed faces:

```sh
pip install fonttools
cargo fetch              # the originals come from the epaint_default_fonts crate
tools/subset-fonts.py
```

Two things a browser does differently, both of them the keyboard's fault:

- **Keys are read from `KeyboardEvent.code`, not from egui.** `egui::Key` cannot
  name 25 of this device's 106 keys — the whole numeric keypad, all three lock
  keys, Print Screen, Pause, Context Menu and Meta — and the keyboard channel's
  modifier word carries the lock states explicitly. So the page listens for keys
  itself, in the capture phase, ahead of the framework.
- **Some combinations cannot be taken from the browser.** Ctrl+W, Ctrl+T, Ctrl+N
  and F11 stay the browser's whatever a page asks. They are on the Keyboard
  menu, which is where a desktop window manager's claims are handled too.

`netcmdr-web` compiles to nothing on a desktop target, so `cargo test --workspace`,
clippy and rustfmt still cover the repository in one pass.

### `netcmdr-bridge` — the web bridge

```sh
netcmdr-bridge --host 192.0.2.10   # serve the client and the protocol on 127.0.0.1:8080
```

The device's TLS is why this exists. As [TLS](#tls) explains, it offers no ECDHE
suite at all, no TLS 1.3 and a 1024-bit DHE group; `netcmdr` and `netcmdr-view` reach
it by asking OpenSSL for security level 1, and a browser has no equivalent
switch. The handshake fails before a certificate is ever considered, so no
amount of clicking through a warning helps.

So the bridge terminates that TLS itself — the same settings the native client
already proved against hardware — and offers the browser plain HTTP and
WebSockets:

| Route | Purpose |
|---|---|
| `POST /message.pv` | forwarded verbatim to the device's management API |
| `GET /kvm?type=cmd\|video\|km\|vm` | a WebSocket carrying one session socket |
| `GET /api/info` | which device this bridge is pointed at |
| anything else | a file of the built-in web client, compressed where the browser takes it |

Each `/kvm` WebSocket is one of the session's four TCP connections. The client
sends its `SESSIONID` as the **first message**, as text; the bridge then
completes the device's plaintext handshake and, where the socket type calls for
it, the TLS upgrade — that decision belongs to the channel, not to the client.
Everything after that first message is opaque bytes. The bridge deliberately
does not understand `KVMMsg`: WebSocket message boundaries have nothing to do
with protocol message boundaries, and the client reassembles the stream exactly
as it would from a socket.

The token travels in the socket rather than the URL for two measured reasons.
It is base64 — on firmware 2.2.1258.1.0 a 44-character string routinely
containing `+`, `/` and `=`, which is to say every character a query string
handles badly. And **a browser never exposes the body of a failed WebSocket
handshake**, so a device refusal answered with an HTTP status would reach the
page as nothing more than "connection failed"; upgrading first and then closing
with a reason is the only way the client can say *why*.

**The bridge authenticates nobody.** It cannot: the only credential in this
protocol is the device's own `SESSIONID`, and minting one is what `/message.pv`
is for. Anything that can reach the listening port can reach the device's
management API and session ports, including to attempt logins. That is why the
default listen address is loopback, and why anything else belongs behind a
reverse proxy that does authenticate — and that terminates a TLS the browser
will accept. It is not an open relay, though: the device address and both ports
are fixed at startup, and no request can steer them.

#### One binary

The bridge is a single file. The web client is inside it, put there by the
crate's build script from `crates/netcmdr-web/dist` — or from `$NETCMDR_WEB_DIST`, if
that is set — and there is nothing to deploy, mount or point it at beside the
executable. It cannot be served a client that has drifted from the bridge it
came with, and there is no directory under it for a file to appear in.

It is stored **brotli-compressed, at quality 11**, for both of the reasons that
matter: that is a third of the size in the executable, and it is the copy most
browsers are sent, so the expensive compression happens once during the build
rather than on every start of the service. What startup does is the cheap half —
decompress each file and gzip it for the clients that do not take brotli — which
is under half a second for the whole client against the minute the brotli would
have cost. Both copies then stay in memory, immutable, so no request pays for
compression and none needs revalidating.

A file brotli does not shrink, such as a PNG, is stored as it is instead, and is
then simply offered in no encoding. Nothing in the binary is larger than the
file it came from.

The cost is on the build: a change to the client re-compresses it, which is
about half a minute for a module this size. Cargo reruns that only when the
`dist` directory actually changes, so an ordinary rebuild of the bridge does not
pay it.

Building the bridge with no client built is a warning rather than an error — the
binary still relays the protocol and answers 404 for everything else. That keeps
`cargo build --workspace` working for anyone without a wasm toolchain, which is
most people touching the native crates.

All of this has been run against a live device: a management login, the TLS
command channel answering with its `CONFIG_XML`, and video decoding to the same
picture the native viewer produces.

The client it serves is [`netcmdr-web`](#netcmdr-web--the-viewer-in-a-browser).

## Protocols

| Layer | Transport | Status |
|---|---|---|
| Management, reading | `POST /message.pv`, TLS 1.2, `CMA_REQUEST` → `PV_*` XML | **Implemented, verified** |
| Management, writing | `CMA_PERMISSION` → `CMA_CONFIG` → `CMA_PERMISSION` | **Implemented, verified** |
| Management, uploads | `CMA_POST`: firmware, SIU firmware, TLS certificate | Implemented, **unexercised** |
| Session framing (`KVMMsg`) | per-channel TCP 900, plaintext handshake ± TLS | **Implemented, verified** |
| Video codec | LZW / zlib + tile bitfield, RGB555 or YCbCr | **Implemented**; RGB555 verified, YCbCr ported but unexercised |
| Keyboard and mouse | 16-byte records on the kbmouse channel | **Implemented, verified** |
| Control commands | `<commands>` facade → `KVMMsg` | **Recovered**; the ones the viewer needs are implemented and verified, `change_port` and the video settings included |
| Virtual media | USB-over-IP in `KVMMsg` on its own channel | Specified, not implemented |
| Serial passthrough | SSH on 4001/4002 | Documented |

## TLS

The device speaks TLS 1.0 through 1.2 and presents a self-signed, SHA-256-signed
RSA-2048 certificate. Both the management endpoint and the session sockets
negotiate TLS 1.2.

It is nonetheless not reachable from a modern Rust TLS stack: it offers **no
ECDHE suite at all** — only DHE and static RSA — and no TLS 1.3, and rustls
implements neither of those key exchanges. So both crates use OpenSSL, with two
settings:

- `SECLEVEL=1`, because the device's DHE group is 1024-bit and OpenSSL's default
  level 2 requires 2048. Level 1 keeps forward secrecy; excluding DHE to fall
  back on static RSA would not.
- TLS 1.2 as the minimum version. Firmware older than 2.2.1258.1.0 offered only
  TLS 1.0 and is no longer supported.

Certificate verification is off. The certificate is self-signed with no trust
anchor, and the vendor's own Java client verifies nothing either — so this is no
weaker than stock, but the session is only as private as the network it crosses.

## Layout

```
crates/
  netcmdr-proto/    KVMMsg framing + enums (authoritative), command vocabulary
  netcmdr-mgmt/     /message.pv client, PV_* XML models and the editable
                 configuration document (OpenSSL, see TLS above)
  netcmdr-session/  the KVM session: channels, login, video codec, input
  netcmdr-ui/       the viewer's interface — menu, screen, input routing — with
                 nothing in it that knows whether it is a window or a tab
  netcmdr-cli/      the `netcmdr` binary
  netcmdr-view/     the `netcmdr-view` viewer: a window, a GL context and raw input,
                 over `netcmdr-ui` (winit + egui on OpenGL)
  netcmdr-bridge/   the `netcmdr-bridge` WebSocket and HTTPS bridge, so a browser can
                 speak a protocol its TLS stack cannot
  netcmdr-web/      the browser client: WebSockets, `fetch` and raw DOM keys,
                 over `netcmdr-ui` (eframe on WebGL)
research/        the protocol write-up, provenance for the vendor material it
                 was derived from, and the live fixtures the tests parse
```

No vendor material is redistributed here: no firmware images, client JARs,
decompiled sources, manuals or datasheets. The protocol was recovered from
them, and the recovery is what
[`research/notes/FINDINGS.md`](research/notes/FINDINGS.md) records;
[`research/SOURCES.md`](research/SOURCES.md) says where each original came from
so the work can be redone.

## Development

Standard cargo: `cargo build`, `cargo test --workspace`. Formatting is plain
rustfmt defaults, pinned in `rustfmt.toml` so the style does not drift with the
toolchain; `cargo fmt --all --check` and `cargo clippy --workspace
--all-targets` are both clean and expected to stay that way.

## License

Copyright (C) 2026 Doridian.

GNU Affero General Public License, version 3 or later — see [`LICENSE`](LICENSE).

Every dependency is permissively licensed (MIT, Apache-2.0, BSD, Zlib, ISC,
Unicode-3.0 and the like), so nothing here conflicts. Two worth naming: the
`self_cell` crate is `Apache-2.0 OR GPL-2.0-only` and is taken under Apache-2.0,
and `epaint_default_fonts` carries the fonts egui draws with under OFL-1.1 and
the Ubuntu Font Licence, which they keep. OpenSSL 3 is Apache-2.0; if you link
this against the end-of-life OpenSSL 1.1.1 instead, its old licence has the
advertising clause that GPL-family licences do not admit.

## Likely applicable to

B070-008-19-IP, B070-016-19-IP(2), B072-008-1-IP, B072-016-1-IP, B072-016-IP2,
B072-032-IP2, and the Minicom Smart 108/116 IP originals.

## Next steps

1. Virtual media: the fourth channel is USB-over-IP with a SCSI command handler.
2. Exercise the YCbCr and palette video paths. The live device sends RGB555
   (`ALX_PACK_16`) with LZW; the other formats are ported from the vendor's
   client and unit-tested, but no hardware has produced one yet.
3. Serial passthrough (`OPEN_TTY_SESSION` on the command channel).
4. Cascade configuration — the `switch`/`switch_id`/`switch_ports` fields that
   describe an analog KVM hanging off a port. The live unit has no cascade and
   answers the `switch` document with nothing, so there is nothing to verify
   against.
