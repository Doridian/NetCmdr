# research/

Reverse-engineering notes for the Tripp Lite B070-008-19-IP (NetCommander IP
Cat5 KVM).

**Start with [`notes/FINDINGS.md`](notes/FINDINGS.md)**, the protocol write-up.
[`SOURCES.md`](SOURCES.md) records where the vendor material came from.

```
notes/FINDINGS.md               the write-up
notes/analysis/live-fixtures/   captured /message.pv XML, sanitized
                                (netcmdr-mgmt's tests parse these)
firmware/work/unpack.py         parser for the firmware's file containers
SOURCES.md                      provenance and download URLs
```

## No vendor material is tracked

Vendor files (firmware, JARs, decompiled output, manuals) are copyrighted and
not committed; FINDINGS.md stands on its own. `.gitignore` excludes their
working directories (`software/`, `decompiled/`, `firmware/*.zip`, `manuals/`,
…).

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

`unpack.py` expects the gzip members of `ip_fw.bin` split out as `m1.bin` and
`m2.bin` (layout in FINDINGS.md §3). It writes `out_config/` (2013 web UI) and
`out_client/` (2013 Java client, including `kvmipjproxy` in `plugin.jar`).

## Recovering the current client

The device serves its own pure-Java client, the reference for the session
protocol:

```sh
curl -k --ciphers DEFAULT@SECLEVEL=1 https://<device>/targets.jnlp
# then fetch each JAR the JNLP names, and decompile:
java -jar cfr.jar --outputdir decompiled/app-live app.jar
```

`SOURCES.md` lists the CFR build used.
