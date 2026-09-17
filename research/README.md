# research/

Collected vendor material and reverse-engineering artifacts for the Tripp Lite
B070-008-19-IP (NetCommander IP Cat5 KVM).

**Start with [`notes/FINDINGS.md`](notes/FINDINGS.md).** Provenance and
download URLs are in [`SOURCES.md`](SOURCES.md).

```
datasheets/    B070-008-19-IP datasheet
manuals/       Owner's manual (B070-016-19-IP2, 2018), Minicom Smart 108/116 IP
               guide (the OEM original), NetCommander-AXS documentation
patents/       US 9,417,720 notes
firmware/      Vendor firmware archives (2013-2014, HW v1) + extraction work
  extracted/     unzipped vendor archives
  work/          unpack.py (container parser), decoded device OS image,
                 out_client/ and out_config/ payloads
software/
  client-3.0.5.583/   Java Web Start client, recovered from firmware
  webui-3.0.5.583/    Device web UI (HTML/JS/XML) — documents the /config/ API
  native/             libkvmipjproxy.so (Linux i386) + kvmipjproxy.dll
  installers/         2017 Windows installers (install4j, payload not yet extracted; untracked)
decompiled/    CFR output for app.jar and boot.jar (384 sources)
notes/
  FINDINGS.md        the writeup
  analysis/          symbol + string dumps, vendor release notes
```

## Not tracked in git

Some directories here exist locally but are deliberately untracked (see the
repo's `.gitignore`), because they duplicate material that *is* tracked. The
originals are what matter; these are regenerated in seconds:

| Untracked | Regenerate with |
|---|---|
| `software/installers/` | `unzip software/*.zip` — the `.exe` files are byte-identical to the archive contents |
| `firmware/extracted/` | `for f in firmware/*.zip; do unzip -o "$f" -d firmware/extracted; done` |
| `firmware/work/out_client/`, `out_config/` | `cd firmware/work && python3 unpack.py` (curated copies are tracked under `software/client-3.0.5.583/` and `software/webui-3.0.5.583/`) |

## Reproducing the firmware extraction

```sh
cd firmware
for f in *.zip; do unzip -oq "$f" -d extracted; done
cd work
base64 -d "../extracted/B070-016-19-IP (HW v1) Firmware_9-13-14/B070-016-19-IP IP Firmware/Tripplite116IP_3_0_5_583.64b" > ip_fw.bin
python3 unpack.py          # parses the two file containers
```

`unpack.py` expects the gzip members already split out of `ip_fw.bin` as
`m1.bin` and `m2.bin`; `notes/FINDINGS.md` §3 documents the container layout.

## Licensing note

Everything under `software/`, `firmware/` and `decompiled/` is vendor
copyrighted material, retained here for interoperability research. Do not
redistribute it as part of any client that ships from this repo.
