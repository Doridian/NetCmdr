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

### 5.0 Hardware-verified update (2026-09-19) — READ THIS FIRST

A live B070-008-19-IP (firmware **2.2.1258.1.0**, hardware family `108`, 8
ports) was analysed on 2026-09-18 and re-verified on 2026-09-19 after a
firmware update from 2.1.1184. Its own client and behaviour **supersede much of
the reverse engineering below**, which was done against 2013 firmware and the
native library `libkvmipjproxy.so`. Where they disagree, this section wins.

The 2.1.1184 → 2.2.1258.1.0 update changed **only the TLS parameters** (point 2)
and the client's version string (`J-9193` → `10106`). Decompiling the new
`app.jar` and diffing it against the old one gives 40 differing files, all of
which are recompilation artifacts (string-concatenation folding, generic casts)
except the version constants and a gutted DES licence check
(`CheckLicense.isValid()` now returns `true` unconditionally). `KVMSocket`,
`SocketUtils`, `HttpCmd` and `SSLContextHandler` are unchanged, so **the wire
protocol and the session model are identical**. The bundled XStream went 1.4.3 →
1.4.10 and `targets.jnlp` gained `--add-opens` flags for JDK 9+.

Key corrections:

1. **The current client is pure Java.** The 2017 firmware dropped the
   `kvmipjproxy` native library and reimplemented the entire protocol in Java
   (`com.minicom.plugin.*`). The client is served from the device at
   `https://<ip>/targets.jnlp`; the JARs and 594 decompiled sources are under
   `research/software/client-live/` and
   `research/decompiled/app-live/`. `plugin.jar` now ships only virtual-media
   helpers (`libvmproxy.so`, `usb_adapter.dll`), not the KVM proxy.

2. **TLS on 2.2.1258.1.0 is 1.2** (1.0 and 1.1 are still accepted; there is no
   1.3). Port 443 prefers `DHE-RSA-AES256-GCM-SHA384`; the session sockets on
   port 900 negotiate `AES256-GCM-SHA384` (static RSA). The certificate is now
   self-signed **SHA-256** RSA-2048 with `CN=kvm-rack` — the device hostname,
   where 2.1.1184 used the serial (`D1214403`) and SHA-1. It carries a critical
   SAN with the device's IPv4 and IPv6 addresses.

   **rustls still cannot talk to this device**: it offers no ECDHE suite at all
   (only DHE and static RSA) and no TLS 1.3, and rustls implements neither DHE
   nor static RSA key exchange. So the Rust code stays on OpenSSL. The only
   remaining concession is `SECLEVEL=1`, because the DHE group is 1024-bit and
   OpenSSL's default level 2 requires 2048; level 1 keeps forward secrecy, where
   excluding DHE to fall back on static RSA would not. Verification stays off —
   the cert is self-signed, and the client itself verifies nothing either
   (`SocketUtils` installs a trust-all `X509TrustManager`, and
   `SSLContextHandler` a `DefaultTrustManager`; neither pins a version, so the
   vendor client simply follows whatever the JRE negotiates).

3. **Management is `POST /message.pv`, not `/config/`.** The old `/config/`
   `client=*XML` endpoint (§5.2) is **404** on current firmware. See §5.2 for
   the replacement, which is implemented and verified in `b070-mgmt`.

4. **The session message is `KVMMsg`, authoritative in §5.4** — it replaces the
   `ITCMsg`/`CommIO` framing that was guessed from the disassembly. The 32-byte
   header's fields are now known exactly (they are *not* the "reserved" words
   the RE assumed): server/client timestamps, `cmd_flag`, `command`, `id`,
   `key_frame`. Full enums (`MagicNumber`, `CmdFlag`, `Cmd`, `SocketType`) are
   transcribed in `crates/b070-proto/src/kvmmsg.rs`.

5. **The session is four sockets, not one.** `SocketType` = cmd(0), kbmouse(1),
   video(2), virtualmedia(3). Each opens with a **plaintext** handshake — write
   `"<sessionId>#"`, then the 4-byte socket-type code, read it echoed back —
   and only cmd and kbmouse then upgrade to TLS. **Video and virtual media run
   unencrypted.** Virtual media is USB-over-IP carried in `KVMMsg` messages
   (`Cmd.CLIENT_USB_MEDIA` / `TERMINATE_USB_MEDIA`), so the separate `CommIO`
   framing is gone.

