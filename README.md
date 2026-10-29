# NetCmdr

Cross-platform replacement for the vendor's Java client for the Tripp-Lite
B070-008-19-IP KVM (and likely others).

## Status

Verified on a B070-008-19-IP, firmware **2.2.1258.1.0**.

- **Management CLI:** `netcmdr` reads and writes every setting (network, users,
  LDAP, RADIUS, security, serial, SNMP, power) via `/message.pv`, and can
  power-control, back up, restore and reboot.
- **KVM viewer:** `netcmdr-view` shows a target's screen with keyboard and mouse
  passthrough, decoding the device's LZW + tile codec.
- **Browser viewer:** `netcmdr-bridge` terminates the device's TLS and relays for
  `netcmdr-web`, the same viewer compiled to WebAssembly.

Virtual media (the session's fourth channel) is not implemented.

The device is a rebadged **Minicom Smart 116 IP** (Windows CE, Intel IXP425).
See [`research/notes/FINDINGS.md`](research/notes/FINDINGS.md) §5.0 and
[`research/SOURCES.md`](research/SOURCES.md).

## Usage

```sh
cargo build --release
```

Requires Rust 1.95+ (set by egui).

### `netcmdr`: management CLI

```sh
netcmdr --host 192.0.2.10 status     # firmware, LAN, per-port state (+ SIU detection)
netcmdr --host 192.0.2.10 config     # every setting
netcmdr --host 192.0.2.10 targets    # ports and names
netcmdr --host 192.0.2.10 users      # local accounts
```

Credentials default to `admin` / `access`; override with `--user` / `--password`
or `NETCMDR_USER` / `NETCMDR_PASSWORD`. `--host` can come from `NETCMDR_HOST`.

The device's certificate is verified, so connect by a name it lists. The
factory certificate is self-signed: pass `--insecure` (`-k`, or
`NETCMDR_INSECURE=1`) to skip the check, or `--ca-file` to trust it. See
[TLS](#tls).

#### Changing settings

Each area has `show` and `set`:

| Area | Covers |
|---|---|
| `device` | name, session TCP port |
| `network` | IPv4 and IPv6 addressing, DNS |
| `ssh` | serial-over-SSH passthrough and its ports |
| `security` | lockout policy, password rules, idle timeout |
| `auth` | authentication sources and order |
| `ldap` | directory settings, plus `ldap server add`/`remove` |
| `radius` | realm and accounting, plus `radius server add`/`remove` |
| `snmp` | traps, destination, community |
| `time` | the device clock |
| `serial` | serial port line settings |
| `kvm-manager` | central management appliance |
| `target` | KVM port names |
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

`set` applies only the options given and prints what changed; with none it does
nothing.

- Hiding a serial port (`netcmdr serial set 2 --visible off`) removes it from
  every account, and showing it again does **not** restore access. `netcmdr`
  lists the affected accounts.
- Secrets (LDAP bind password, RADIUS secrets, SNMP community) are masked unless
  `--show-secrets` is given. The device returns them in the clear to any admin.
- Changing network settings or the session port reboots the device.

**Each `set` rewrites the whole configuration**, since the device has no partial
update (FINDINGS.md §5.2.1). To make several changes at once, or keep the config
in version control:

```sh
netcmdr export config.xml     # the device's <config_data>, verbatim
$EDITOR config.xml
netcmdr import config.xml --confirm
```

#### Operating the device

```sh
netcmdr power cycle 3              # power-cycle the target on port 3
netcmdr disconnect 3               # kick whoever is on port 3
netcmdr lock / netcmdr unlock      # lock out other users
netcmdr backup device.bak          # the device's backup blob
netcmdr restore device.bak --confirm
netcmdr factory-reset --keep-network --confirm
netcmdr certificate --certificate cert.pem --key key.pem --confirm
netcmdr reboot --confirm
netcmdr fetch config               # raw XML (config|status|switch|…)
```

Destructive commands require `--confirm`.

The firmware commands (`netcmdr upgrade`, `netcmdr siu upgrade`) instead require
**`--please-brick-my-device`**. They follow the vendor client's request format
but have **never been run on hardware**, and a bad flash can't be recovered over
the network. Only use them if you can reflash out of band.

`netcmdr siu versions` reads each port's SIU hardware and firmware revision. The
device answers one port per request (a second or two each), so rows print as
they arrive.

Everything else has been run against a live device.

### `netcmdr-view`: KVM viewer

```sh
netcmdr-view --host 192.0.2.10 --target 1      # open a window on target 1
netcmdr-view --host 192.0.2.10 --list          # list targets
netcmdr-view --host 192.0.2.10 -t 1 --auto-adjust   # device auto-adjust
netcmdr-view --host 192.0.2.10 -t 1 --align    # measure and fix capture offsets
netcmdr-view --host 192.0.2.10 -t 1 --snapshot out.ppm   # headless capture
```

Input goes to the target; the image scales with aspect ratio preserved. Local
shortcuts:

| Key | Does |
|---|---|
| **Ctrl+Alt+End** | send Ctrl+Alt+Delete |
| **Ctrl+Alt+Home** | re-sync the remote pointer |
| **Ctrl+Alt+A** | device auto-adjust |
| **Ctrl+Alt+Shift+A** | measure and correct capture offsets |

The menu bar mirrors the vendor toolbar, minus virtual media:

| Menu | Offers |
|---|---|
| **Keyboard** | Ctrl+Alt+Delete and combinations your desktop intercepts (Alt+Tab, Alt+F4, Ctrl+Shift+Esc, Super+L, Print Screen, Ctrl+Alt+F1…), plus *release held keys* |
| **Mouse** | sync pointer, calibrate, align, local pointer style |
| **Video** | refresh, auto adjust, **re-align capture**, **manual adjustment**, fit or 1:1, resize window to target |
| **Power** | cycle / on / off for the target's IP-PDU outlet, with confirmation |
| **Session** | **switch target**, reload targets, exclusive access, disconnect |

Switching target keeps the same session (sockets, login and heartbeat). The
device keeps sending the old target's video for a while (5 s with a signal,
9.6 s without), so the viewer shows *Switching to …* until the device confirms.

### Capture alignment

The device samples analog VGA using a per-mode offset table. When an entry is
wrong the picture's edge is cut off (on a 1360×768 target, sampling started 4 px
late and clipped the first character of every line).

