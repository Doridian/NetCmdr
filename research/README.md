# research/

Reverse-engineering notes for the Tripp Lite B070-008-19-IP (NetCommander IP
Cat5 KVM).

**Start with [`notes/FINDINGS.md`](notes/FINDINGS.md)** — the protocol write-up
the crates are built from. [`SOURCES.md`](SOURCES.md) records where every piece
of vendor material came from, so the analysis can be redone from the originals.

```
notes/FINDINGS.md               the write-up
notes/analysis/live-fixtures/   captured /message.pv XML, sanitized
                                (netcmdr-mgmt's tests parse these)
firmware/work/unpack.py         parser for the firmware's file containers
SOURCES.md                      provenance and download URLs
```

## No vendor material is tracked here

This repository carries none of the vendor's own files: no firmware images, no
client JARs, no decompiled or disassembled output, no manuals or datasheets.
All of it is copyrighted, and it was only ever input to the analysis that
`notes/FINDINGS.md` records — the write-up is what the crates are built from,
and it is enough to work from on its own.

`.gitignore` keeps the working directories for that material (`software/`,
`decompiled/`, `firmware/*.zip`, `manuals/`, …) out of git, so a local copy can
sit here without ever being committed.

## Reproducing the firmware extraction

Fetch the firmware archives from the URLs in [`SOURCES.md`](SOURCES.md) into
`firmware/`, then:

```sh
cd firmware
for f in *.zip; do unzip -oq "$f" -d extracted; done
cd work
base64 -d "../extracted/B070-016-19-IP (HW v1) Firmware_9-13-14/B070-016-19-IP IP Firmware/Tripplite116IP_3_0_5_583.64b" > ip_fw.bin
python3 unpack.py          # parses the two file containers
```

`unpack.py` expects the gzip members already split out of `ip_fw.bin` as
`m1.bin` and `m2.bin`; `notes/FINDINGS.md` §3 documents the container layout.
It writes `out_config/` (the 2013 device web UI) and `out_client/` (the 2013
Java Web Start client, plus the `kvmipjproxy` native library inside
`plugin.jar`).

## Recovering the current client

The device serves its own client, which on current firmware is pure Java and is
the authoritative reference for the session protocol:

```sh
curl -k --ciphers DEFAULT@SECLEVEL=1 https://<device>/targets.jnlp
# then fetch each JAR the JNLP names, and decompile:
java -jar cfr.jar --outputdir decompiled/app-live app.jar
```

`SOURCES.md` lists the CFR build used.