6. **Video codec is LZW + a tile bitfield**, with RGB555, palette or YCbCr
   pixels (`com.minicom.plugin.{LZW,ycbcr,video_impl}`), pure Java — reversible
   without touching native code. **Now ported and verified against the device**;
   see §5.4.1, which also covers keyboard and mouse. The live unit sends RGB555
   over LZW.

Everything in §5.1, §5.3 and the parts of §5.4 describing `ITCMsg`/`CommIO`
below is retained as the historical 2013 picture.

### 5.1 Session channel — TCP 900 (default)

- TLS, OpenSSL, `AES256-SHA`. Configurable 800–65535; `config.xml` shows
  `ip_port="900 , 901 , 902"` (three ports). Web UI JS defaults to `902`.
- Payload compressed with zlib.
- Carries video tiles, keyboard/mouse, virtual media, serial TTY.
- Framing is **recovered** — see 5.4. Remaining unknowns there are three
  reserved header words and the tile codec inside the payload. Confirm by
  capture: the client does not pin certificates (it ships a `*.kvm.net`
  self-signed cert), so a MITM proxy is straightforward. Note that such a proxy
  must speak OpenSSL, not rustls — see §5.0 point 2.

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

HTTPS only, **TLS 1.2** (`DHE-RSA-AES256-GCM-SHA384`) on firmware 2.2.1258.1.0;
2.1.1184 was TLS 1.0 / `DHE-RSA-AES256-SHA`. Server is `Embedthis-Appweb/2.4.4`.

**Current firmware (verified live):** one endpoint, `POST /message.pv`, body is a
`CMA_Message` XML document, response is a `PV_*` XML document (`Content-Type:
text/plain`, `Content-Length`-framed). This is implemented in `b070-mgmt` and
run against the device; captured request/response fixtures are in
`research/notes/analysis/live-fixtures/`.

Three verbs (`name=` on the `CMA_Message`):

```xml
<!-- log in: returns SESSIONID used by every later request -->
<CMA_Message name="CMA_AUTHORIZE"><user name="admin" password="access"/></CMA_Message>
<!-- read: value = config | status | switch -->
<CMA_Message name="CMA_REQUEST" SESSIONID="…"><request_data value="config" type="full"/></CMA_Message>
<CMA_Message name="CMA_REQUEST" SESSIONID="…"><request_data value="status" type="full" changetime="0"/></CMA_Message>
<!-- act -->
<CMA_Message name="CMA_ACTION" SESSIONID="…"><action type="reboot"/></CMA_Message>
<CMA_Message name="CMA_ACTION" SESSIONID="…"><action type="power" command="cycle|up|down" port="N"/></CMA_Message>
<!-- log out: CMA_AUTHORIZE with a SESSIONID and no user element -->
```

The auth reply carries the session token and device facts:

```xml
<PV_reply status="ok" hardware="108" hasFastVM="true" hasGigaEthernet="true"
          SESSIONID="…" SESSION_AAA_TYPE="3" port="900">
  <reply_error description="no error" id="1000"/>
</PV_reply>
```

`SESSIONID` is the **same token** the session sockets' plaintext preamble needs
(§5.4). `reply_error` id `1000` = success; `32004` = access denied; "Invalid
session" means re-authenticate. Full request grammar and every `Fetch*`/`Save*`/
`*Action` command are in
`research/decompiled/app-live/com/minicom/app/config/controller/http/pv/`.
The many attribute names are XStream-mangled on the wire (underscores doubled,
etc.); `HttpUtils.replaceX*UnderlineTokens` lists every substitution.

**Legacy firmware (2013, from the old web UI, now 404):** a single ISAPI
endpoint `POST /config/` with `client=ConfigXML|AdminXML|SwitchXML|StatusXML|
LogOFF|RPMDoReboot`, responses wrapped in `CMA_Message`/`config_data`. Reference
schemas: `software/webui-3.0.5.583/config/`. Kept for sibling devices that may
still run it, but not present on the current unit.