**Auto adjust** (`--auto-adjust`, Ctrl+Alt+A, Video → Auto adjust) fixes this
itself in about six seconds: it found the same `hoffset` 364 we measure, stored
it, and every later login (any client) was correct. It also re-derives phase and
filter. **Run it only in a fresh session**: after a manual set its errors
compound (from `hoffset` 352 it stored 380).

**Re-align capture** (Ctrl+Alt+Shift+A, `--align`) measures from the picture.
Lowering an offset by one shifts the picture a pixel and exposes one blank lane,
so probing below the current value and counting blank lanes gives the answer:
one probe per axis, about five seconds total. The result shows in the status bar
(`hoffset 368 → 364`) and is stored on the device.

- It needs content on screen: blanking and black look identical, so a bare
  console can't be measured vertically until it scrolls.
- Answers outside the device's declared range (the mode's blanking interval)
  are refused.
- It finishes with one stored write of both axes, so no probe is left behind.
  The bar reports if the device later re-derives and discards it.

**Video → Adjust manually…** exposes the vendor's Advanced Video controls, applied
live:

| Control | Effect |
|---|---|
| **Horizontal position** | sampling start per line, in pixels. **Lower moves the picture right**, recovering a cut-off left edge |
| **Vertical position** | first captured line |
| **Phase** | sampling point within each pixel clock; use when the picture is sharp in places and smeared in others |
| **Brightness / contrast** | digitiser gain (also affects Noise) |
| **Filter** | signal filtering. *Auto* runs auto-adjust, as in the vendor client |

To position by hand, lower until a black band appears at the edge, then raise
until it's just gone.

Auto adjust and the *Auto* filter both replace any manual or aligned position.

Controls apply for this session only. **Save to device** persists them: the
device stores a `<video_settings>` element only if it includes both
`brightness` and `contrast` (FINDINGS.md §5.4). **Restore** reapplies and saves
the opening values; a session set equal to the stored value is silently
dropped, so it has to be a stored write.

