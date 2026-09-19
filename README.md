# B070-008-19-IP

Cross-platform client for the Tripp-Lite B070-008-19-IP KVM (and possibly others).

The vendor ships a Windows-only Java client. This project replaces it.

## Status

Verified against real hardware (a B070-008-19-IP on firmware **2.2.1258.1.0**).

- **Management CLI works end to end.** `b070` reads config/status/targets/users
  and can power-control and reboot, over the device's real `/message.pv` API.
- **The KVM session works end to end.** `b070-view` opens a window on a target's
  screen with keyboard and mouse passed through: live video decoded from the
  device's own LZW + tile codec, verified against hardware.

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
b070 --host 192.0.2.10 config     # device, network, session settings
b070 --host 192.0.2.10 targets    # ports, names, which have an SIU attached
b070 --host 192.0.2.10 users      # local accounts
b070 --host 192.0.2.10 fetch config          # raw XML (config|status|switch|…)
b070 --host 192.0.2.10 power cycle 3         # power-cycle target on port 3
b070 --host 192.0.2.10 reboot --confirm
```

Credentials default to the factory `admin` / `access`; override with `--user` /
`--password` or `B070_USER` / `B070_PASSWORD`. The device speaks **TLS 1.2** with
a self-signed cert, so the client does not verify it — treat the management
network as trusted or tunnel it. See [TLS](#tls) for why this is OpenSSL and not
rustls.

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

## Protocols

| Layer | Transport | Status |
|---|---|---|
| Management | `POST /message.pv`, TLS 1.2, `CMA_*`/`PV_*` XML | **Implemented, verified** |
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
  b070-mgmt/     /message.pv client and PV_* XML models (OpenSSL, see TLS above)
  b070-session/  the KVM session: channels, login, video codec, input
  b070-cli/      the `b070` binary
  b070-view/     the `b070-view` viewer (winit + egui on OpenGL)
research/        vendor firmware, extracted clients (2013 + live 2017),
                 decompiled sources, disassembly, live fixtures, findings
```

## Likely applicable to

B070-008-19-IP, B070-016-19-IP(2), B072-008-1-IP, B072-016-1-IP, B072-016-IP2,
B072-032-IP2, and the Minicom Smart 108/116 IP originals.

## Next steps

1. Virtual media: the fourth channel is USB-over-IP with a SCSI command handler.
2. Exercise the YCbCr and palette video paths. The live device sends RGB555
   (`ALX_PACK_16`) with LZW; the other formats are ported from the vendor's
   client and unit-tested, but no hardware has produced one yet.
3. Serial passthrough (`OPEN_TTY_SESSION` on the command channel).