### 5.4 Session framing

#### 5.4.0 `KVMMsg` — AUTHORITATIVE (current firmware)

Transcribed from the current client's own Java
(`com.minicom.plugin.serialization.KVMMsgHeaderEncoder` / `KVMMSgHeaderDecoder`,
`com.minicom.plugin.data.*`), implemented in `crates/b070-proto/src/kvmmsg.rs`.
Every session channel (cmd, keyboard/mouse, video, virtual media) uses this one
message.

**Socket setup.** The client opens a TCP socket per `SocketType`
(cmd=0, kbmouse=1, video=2, virtualmedia=3), and on each:

1. writes `"<sessionId>#"` as ASCII (the `SESSIONID` from the HTTP login),
2. writes the socket-type code as a 4-byte native-endian int,
3. reads 4 bytes back and checks they equal the socket-type code,
4. **only for cmd and kbmouse**, upgrades the socket to TLS
   (`SocketType.uses_tls()`); video and virtual media continue in plaintext.

Then messages flow, each a 32-byte header + `size`-byte payload:

| Off | Size | Field | Notes |
|----:|-----:|-------|-------|
| 0  | u32 | magic | `ALX_TAG`=22222 (`0x56CE`), `ALX_HEARTBEAT`=77777, `ALX_BYE_MSG`=88888 |
| 4  | u32 | size | payload length (heartbeat writes 0) |
| 8  | u32 | server_timestamp | |
| 12 | u32 | client_timestamp | |
| 16 | u16 | version | 2 |
| 18 | u16 | cmd_flag | `CmdFlag` 0–21 (KEYFRAME, PALETTE, RAW_DATA, BMP, GENERAL, …, XML=20, DISCONNECT=21) |
| 20 | u32 | command | `Cmd` (VERIFY_USER=5, CONNECT=14, CONFIG_XML=71, CLIENT_USB_MEDIA=72, …) |
| 24 | u32 | id | |
| 28 | u8  | key_frame | 1 = video keyframe |
| 29 | 3×u8 | padding | zero |

Byte order is `ByteOrder.nativeOrder()`, i.e. **little-endian** on the x86
client. The decoder skips resolving `command` when `cmd_flag == MOUSE_ACC` (that
field then holds mouse data). The heartbeat variant reuses the timestamp fields
and zeroes size/cmd_flag/command.

Login on the cmd channel is `CmdFlag.RAW_DATA` + `Cmd.VERIFY_USER` with a
semicolon-delimited payload ``MessageFormat " {0} ; {1} ; 0 ; 1 ; -1 ; {2}; {3}"``
(user, password, switch_port, exclusive). Video is **LZW + YCbCr**
(`com.minicom.plugin.LZW`, `com.minicom.plugin.ycbcr`). The full 90-value `Cmd`
enum is in the decompiled `Cmd.java`; the actively-used subset is named in
`kvmmsg.rs`.

#### 5.4.1 Video, keyboard and mouse — IMPLEMENTED AND HARDWARE-VERIFIED (2026-09-18)

Ported to `crates/b070-session/` and run against the live device: the viewer
`b070-view` shows a target's screen and passes keyboard and mouse through.
Transcribed from `com.minicom.plugin.{video_impl,LZW.decoder,ycbcr}`.

**Video is tile-based.** A `CmdFlag::BMP` message on the video channel carries a
bitfield with one bit per screen tile plus the pixels of only the changed tiles,
stacked vertically into an image `tile_cx` wide. Message layout, read straight
from the channel (the header's `size` does cover it, but the vendor's reader
never checks):

```
i16 byte-order marker, little-endian:  0x4759 "YG" -> body is big-endian
                                       0x5947 "GY" -> body is little-endian
                                       anything else IS the compress method
i16 compress method (AlxCompress)      [only when a marker was present]
i32 compressed size
i16 pixel format (AlxBmpFormat)
i32 biHeaderSize                       4 -> an i32 frame count follows,
                                       otherwise that many BITMAPINFOHEADER bytes
u8[compressed size] pixel payload
i16 tile_cx, tile_cy, source_cx, source_cy, idx_size, idx_size_comp
u8[idx_size_comp] compressed tile bitfield
```

