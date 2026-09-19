# B070-008-19-IP

Cross-platform client for the Tripp-Lite B070-008-19-IP KVM (and possibly others).

The vendor ships a Windows-only Java client. This project replaces it.

## Status

Verified against real hardware (a B070-008-19-IP on firmware **2.2.1258.1.0**).

- **Management CLI works end to end.** `b070` reads and *writes* every setting
  the device has — network, users, LDAP, RADIUS, security policy, serial ports,
  SNMP, power — over the device's real `/message.pv` API, and can power-control,
  back up, restore and reboot.
- **The KVM session works end to end.** `b070-view` opens a window on a target's
  screen with keyboard and mouse passed through: live video decoded from the
  device's own LZW + tile codec, verified against hardware.
- **A browser can reach the device.** `b070-bridge` terminates the device's
  un-negotiable TLS and relays both the management API and the session sockets
  over WebSockets. The web client that will use it is next.

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

### `b070` — management CLI

```sh
b070 --host 192.0.2.10 status     # firmware, LAN, per-port state (+ SIU detection)
b070 --host 192.0.2.10 config     # every setting, in one page
b070 --host 192.0.2.10 targets    # ports and names
b070 --host 192.0.2.10 users      # local accounts
```

Credentials default to the factory `admin` / `access`; override with `--user` /
`--password` or `B070_USER` / `B070_PASSWORD`. `--host` also comes from
`B070_HOST`. The device speaks **TLS 1.2** with a self-signed cert, so the
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
b070 network set --ipv4 static --address 192.0.2.10 --subnet 255.255.255.0 \
                 --gateway 192.0.2.1 --dns-source static --dns 192.0.2.53
b070 security set --max-attempts 5 --attempt-window 3m --block-for 30m
b070 ldap server add ldap.example.com --protocol ldaps
b070 ldap set --mode group --search-base dc=example,dc=com --user-attribute uid
b070 auth set --order local,ldap
b070 user add alice --new-password-stdin --targets 1-4 --serial 1
b070 target set 3 --name ntp-pi
b070 time set --now
```

A `set` applies only the options you give it and prints what it changed; with
no options it changes nothing, so it is safe to run just to see what it offers.

One firmware behaviour to know about: hiding a serial port
(`b070 serial set 2 --visible off`) also strips that port from every account's
serial access, and showing the port again does **not** put those grants back.
`b070` names the accounts that lose it so you can re-grant them.

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
b070 export config.xml     # the device's own <config_data>, verbatim
$EDITOR config.xml
b070 import config.xml --confirm
```

Changing the network settings or the session port reboots the device; `b070`
says so when it happens.

#### Operating the device

```sh
b070 power cycle 3              # power-cycle the target on port 3
b070 disconnect 3               # kick whoever is holding port 3
b070 lock / b070 unlock         # keep everyone else out while you work
b070 backup device.bak          # the device's own backup blob
b070 restore device.bak --confirm
b070 factory-reset --keep-network --confirm
b070 certificate --certificate cert.pem --key key.pem --confirm
b070 reboot --confirm
b070 fetch config               # raw XML (config|status|switch|…)
```

Everything destructive takes `--confirm`, including `factory-reset`, which also
takes `--keep-network` so the device comes back reachable.

The two firmware commands (`b070 upgrade`, `b070 siu upgrade`) are the
exception: they take **`--please-brick-my-device`** instead, and `--confirm`
deliberately does not work on them. They are built from the vendor client's own
request format and have **never been run against hardware** — nothing has
confirmed the device accepts what they send, and a firmware write that is wrong
or interrupted leaves hardware that cannot be recovered over the network. Do not
use them without a way to reflash out of band.

`b070 siu versions` reads the hardware and firmware revision of the Server
Interface Unit on each port. The device answers for **one port per request**, so
a sweep is one round trip per port, at a second or two each; rows print as they
arrive, and a port with nothing plugged in says so.

Everything else above has been run against a live device.

### `b070-view` — KVM viewer

```sh
b070-view --host 192.0.2.10 --target 1      # open a window on target 1
b070-view --host 192.0.2.10 --list          # list targets
b070-view --host 192.0.2.10 -t 1 --auto-adjust   # re-align the analog capture
b070-view --host 192.0.2.10 -t 1 --snapshot out.ppm   # headless frame capture
```

In the window, keyboard and mouse go to the target and the image scales to the
window with the aspect ratio preserved. Three shortcuts stay local:

| Key | Does |
|---|---|
| **Ctrl+Alt+End** | send Ctrl+Alt+Delete to the target |
| **Ctrl+Alt+Home** | re-sync the remote pointer with yours |
| **Ctrl+Alt+A** | run the device's automatic video adjustment |

Everything else is on the menu bar, which mirrors the stock client's toolbar
minus virtual media:

| Menu | Offers |
|---|---|
| **Keyboard** | Ctrl+Alt+Delete, and the combinations your own desktop swallows — Alt+Tab, Alt+F4, Ctrl+Shift+Esc, Super+L, Print Screen, Ctrl+Alt+F1… — plus *release held keys* |
| **Mouse** | sync pointer, calibrate, align, and how the local pointer is drawn over the video |
| **Video** | refresh, auto adjust, **manual adjustment** (position, phase, brightness, filter), fit-to-window or 1:1, resize the window to the target |
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

The device digitises analog VGA, so its horizontal position and sampling phase
drift per target and per video mode; `--auto-adjust` (or Ctrl+Alt+A, or
Video → Auto adjust) is what re-centres the captured image.

### When the automatic adjustment is not enough

The device keeps a horizontal sampling offset per video mode, and on a mode
whose figure it has wrong, the edge of the picture is simply never captured —
measured on a target driving 1360×768, the device starts sampling 4 px into the
active area and cuts the first character off every line. **Its own automatic
adjustment does not touch that value**: ask for one and the device answers with
a byte-identical `<video_settings>`, so Ctrl+Alt+A can never fix it.

Video → *Adjust manually…* opens the controls the stock client keeps in its
Advanced Video dialog, each applied live as you move it:

| Control | What it does |
|---|---|
| **Horizontal position** | where sampling starts on each line, in pixels. **Lower moves the picture right** — this is the one that recovers a cut-off left edge |
| **Vertical position** | which line capture starts at |
| **Phase** | where in each pixel's clock period the signal is sampled: what to reach for when the picture is sharp in places and smeared in others |
| **Brightness / contrast** | the digitiser's gain, so they also move the Noise reading |
| **Filter** | how hard the device filters the signal. *Auto* is not a value — picking it runs the automatic adjustment, exactly as the stock client does |

To find the right horizontal position, take it *down* until a black band appears
along the left edge, then back up until the band is just gone: that band is the
scan line's back porch, so its edge is where the picture really begins.

These settings are per session and are **not** persisted — the device reads its
own defaults again at the next login, so a mode it has wrong needs correcting
each time you connect.

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

### `b070-bridge` — the web bridge

```sh
b070-bridge --host 192.0.2.10                     # serve on 127.0.0.1:8080
b070-bridge --host 192.0.2.10 --static-dir ./web  # ... and the web client with it
```

The device's TLS is why this exists. As [TLS](#tls) explains, it offers no ECDHE
suite at all, no TLS 1.3 and a 1024-bit DHE group; `b070` and `b070-view` reach
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
| anything else | a file from `--static-dir` |

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

All of this has been run against a live device: a management login, the TLS
command channel answering with its `CONFIG_XML`, and video decoding to the same
picture the native viewer produces.

The web client this exists to serve is not written yet. The bridge is the piece
that had to come first, because it is what makes a browser client possible at
all.

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
  b070-proto/    KVMMsg framing + enums (authoritative), command vocabulary
  b070-mgmt/     /message.pv client, PV_* XML models and the editable
                 configuration document (OpenSSL, see TLS above)
  b070-session/  the KVM session: channels, login, video codec, input
  b070-cli/      the `b070` binary
  b070-view/     the `b070-view` viewer (winit + egui on OpenGL)
  b070-bridge/   the `b070-bridge` WebSocket and HTTPS bridge, so a browser can
                 speak a protocol its TLS stack cannot
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

1. The web client: `b070-bridge` serves it and relays the protocol for it, but
   the WebAssembly build of the viewer itself is still to come.
2. Virtual media: the fourth channel is USB-over-IP with a SCSI command handler.
3. Exercise the YCbCr and palette video paths. The live device sends RGB555
   (`ALX_PACK_16`) with LZW; the other formats are ported from the vendor's
   client and unit-tested, but no hardware has produced one yet.
4. Serial passthrough (`OPEN_TTY_SESSION` on the command channel).
5. Cascade configuration — the `switch`/`switch_id`/`switch_ports` fields that
   describe an analog KVM hanging off a port. The live unit has no cascade and
   answers the `switch` document with nothing, so there is nothing to verify
   against.