The status bar shows a **Noise** meter: the percentage of the picture changing
per second, computed as the vendor library does (FINDINGS.md §5.4.1). An idle
desktop should read zero; a steady non-zero level indicates VGA noise, which
auto-adjust usually fixes.

Status messages expire on their own. The device never reports that auto-adjust
or calibration finished, so those notes time out.

### `netcmdr-web`: browser viewer

```sh
tools/build-web.sh                        # build into crates/netcmdr-web/dist
tools/pack-web.sh                         # gzip into bridge/web/dist
go build -C bridge -o netcmdr-bridge .    # embed in the bridge
./bridge/netcmdr-bridge --host 192.0.2.10
```

Open <http://127.0.0.1:8080> and log in as with the desktop client. The UI is
the same `netcmdr-ui` code, and video is decoded in the browser from the
device's bytes.

Building needs the `wasm32-unknown-unknown` target and a `wasm-bindgen` CLI
matching `Cargo.lock` (the script reports the version). `wasm-opt` (binaryen)
is optional.

| | size |
|---|---|
| raw | 4.3 MiB |
| gzip | **1.3 MiB** |

`wasm-opt -Oz` saves only ~8% because the `web` profile already uses
`opt-level = "z"`, fat LTO and one codegen unit.

#### Fonts

egui needs embedded font files, and its stock faces are 1.3 MiB. So
`default_fonts` is off and `crates/netcmdr-ui/fonts` holds three subset faces
(ASCII, Latin-1, the viewer's punctuation, and egui's icon glyphs): 42 KiB, each
with its licence. Regenerate after bumping egui:

```sh
pip install fonttools
cargo fetch              # originals come from epaint_default_fonts
tools/subset-fonts.py
```

#### Keyboard

- Keys come from `KeyboardEvent.code`, captured before egui, because
  `egui::Key` lacks 25 of the device's 106 keys (keypad, lock keys, Print Screen,
  Pause, Context Menu, Meta).
- Browsers always keep Ctrl+W, Ctrl+T, Ctrl+N and F11; use the Keyboard menu.

`netcmdr-web` is empty on desktop targets, so workspace-wide cargo commands
still work.

### `netcmdr-bridge`: web bridge

```sh
netcmdr-bridge --host 192.0.2.10   # serve on 127.0.0.1:8080
```

Browsers can't open the device's TCP session sockets, so the bridge
terminates its TLS and offers HTTP and WebSockets:

| Route | Purpose |
|---|---|
| `POST /message.pv` | forwarded verbatim to the management API |
| `GET /kvm?type=cmd\|video\|km\|vm` | WebSocket for one session socket |
| `GET /api/info` | which device the bridge targets |
| anything else | the embedded web client |

Each `/kvm` WebSocket is one session TCP connection. The client sends its
`SESSIONID` as the first (text) message; the bridge does the handshake and any
TLS, then relays opaque bytes. It doesn't parse `KVMMsg`.

The token isn't in the URL because it's a credential, it's base64 with `+`, `/`
and `=`, and browsers hide failed-handshake responses, so only a close frame
after upgrading can carry an error reason.

**The bridge has no authentication**, so anyone who can reach it can reach the
device. It listens on loopback by default; otherwise put it behind an
authenticating reverse proxy with browser-trusted TLS. The device address and
ports are fixed at startup, so it isn't an open relay.

#### Single binary