**Decoding has four stages**, in this order:

1. *restore* — decompress the bitfield, then the pixels. Both the zlib and LZW
   modes are **streaming across messages**; only an `_INIT`/`_START` method
   restarts them. The zlib modes run the bitfield and the pixels through **one
   shared inflate stream**, so inflating them out of order desynchronises it.
   The bitfield is either raw, RLE (`0x00-0x7F` = that many + 1 literals,
   `0x80-0xFF` = repeat the next byte `(b & 0x7F) + 2` times) or zlib.
2. *post-uncompress* — palette lookup (`ALX_PACK_8`), RGB555 expansion
   (`ALX_PACK_16`) or YCbCr conversion.
3. *unpack* — blit each changed tile to its place, `tile_index / (width /
   tile_cx)` giving its row.

**Pixel order.** The framebuffer holds `0x00RRGGBB` per pixel. The vendor stores
bytes B, G, R, x and reads them back with a little-endian `getInt` into a
`TYPE_INT_RGB` image; its YCbCr converter also crosses R and B over and the
caller crosses them back, so the two swaps cancel.

**The LZW is unusual**: symbols are whole 32-bit pixels, not bytes, and the code
width grows in place from 16 through 20 bits and then to a 32-bit escape mode,
driven by in-band control words. The four hand-written per-width switch tables in
`BufData` are all one LSB-first bit reader over the 32-bit input words. Input
words are read big-endian and output written little-endian, so LZW frames arrive
byte-swapped and the YCbCr stage skips its own swap for them.

**What the live device actually sends** (firmware 2.2.1258.1.0, two targets, 1360x768
and 1024x768): `ALX_PACK_16` (RGB555) with `ALX_LZW_STREAM_TCB_COMP_START` for
the key frame then `..._CONT` for deltas, 8x8 tiles, typically 6-60 changed tiles
per message at 100-1100 bytes. The YCbCr and palette paths are transcribed and
unit-tested but **no hardware has produced one yet**; they presumably need a
different colour-depth setting.

**Keyboard and mouse** (`com.minicom.plugin.km_impl`) are 16-byte payloads of
four little-endian 32-bit words on the kbmouse channel, as `CmdFlag::RAW_DATA`:

| Class | Word 0 | Word 1 | Word 2 | Word 3 |
|---|---|---|---|---|
| Mouse (256) | 256 | type (`MouseMsgType`) | x, or wheel count | y |
| Keyboard (257) | 257 | 769 up / 771 down | `scancode << 16`, `+ 0x01000000` if extended | modifier bits |
| Settings (258) | 258 | e.g. 773 = Ctrl+Alt+Del | param 1 | param 2 |

Mouse moves — and only moves — also set the header command to `MOUSE_MOVE` (76).
Coordinates are **absolute in remote screen pixels**; button events send (0, 0)
and rely on the last move. Modifier bits are scroll lock 1, num lock 2, caps lock
4, shift 8, ctrl 16, alt 32. Scancodes are PC set 1; the vendor's table sends
Pause as 0x45 unextended and Num Lock as 0x45 **extended**.

Confirmed on hardware: typed characters echo on the target's console, and the
session survives its 30-second heartbeat.

**Video adjustment.** The device digitises analog VGA, so the captured image sits
a few pixels off horizontally until adjusted. `CmdFlag::GENERAL` + `Cmd` 64
(`VIDEO_AUTO`) starts the device's own auto-adjust, 65 stops it, and progress
comes back as `RAW_DATA` + `Cmd` 19 with an `RPMAutoAdjStruct` (two i16s then a
64-byte status string).

#### 5.4.2 `ITCMsg` / `CommIO` — LEGACY (2013 firmware, superseded)

The rest of this section describes the framing reverse-engineered from
`libkvmipjproxy.so` (2013). It is **superseded by `KVMMsg` in §5.4.0** on the
current firmware and is retained for history and for sibling devices that may
still run the old native client. Raw listings:
`notes/analysis/protocol-framing.disasm.txt`.

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
