# B070-008-19-IP

Cross-platform client for the Tripp-Lite B070-008-19-IP KVM (and possibly others).

The vendor ships a Windows-only client. This project aims to replace it.

## Status

**No hardware yet.** Everything here was derived from vendor firmware, the
official Java/Windows client, and documentation. The management API is
implemented and unit-tested against the device's own reference XML. The session
protocol's framing is recovered but unverified against a real device.

The device is a rebadged **Minicom Smart 116 IP** running **Windows CE on an
Intel IXP425**. Its stock client is a Java Web Start app
(`https://<ip>/targets.jnlp`) that is a thin Swing shim over a native JNI
library — and that library already ships a **Linux i386 build**, which serves
both as a reference implementation and as a possible short-term workaround.

See [`research/notes/FINDINGS.md`](research/notes/FINDINGS.md) for the full
analysis and [`research/SOURCES.md`](research/SOURCES.md) for provenance.

## What works today

```sh
cargo build --release
```

### `b070` — management CLI

Talks to the device's HTTPS management API (`POST /config/`). Fully
implemented; needs a device to point at.

```sh
b070 --host 192.168.0.254 status
b070 --host 192.168.0.254 config
b070 --host 192.168.0.254 targets
b070 --host 192.168.0.254 users
b070 --host 192.168.0.254 switches
b070 --host 192.168.0.254 fetch config      # raw XML
b070 --host 192.168.0.254 reboot --confirm
```

Host, port, username and password can come from `B070_HOST`, `B070_PORT`,
`B070_USER` and `B070_PASSWORD`.

TLS verification is **off by default** — these devices ship a self-signed
`*.kvm.net` certificate and cannot be issued a trusted one for a bare IP.
Pass `--verify-tls` once you have installed your own.

### `b070-capture` — session capture harness

A TLS-intercepting proxy for the KVM session port. Point the stock Java client
at it instead of the KVM; it records both plaintext directions and decodes
`ITCMsg` frames live.

```sh
b070-capture --device 192.168.0.254 --listen 0.0.0.0:900 --out ./capture
```

This works because the vendor client does not verify certificates. Verified
end-to-end against a synthetic device; not yet run against real hardware.

**Captures contain credentials in cleartext.** Use only on a device you
administer.

## Protocols

| Layer | Transport | Status |
|---|---|---|
| Management | HTTPS 443, `POST /config/`, XML | **Implemented** |
| KVM session framing (`ITCMsg`) | TLS on TCP 900 | **Recovered**, unverified |
| Virtual media framing (`CommIO`) | same TLS session, USB-over-IP + SCSI | **Recovered**, unverified |
| Control commands | XML inside the session | **Recovered** (exact vendor strings) |
| Tile video codec | inside session payloads | **Unknown** — YCbCr, 8 or 16 bpp |
| Serial passthrough | SSH on TCP 4001/4002 | Documented, not implemented |

## Layout

```
crates/
  b070-proto/    ITCMsg + CommIO framing, XML command vocabulary
  b070-mgmt/     HTTPS management client and XML models
  b070-cli/      the `b070` binary
  b070-capture/  the `b070-capture` binary
research/        vendor firmware, extracted clients, disassembly, findings
```

## Likely applicable to

B070-008-19-IP, B070-016-19-IP(2), B072-008-1-IP, B072-016-1-IP, B072-016-IP2,
B072-032-IP2, and the Minicom Smart 108/116 IP originals.

## Next steps

1. On arrival: pull the device's own client JARs from
   `https://<ip>/targets.jnlp` and diff against
   `research/software/client-3.0.5.583/` — the archived firmware is from 2013
   and the unit is almost certainly newer.
2. Confirm the management API's authentication scheme (the recovered web UI
   posts no credentials, so there is a login step still unmapped).
3. Run `b070-capture` against a real session and confirm the `ITCMsg` framing,
   the three reserved header words, and what distinguishes message types 1/2/4.
4. Reverse the tile codec, then build the viewer.
5. Optionally: try running the stock Java client on Linux (needs a 32-bit JRE
   plus 32-bit X11 libs) as an interim workaround and reference.
