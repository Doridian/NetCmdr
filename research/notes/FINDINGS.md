# B070-008-19-IP — Reverse Engineering Findings

Status: **pre-hardware**. Everything below was derived from public firmware, the
official Windows/Java client, and vendor documentation. Nothing has been
verified against a live device yet — items marked **[ASSUMED]** need
confirmation once the KVM arrives.

---

## 1. What this device actually is

The Tripp Lite **NetCommander** IP KVM line is a rebadged **Minicom Advanced
Systems** product. Tripp Lite acquired Minicom in 2012; Eaton acquired Tripp
Lite in 2021.

Evidence:

- Client main class is `com.minicom.app.video.ui.ShowLogin`; the entire Java
  package tree is `com.minicom.*`.
- Firmware config XML self-describes as `DeviceDescription="Minicom Smart 116 IP"`.
- `environ.xml` carries the Minicom device table (`NetCommander 108/116/216/232 IP`).
- US Patent 9,417,720 ("Fast Virtual Media KVM System"), cited on the
  datasheet, lists inventors Yaron Alfia, Miki Wechsler, Shlomo Bergman
  (Minicom, Israel), originally assigned to Trippe Manufacturing.

**Practical consequence:** documentation, firmware and clients for the Minicom
Smart 108/116 IP and the Tripp Lite B072-xxx-IP models are directly applicable.

---

## 2. Device architecture

The IP module is a **Windows CE** system on an **Intel IXP425 (ARM XScale)**.

Recovered from the decoded firmware payload (`firmware/work/device-arm-firmware.bin`):

- Bootloader banner: `******* IXDP4XX System Initialization *******`, `CPU ID = IXP425 A0`
- Windows CE kernel (`NKNoKitl`), `filesys.exe`, `gwes.exe`, `device.exe`, `coredll.dll`
- IXP4xx platform drivers: `ixethaccdll.dll`, `ixcryptoaccdll.dll`, `i2cdrv.dll`, `gpiodrv.dll`

Vendor binaries — these are the server side of everything we want to talk to:

| Binary | Role |
|---|---|
| `RPMServer.exe` | main service (KVM session server) |
| `rpmconfigisapi.dll` | ISAPI handler backing the `/config/` web API |
| `KvmDriver.dll` | KVM switch control |
| `RPMKbMouse.dll` | keyboard/mouse injection |
| `fgdriverPX.dll` | video frame grabber |
| `rpmcmafilter.dll` | CMA message filter (see `CMA_Message` XML) |
| `rpmsnmp.dll` | SNMP traps |