The web client is embedded via `go:embed` from `bridge/web/dist`, filled by
`tools/pack-web.sh`. Files are stored gzipped (raw if gzip doesn't help), and an
identity copy is unpacked at startup. Both stay in memory.

Without a packed client the bridge still relays and returns 404 for other paths,
so `go build ./...` works without a wasm toolchain.

Tested against a live device: management login, TLS command channel and video.

#### Container

`nix build .#netcmdr-bridge` produces a static binary (Go, `CGO_ENABLED=0`) that
reads nothing at runtime except `/etc/resolv.conf` if `--host` is a name, and
the system CA bundle (plus `--ca-file`) unless `--insecure`.
`nix build .#netcmdr-bridge-image` wraps it in a single-layer image with no base
or shell.

```sh
docker load < $(nix build --no-link --print-out-paths .#netcmdr-bridge-image)
docker run -d -e NETCMDR_HOST=192.0.2.10 -p 8080:8080 netcmdr-bridge:0.1.0
```

Add `-e NETCMDR_INSECURE=1` for a self-signed certificate, or mount a PEM file
and set `NETCMDR_CA_FILE`. The image carries the Mozilla CA bundle.

- The device is set via the environment so extra arguments don't replace it.
- The image listens on `0.0.0.0:8080`; the same warning about reachability
  applies.
- It runs as uid 65534 and handles `SIGTERM`/`SIGINT`, so `docker stop` is
  immediate.
- Images are Linux-only. On macOS use `nix build .#netcmdr-bridge` or `go build`
  in [`bridge/`](bridge/).

`.#netcmdr-bridge-image-amd64` and `-arm64` build for a specific architecture
from any host (cross-compiling is just `GOARCH`). CI pushes both to
`git.foxden.network/foxden/netcmdr:latest` as a manifest list.

## Protocols

| Layer | Transport | Status |
|---|---|---|
| Management, reading | `POST /message.pv`, TLS 1.2, `CMA_REQUEST` → `PV_*` XML | **Implemented, verified** |
| Management, writing | `CMA_PERMISSION` → `CMA_CONFIG` → `CMA_PERMISSION` | **Implemented, verified** |
| Management, uploads | `CMA_POST`: firmware, SIU firmware, TLS certificate | Implemented; certificate **verified**, firmware **unexercised** |
| Session framing (`KVMMsg`) | per-channel TCP 900, plaintext handshake ± TLS | **Implemented, verified** |
| Video codec | LZW / zlib + tile bitfield, RGB555 or YCbCr | **Implemented**; RGB555 verified, YCbCr unexercised |
| Keyboard and mouse | 16-byte records on the kbmouse channel | **Implemented, verified** |
| Control commands | `<commands>` facade → `KVMMsg` | **Recovered**; the viewer's (incl. `change_port`, video settings) implemented and verified |
| Virtual media | USB-over-IP in `KVMMsg` on its own channel | Specified, not implemented |
| Serial passthrough | SSH on 4001/4002 | Documented |

## TLS

The device speaks TLS 1.2 only, with **no ECDHE**, so rustls can't connect and
the Rust crates use OpenSSL. Firmware before 2.2.1258.1.0 (TLS 1.0 only) is
unsupported. What it accepts differs by port:

| Port | Accepts | netcmdr uses |
|---|---|---|
| HTTPS (443) | DHE (1024-bit group) with AES-GCM; static RSA as `AES256-SHA` only | `DHE+AESGCM` at OpenSSL security level 1 |
| Session (900) | static RSA only, with AES-GCM | `kRSA+AESGCM` at the default level 2 |

Level 1 is needed on 443 only because the device's DHE group is 1024-bit.
Excluding DHE would allow level 2, but only by falling back to `AES256-SHA`:
CBC with a SHA-1 MAC and no forward secrecy. The session port has no forward
secrecy to offer.

### Certificates

Every client checks that the certificate chains to a trusted one and names the
host you connected to:

- `--insecure` / `-k` (`NETCMDR_INSECURE`) skips the check, as the vendor
  client always does. Needed for the factory self-signed certificate, unless
  you trust it with `--ca-file` and connect by the name in it.
- `--ca-file` (`NETCMDR_CA_FILE`) trusts the PEM certificates in a file on top
  of the system's. Any of them can anchor the chain: a private CA, an
  intermediate, or the device's own certificate.

**The device sends no chain**, only its own certificate. For a certificate from
a public CA, `--ca-file` needs the issuing intermediate (for Let's Encrypt, the
one at the leaf's *CA Issuers* URL).

Both ports serve the same certificate, installed with `netcmdr certificate`.

### The bridge is Go

Go has no DHE, so the bridge uses static RSA suites, listed explicitly since
Go 1.22 dropped them from the defaults. **This loses forward secrecy on
`/message.pv`** and means `AES256-SHA` there: someone who records traffic and
later obtains the device's RSA key can decrypt it. The session sockets on port
900 already use static RSA (`AES256-GCM-SHA384`) with either client. Treat the
network as trusted or tunnel it. The bridge takes the same `-insecure` and
`-ca-file` options.

In return, the bridge cross-compiles with just `GOARCH`: no libc, OpenSSL or
extra toolchain. `flake.nix` exports `GOARCH` in `postConfigure` (so the module
fetch is shared), flattens Go's cross-build subdirectory, and checks the output's
ELF `e_machine` so a mislabelled image fails the build.

## Layout

```
crates/
  netcmdr-proto/    KVMMsg framing and enums, command vocabulary
  netcmdr-mgmt/     /message.pv client, PV_* XML models, editable config (OpenSSL)
  netcmdr-session/  KVM session: channels, login, video codec, input
  netcmdr-ui/       viewer UI (menu, screen, input routing), host-agnostic
  netcmdr-cli/      the `netcmdr` binary
  netcmdr-view/     the `netcmdr-view` desktop shell (winit + egui on OpenGL)
  netcmdr-web/      the browser shell (eframe on WebGL)
bridge/             `netcmdr-bridge`, in Go (see "The bridge is Go")
research/           protocol write-up, vendor material provenance, test fixtures
```

No vendor material (firmware, JARs, decompiled code, manuals) is redistributed.
[`FINDINGS.md`](research/notes/FINDINGS.md) documents the recovered protocol and
[`SOURCES.md`](research/SOURCES.md) where each original came from.

## Development

`cargo build`, `cargo test --workspace`. `cargo fmt --all --check` and
`cargo clippy --workspace --all-targets` must stay clean (rustfmt defaults,
pinned in `rustfmt.toml`).

The bridge is a separate Go module in [`bridge/`](bridge/): `go build ./...`,
`go test ./...`, `golangci-lint run ./...` (config in
[`bridge/.golangci.yml`](bridge/.golangci.yml)). The Nix build needs the module
hash in [`bridge/vendor-hash.txt`](bridge/vendor-hash.txt), which Renovate
updates. After editing `go.mod` by hand:

```sh
echo 'sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=' > bridge/vendor-hash.txt
nix build '.#netcmdr-bridge.goModules'   # fails and prints the correct hash
```

CI ([`.forgejo/workflows/test.yml`](.forgejo/workflows/test.yml)) lints then
tests on every push and PR, using the flake's dev shell.

| Command | Builds |
|---|---|
| `nix develop` | a shell with the Rust and Go toolchains (used by CI) |
| `nix build` | the CLI and the viewer, with dlopened libraries on the RPATH |
| `nix build .#netcmdr-bridge` | the bridge with the wasm client embedded |
| `nix build .#netcmdr-bridge-image` | the bridge image for this architecture (Linux) |
| `nix build .#netcmdr-bridge-image-{amd64,arm64}` | the image for a given architecture (Linux) |

## License

Copyright (C) 2026 Doridian.

GNU Affero General Public License, version 3 or later; see [`LICENSE`](LICENSE).

All dependencies are permissively licensed. Notably, `self_cell` is taken under
Apache-2.0 (of `Apache-2.0 OR GPL-2.0-only`), and `epaint_default_fonts` keeps
its OFL-1.1 and Ubuntu Font Licence terms. OpenSSL 3 is Apache-2.0; OpenSSL
1.1.1's licence has an advertising clause incompatible with the GPL family.

## Likely applicable to

B070-008-19-IP, B070-016-19-IP(2), B072-008-1-IP, B072-016-1-IP, B072-016-IP2,
B072-032-IP2, and the Minicom Smart 108/116 IP originals.

## Next steps

1. Virtual media: USB-over-IP with a SCSI command handler.
2. Exercise the YCbCr and palette video paths (ported and unit-tested; the
   device has only sent RGB555 LZW so far).
3. Serial passthrough (`OPEN_TTY_SESSION` on the command channel).
4. Cascade configuration (`switch`/`switch_id`/`switch_ports`); no cascaded unit
   is available to test against.
