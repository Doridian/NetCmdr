# Sources

All material collected 2026-09-16. Eaton has retired the Tripp Lite support
site — every `tripplite.eaton.com/support/*` URL now 302s to an
"Eaton acquires Tripp Lite" landing page. Files on `assets.tripplite.com` are
still served directly if you know the exact filename, and the Wayback Machine
holds the old download index.

## Live (still working)

| What | URL |
|---|---|
| Datasheet, B070-008-19-IP | https://assets.tripplite.com/product-pdfs/en/b07000819ip.pdf |
| Non-browser Windows client (2017) | https://assets.tripplite.com/software/b070-and-b072-ip-non-browser-client.zip |
| NetCommander-AXS management software (2017) | https://assets.tripplite.com/software/netcommander-axs-ip-kvm-software.zip |
| Product page | https://tripplite.eaton.com/netcommander-8-port-cat5-1u-rack-mount-1+1-user-console-kvm-switch-19-in-lcd-ip-remote-access~B07000819IP |
| Eaton SKU page | https://www.eaton.com/us/en-us/skuPage.B070-008-19-IP.html |

## Mirrors / third party

| What | URL |
|---|---|
| Owner's Manual, B070-016-19-IP2 (2018, sibling model — the best manual found) | https://bgs.cc/content/TRI-B07001619IP2%20MANUAL.pdf |
| Minicom Smart 108/116 IP User Guide (the OEM original) | https://archive.org/stream/manualzilla-id-7007279/7007279_djvu.txt |

## Wayback Machine

The old download index — this is how the firmware was found. It lists every
file under `tripplite.com/shared/software/`:

```
http://web.archive.org/web/20150503061449/http://www.tripplite.com/support/downloads/type/9/tab/2
```

2022 snapshot of the exact model's support page (source of the two live
software URLs above):

```
http://web.archive.org/web/20220119065357/https://www.tripplite.com/support/B07000819IP
```

Firmware archives retrieved (prefix each with
`http://web.archive.org/web/20150503061449id_/http://www.tripplite.com`):

- `/shared/software/Firmware/B070-016-19-IP-(HW v1)-Firmware-9-13-14.zip`
- `/shared/software/Firmware/B072-016-1-IP-(HW v1)-Firmware-9-13-14.zip`
- `/shared/software/Firmware/B070-xxx-19 Firmware.zip`
- `/shared/software/Firmware/B072-008-1 and B072-016-1 Firmware.zip`

## Patent

US 9,417,720 B2 — "Fast Virtual Media KVM System".
Inventors: Yaron Alfia, Miki Wechsler, Shlomo Bergman.
Original assignee Trippe Manufacturing Co; now Eaton Intelligent Power Ltd.
https://patents.google.com/patent/US9417720B2/en

Describes the Cat5 link to the Server Interface Unit (ROC): unidirectional
100Base-T switch→ROC on one pair, async serial ROC→switch superimposed on the
blue video common-mode voltage, RGB+sync on the remaining pairs. Relevant to
the *dongle* link, not the network protocol.

## Tools used

- `cfr` 0.152 (Java decompiler) — https://repo1.maven.org/maven2/org/benf/cfr/0.152/cfr-0.152.jar
- `binutils` (`nm`, `readelf`, `c++filt`), `7z`, `poppler` (`pdftotext`)