CE filesystem layout (from the firmware update container's target paths):
`\windows\`, `\config\`, `\k1web\`, `\k1sys\`, `\Temp\`.

"K1"/"KOne" is the internal platform name (`<KOneConfig>`, `K1_CONFIG`, `k1web`).

---

## 3. Firmware container format

Fully decoded — see `firmware/work/unpack.py`.

```
Tripplite116IP_3_0_5_583.64b
  └── base64 (whole file, no wrapper)
      └── 19-byte header, then 3 concatenated gzip members
          ├── member 0 → 5-byte header + zlib → Windows CE OS image (ARM)
          ├── member 1 → container "config"     (56 files → \config\, \k1sys\, \k1web\, \windows\)
          └── member 2 → container "client.zip" (15 files → \k1web\  = the Java client)
```

Container format (little-endian):

```
u32 version (0)
u32 name_len ; char name[name_len]
u32 entry_count
repeat entry_count times:
    u32 path_len ; char path[path_len]     // backslash-separated CE path
    u32 data_len ; u8   data[data_len]
```

---

## 4. Client architecture

The client is **Java Web Start**, served by the device itself:

```
https://<kvm-ip>/targets.jnlp
```

`ui.jnlp` template (`software/client-3.0.5.583/ui.jnlp`) requests:
`boot.jar`, `app.jar`, `vendor.jar`, `miglayout15-swing.jar`, `jnlp.jar`,
`xstream-1.3.1.jar`, `xpp3_min-1.1.4c.jar`, and `plugin.jar` as a
`<nativelib>`. Main class `com.minicom.app.video.ui.ShowLogin`.

**The Java code is a thin GUI shim. All protocol work happens in native code.**

`plugin.jar` contains both:

- `kvmipjproxy.dll` — Windows, i386
- `libkvmipjproxy.so` — **Linux, i386, ELF** ← a Linux build already exists

### JNI surface (`com.minicom.app.jni.KVMIPGate`)

```
createProxy / destroyProxy
getConfig / getGlobalConfig / getStatus / getResolution / getSDF
getFrameBuffer            -> direct ByteBuffer, painted by Swing
setCommand(String xml)    -> the entire control channel
sendKeyboard / sendMouse / sendMouseEx / setMouseOrigin / enableMouseCapturing
sendTTYData               -> serial passthrough
processFocusLost / setWindowID
```

Callbacks native → Java: `OnEvent(name, data)`, `OnTTYData(byte[])`,
`OnInvalidateRect(l,t,r,b)`, `OnInvalidateRectEx(l,t,r,b, short[] tiles)`.

Video is delivered as a shared framebuffer plus **tile invalidation** lists —
i.e. a tile-based codec, not a full-frame stream.

### Native library internals

`libkvmipjproxy.so` is stripped but retains C++ dynamic symbols. Notable classes:

`KVMClientProxy`, `ClientStreamer`, `ITCStreamerInterface`, `ITCSecurityLayer`,
`ITCOpenSSLSocket` / `ITCNullSSLSocket`, `SockConn`, `StreamSocket`,
`MPGrabber`, `CursorMngr`, `RemoteManager`, plus USB endpoint classes
(`EndpointInterface`, `OUTInterfaceDescriptor`) for virtual media.

Statically linked: **OpenSSL** (cipher string `AES256-SHA`) and **zlib 1.2.5**.

Full symbol and string dumps: `notes/analysis/libkvmipjproxy.symbols.txt`,
`notes/analysis/libkvmipjproxy.strings.txt`.

---

## 5. Protocols

### 5.1 Session channel — TCP 900 (default)

- TLS, OpenSSL, `AES256-SHA`. Configurable 800–65535; `config.xml` shows
  `ip_port="900 , 901 , 902"` (three ports). Web UI JS defaults to `902`.
- Payload compressed with zlib.
- Carries video tiles, keyboard/mouse, virtual media, serial TTY.
- Framing is **recovered** — see 5.4. Remaining unknowns there are three
  reserved header words and the tile codec inside the payload. Confirm by
  capture: the client does not pin certificates (it ships a `*.kvm.net`
  self-signed cert), so a MITM proxy is straightforward — `b070-capture`
  implements one.

Control commands are XML strings handed to `setCommand()`; the native layer
translates them onto the wire. Complete vocabulary recovered from the client:

```xml
<commands><command name='connect' ip='%s' username='%s' password='%s'
    switch_port='%s' tcp_port='%s' session_exlusive='%s'/></commands>
<commands><command name='disconnect'/></commands>
<commands><command name='change_port' servername='%s' port_num='%s'/></commands>
<commands><command name='video_refresh'/></commands>
<commands><command name='auto_video_adjust' state='start'/></commands>
<commands><command name='calibrate' state='start' mode='calibrate'/></commands>
<commands><command name='align' state='start' mode='align'/></commands>
<commands><command name='mouse_pointer' type='default|dot|none'/></commands>
<commands><command name='power' power_command='%s'/></commands>
<commands><command name='mount_drive' id='%s'/></commands>
<commands><command name='mount_iso_file' iso_file_path='%s'/></commands>
<commands><command name='unmount' id='%s'/></commands>
<commands><command name='serial' type='open' external='true' port_num='%s'/></commands>
<commands><command name='serial' type='close' external='true'/></commands>
<commands><command name='run_special_key'    special_key_id='%s'/></commands>
<commands><command name='update_special_key' special_key_id='%s' macro='%s' description='%s'/></commands>
<commands><command name='delete_special_key' special_key_id='%s'/></commands>
<commands><command name='gui' active='false'/></commands>
<commands><command name='config'><config>...</config></command></commands>
```

Events pushed back (native → Java): `resolution_change`, `config_change(d)`,
`status_change(d)`, `mounted_devices`, `toolbarstatus`, `client_config`,
`global_config`. Example payload:
`<resolution_change width='%d' hight='%d' wtile='%d' htile='%d'/>` (note the
vendor's `hight` typo — preserve it).

### 5.2 Management channel — HTTPS 443

HTTPS only; plain HTTP is rejected. TLS 1.2 on current firmware.

The web UI talks to a single ISAPI endpoint:

```
POST /config/
Content-Type: application/x-www-form-urlencoded

client=ConfigXML | AdminXML | SwitchXML | StatusXML | LogOFF | RPMDoReboot
```

Responses are XML documents wrapped as:

```xml
<CMA_Message id="k1" name="K1_CONFIG" number="2000">
  <config_data config_id="..." value="update">
    <proxyview ...>
      <network_adapters>…</network_adapters>
      <connection client_https_only="0" idle_time="10" ip_port="900 , 901 , 902" .../>
      <targets><target name="Server1" port="1"/>…</targets>
      <special_keys/>  <snmp .../>  <mouse .../>  <kvm_manager .../>
    </proxyview>
  </config_data>
</CMA_Message>
```

Reference copies of the live schemas are in
`software/webui-3.0.5.583/config/`: `config.xml`, `admin.xml`, `status.xml`,
`log.xml`, `SDF.xml`.

**[ASSUMED]** The write path is not visible in the recovered JS (forms use
`.submit()` to server-side handlers). Needs confirmation on hardware.

### 5.4 Session framing — RECOVERED

Recovered by disassembling `libkvmipjproxy.so`; raw listings in
`notes/analysis/protocol-framing.disasm.txt`. **Not yet verified against
hardware**, but the read path's own validation rules are explicit in the code.

Two distinct framings exist. Both ride inside the TLS session.

#### `ITCMsg` — the KVM session message (video, input, control)

`ClientStreamer::SendMsg(ITCMsg*, bool)` / `WaitNextMsg(ITCMsg*)` move these.
`ITCMsg::Serialize(BaseStream&, bool)` @ `0x5e010` writes a **fixed 32-byte
header** (from object offset `+0x4C`) followed by the payload:

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0  | u32 | **magic** | always `0x000056CE`; read path rejects anything else |
| 4  | u32 | **payload_size** | must be non-zero on read |
| 8  | u32 | (unknown) | zeroed by the constructor |
| 12 | u32 | (unknown) | zeroed by the constructor |
| 16 | u16 | **version** | always `2`; read path rejects anything else |
| 18 | u16 | **msg_type** | must be 1, 2, 3 or 4 |
| 20 | u32 | param A | `ITCMsg::Init` arg 2 |
| 24 | u32 | param B | `ITCMsg::Init` arg 3 |
| 28 | u32 | (unknown) | |

Then `payload_size` bytes of payload.

`msg_type` semantics from the serializer's switch:

- **1, 2, 4** — payload is written/read as a raw byte blob.
- **3** — payload is an `ITCObject` serialized through a virtual call
  (`ITCMsg::SerializeXML` exists alongside `Serialize`, so type 3 is very
  likely the XML control channel carrying the `<commands>` documents).

Receiver validation order, exactly as implemented:
read 32 bytes → `version == 2` → `magic == 0x56CE` → `msg_type in 1..=4` →
`payload_size != 0` → resize buffer → read `payload_size` bytes.

**[ASSUMED]** Endianness is little-endian (x86 client, and the CommIO length
field below is explicitly little-endian). The two unknown u32s at offsets 8 and
12, plus the one at 28, are zero-initialised and never written by the paths
examined — likely sequence/timestamp fields set elsewhere, or reserved.

#### `CommIO` — the virtual-media / USB-over-IP channel

`CommIO::sendPacket(u8, u8, const u8*, u32)` @ `0x843dc` writes a **4-byte
header**, then the payload:

| Offset | Size | Field |
|---|---|---|
| 0 | u8  | field A (arg 1 — channel/type) |
| 1 | u8  | field B (arg 2 — opcode; this is the byte `readPacket` hands back) |
| 2 | u16 | **length, little-endian** (`hdr[2] | hdr[3]<<8`) |

`sendPacket` throws `cException(9)` if `length >> 16` is non-zero, so payloads
are capped at 65535 bytes. With a NULL data pointer the length field is written
as zero. `CommIO::readPacket(u8& out_opcode, buf&, u32 max)` requires the 4-byte
read to return exactly 4, rejects `length >= max`, then reads the payload.

This layer carries `RemoteINEndpoint` / `RemoteOUTEndpoint` traffic and
`MassStorageInterface::onPacket`, i.e. **virtual media is USB-over-IP with a
SCSI command handler** (`SCSICommandHandler`,
`MassStorageCommandHandlerInterface`, `RPMUSBMediaDevice`).

#### Other recovered facts

- `YCbCrColorModeToBPP(int)` @ `0x641de` is literally
  `return (mode != 10) * 8 + 8;` — mode 10 → 8 bpp, every other mode → 16 bpp.
  **Video is YCbCr**, not RGB, at 8 or 16 bits per pixel.
- Stream primitives exist for both endiannesses of fixed-width ints:
  `basicInput::streamReadUint8/16/32/64`, `basicOutput::streamWriteUint8/16/32/64`,
  plus `streamRead/WriteRemoteAddress`.
- `ITCStreamerInterface::RTTMonitor` implements a **round-trip-time heartbeat**
  (`SendHartBeat`, `Respond`, `CheckResponse`) — the client is expected to keep
  the session alive.
- Compression helpers: `zlib_buffcompress`, `zlib_buffuncompress`,
  `zlib_buffcompress_stream`, `zlib_buffuncompress_stream`, `CompressStream`,
  `DecompressStream`, `PostUnCompress`, `DecompressBitField`.
- Blitters: `RPMBitBlt`, `RPMBitBlt32`, `RPMBitBltAny`, `ChangeDIBResolution`.
- `ITCSecurityLayer::CreateCertificates()` — the client generates its own
  certs; combined with the shipped `*.kvm.net` cert this confirms no meaningful
  peer verification.

### 5.5 Serial passthrough — TCP 4001 / 4002

Direct SSH to the two serial ports, if enabled in Security settings.

---

## 6. Defaults and access

- DHCP by default; falls back to static **192.168.0.254**.
- Discoverable by DHCP hostname `D` + 6-digit device number (printed under the unit).
- Firmware default account in `admin.xml`: user ID 0, password **`access`**
  (permission=1, targets=-1). Change immediately.
- Client ships a self-signed cert with subject `*.kvm.net`; certificate
  validation is effectively absent → MITM for protocol capture is easy.

---

## 7. Paths to a cross-platform client

Ordered by effort.

**A. Run the stock Java client on Linux (fastest, immediate).**
A Linux `libkvmipjproxy.so` already exists in `plugin.jar`. Requirements: a
**32-bit** JRE plus 32-bit `libX11/libXext/libXt/libstdc++`. Worth trying as
soon as the device arrives — it both gives working access and provides a
reference implementation to diff against. Limits: i386-only, X11-only (no
Wayland-native, no macOS), Java 6/8-era Web Start.

**B. Reimplement the management API (easy, high value).**
`POST /config/` with the `client=*XML` verbs is plain HTTPS + XML. A full
config/target/user/power management tool is straightforward and needs no
binary RE. Good first deliverable.

**C. Reimplement the session protocol (the real work).**
Requires recovering the framing and the tile codec from
`ClientStreamer`/`ITCStreamerInterface` in `libkvmipjproxy.so` (Ghidra/radare2),
cross-checked against a live capture. This is what a native cross-platform
viewer needs.

**D. Shim the existing native library.**
Write a small C host that `dlopen`s `libkvmipjproxy.so` and drives the JNI
entry points directly, exposing the framebuffer over a modern transport. Avoids
decoding the codec but inherits the i386/X11 constraints.

Recommended order: **B** now → **A** on arrival → capture traffic → **C**.

---

## 8. Open questions for when the hardware arrives

1. Hardware revision and firmware version. Archived firmware here is HW v1
   (`3.0.5.583`, 2013). The 2018 manual documents TLS 1.2, IPv6, RADIUS/LDAP —
   clearly newer. The device serves its own current client at
   `https://<ip>/targets.jnlp`; **pull those JARs first** and diff against
   `software/client-3.0.5.583/`.
2. Does the newer `plugin.jar` still ship `libkvmipjproxy.so`, and is it still i386?
3. Exact session framing on TCP 900 and the tile codec.
4. The `/config/` write path.
5. Whether virtual media rides the same 900 socket or a separate port (901/902).

---

## 9. Known gaps in this research

- The 2017 Windows installers (`software/installers/`) are **install4j**
  packages; their payload is neither raw deflate, zlib, nor LZMA-alone, so the
  bundled jars were not extracted. Easiest route is to run the installer under
  Wine and copy the `user/` directory out. The 2013 client recovered from
  firmware is complete and architecturally identical, so this is low priority.
- No firmware newer than 2014 is archived anywhere reachable; Eaton retired the
  Tripp Lite support pages (all `tripplite.eaton.com/support/*` now 302 to a
  landing page). `assets.tripplite.com` still serves files directly if you know
  the filename.
- `k1sys/switch.rpc` is encrypted/opaque; not investigated.
