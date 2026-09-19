# B070-008-19-IP

Cross-platform client for the Tripp-Lite B070-008-19-IP KVM (and possibly others).

The vendor ships a Windows-only Java client. This project replaces it.

## Status

Verified against real hardware (a B070-008-19-IP on firmware **2.1.1184**).

- **Management CLI works end to end.** `b070` reads config/status/targets/users
  and can power-control and reboot, over the device's real `/message.pv` API.
- **Session protocol is fully specified** (the current firmware's client is pure
  Java, so the wire format is transcribed rather than guessed), but the video
  client is not built yet.

The device is a rebadged **Minicom Smart 116 IP** running Windows CE on an Intel
IXP425. See [`research/notes/FINDINGS.md`](research/notes/FINDINGS.md) §5.0 for
the hardware-verified summary and [`research/SOURCES.md`](research/SOURCES.md)
for provenance.

## What works today

```sh
cargo build --release
```

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
`--password` or `B070_USER` / `B070_PASSWORD`. The device speaks **TLS 1.0** with
a self-signed cert, so the client uses OpenSSL and does not verify it — treat the
management network as trusted or tunnel it.

### `b070-capture` — session capture harness

A `KVMMsg` frame decoder and TLS proxy skeleton. **Needs rework** for the
current firmware's session model (four sockets, a plaintext handshake, and TLS
on only two of them — see the note at the top of the source). Kept for the
decoder and as the basis for that rework.

## Protocols

| Layer | Transport | Status |
|---|---|---|
| Management | `POST /message.pv`, TLS 1.0, `CMA_*`/`PV_*` XML | **Implemented, verified** |
| Session framing (`KVMMsg`) | per-channel TCP 900, plaintext handshake ± TLS | **Specified** (`b070-proto`), not wired |
| Control commands | `<commands>` facade → `KVMMsg` | **Recovered** |
| Video codec | LZW + YCbCr, in session payloads | **Identified**, not ported |
| Serial passthrough | SSH on 4001/4002 | Documented |

## Layout

```
crates/
  b070-proto/    KVMMsg framing + enums (authoritative), command vocabulary
  b070-mgmt/     /message.pv client and PV_* XML models (OpenSSL/TLS 1.0)
  b070-cli/      the `b070` binary
  b070-capture/  KVMMsg frame decoder + capture proxy skeleton
research/        vendor firmware, extracted clients (2013 + live 2017),
                 decompiled sources, disassembly, live fixtures, findings
```

## Likely applicable to

B070-008-19-IP, B070-016-19-IP(2), B072-008-1-IP, B072-016-1-IP, B072-016-IP2,
B072-032-IP2, and the Minicom Smart 108/116 IP originals.

## Next steps

1. Build the session client: open the four sockets with the plaintext handshake,
   authenticate on the cmd channel (`VERIFY_USER`), decode the LZW+YCbCr video.
2. Rework `b070-capture` for the real socket model to validate the above.
3. A cross-platform viewer UI on top.
