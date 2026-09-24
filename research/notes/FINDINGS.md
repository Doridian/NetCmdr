# B070-008-19-IP — Reverse Engineering Findings

Status: **hardware-verified** on a B070-008-19-IP, firmware 2.2.1258.1.0
(2026-09-18 onward). Sections 1–4 and parts of 5 predate the hardware and were
derived from public firmware, the vendor clients and documentation; §5.0
summarises what was confirmed. Items still marked **[ASSUMED]** are unverified.

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

Recovered from the decoded firmware payload (`device-arm-firmware.bin`, the
second container's OS image — [`../README.md`](../README.md) has the extraction
steps):

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

The `ui.jnlp` template in the 2013 firmware's client requests:
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

Reproduce the symbol and string dumps with `nm -D --defined-only` piped through
`c++filt`, and `strings -a`, over `libkvmipjproxy.so`.

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
   `https://<ip>/targets.jnlp`; its JARs decompile to 594 sources with CFR (see
   [`../README.md`](../README.md)). `plugin.jar` now ships only virtual-media
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
   the replacement, which is implemented and verified in `netcmdr-mgmt` —
   including **writing** configuration (§5.2.1), which is one whole-document
   `CMA_CONFIG` between two `CMA_PERMISSION` calls, not a per-setting update.

4. **The session message is `KVMMsg`, authoritative in §5.4** — it replaces the
   `ITCMsg`/`CommIO` framing that was guessed from the disassembly. The 32-byte
   header's fields are now known exactly (they are *not* the "reserved" words
   the RE assumed): server/client timestamps, `cmd_flag`, `command`, `id`,
   `key_frame`. Full enums (`MagicNumber`, `CmdFlag`, `Cmd`, `SocketType`) are
   transcribed in `crates/netcmdr-proto/src/kvmmsg.rs`.

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

**How they reach the wire (verified live, 2026-09-19).** The all-Java client's
`CommandHandlerFactory` routes each document by its `name` attribute, and only a
handful get a dedicated encoding:

| `name` | Handler | On the wire |
|---|---|---|
| `video_refresh` | `VideoRefreshCommandHandler` | `CmdFlag::General`, `Cmd::RefreshScreen`, empty payload |
| `auto_video_adjust` | `AutoAdjustVideoCommandHandler` | `CmdFlag::General`, `Cmd::VideoAuto` (`state='end'` → `Cmd::VideoStopAdjust`) |
| `config` | `ConfigCommandHandler` | `CmdFlag::RawData`, **`Cmd::ConfigXml`**, the document as bytes |
| `mouse_pointer` | `MousePointerCommandHandler` | **nothing** — it only changes how the *client* draws its own cursor |
| `run_special_key` id 1000 | `CtrlAltDelCommandHanlder` | the keyboard channel's Ctrl+Alt+Del record, not XML |
| everything else, incl. `change_port` and `power` | `DefaultCommandHandler` | `CmdFlag::RawData`, **`Cmd::MsgCmdXml`** (77), the document as bytes |

So the `Cmd` code is *not* uniform across the vocabulary: `config` documents and
everything-else documents differ, and two entries never reach the device at all.

`change_port` is implemented in `netcmdr-session` as `Session::change_target` and
**verified against the hardware**: sending it on a live session moved the same
three channels from port 1 (1360×768) to port 2 (1024×768) and back, emitting a
new `Resolution` and a fresh client config each time, with no re-login and no
reconnect. The client config's `<server servername='…' port='…' active='…'/>`
list is what confirms the switch landed.

**The switch is not instant, and the video channel lies in the meantime.**
Timed against the device, tiles of the *previous* target keep arriving from
~90 ms after the command until the device re-points its capture:

| Transition | Old target's video continues | Settled by |
|---|---|---|
| port 1 (1360×768) → port 2 (1024×768) | 4.8 s | `Resolution` at 4.85 s, config at 4.91 s |
| port 2 → port 3 (no signal) | 9.6 s, at port 2's geometry | `NoSignal` at 9.59 s, config at 9.80 s |
| port 3 → port 1 | — | `Resolution` at 4.48 s, config at 4.54 s |

So the first frame after a `change_port` is **not** a usable "the switch is
done" signal. The usable ones, in the order they arrive, are `Resolution` (or
`NoSignal` for a dead port) and then the client config naming the new active
port. A client has to treat the gap as "picture unknown" — `netcmdr-view` blanks
the window over it. Two targets at the same resolution emit no `Resolution`
event at all, which is why the config document is needed as well.

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
text/plain`, `Content-Length`-framed). This is implemented in `netcmdr-mgmt` and
run against the device; captured request/response fixtures are in
`research/notes/analysis/live-fixtures/`.

Six verbs (`name=` on the `CMA_Message`):

```xml
<!-- log in: returns SESSIONID used by every later request -->
<CMA_Message name="CMA_AUTHORIZE"><user name="admin" password="access"/></CMA_Message>
<!-- read: value = config | status | switch -->
<CMA_Message name="CMA_REQUEST" SESSIONID="…"><request_data value="config" type="full"/></CMA_Message>
<CMA_Message name="CMA_REQUEST" SESSIONID="…"><request_data value="status" type="full" changetime="0"/></CMA_Message>
<!-- act -->
<CMA_Message name="CMA_ACTION" SESSIONID="…"><action type="reboot"/></CMA_Message>
<CMA_Message name="CMA_ACTION" SESSIONID="…"><action type="power" command="cycle|up|down" port="N"/></CMA_Message>
<!-- act: also systemlock | systemunlock | disconnect port=N | backup
     | restore preserve=B (+ <data><![CDATA[…]]></data>) | restore_from_backup
     | roc_version (+ <port number="N"/>, one port only — see 5.2.2) -->
<!-- take / release exclusive write access -->
<CMA_Message name="CMA_PERMISSION" SESSIONID="…"><permission level="config|normal"/></CMA_Message>
<!-- write: the whole configuration, see 5.2.1 -->
<CMA_Message name="CMA_CONFIG" SESSIONID="…"><config_data config_id="…" value="">…</config_data></CMA_Message>
<!-- upload a file -->
<CMA_Message name="CMA_POST" SESSIONID="…"><post_data value="upgrade"><![CDATA[…]]></post_data></CMA_Message>
<CMA_Message name="CMA_POST" SESSIONID="…"><post_data value="roc" ports="1,2"><![CDATA[…]]></post_data></CMA_Message>
<CMA_Message name="CMA_POST" SESSIONID="…"><post_data value="ssl_certificate"><private password="…"><![CDATA[…]]></private><public><![CDATA[…]]></public></post_data></CMA_Message>
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
`*Action` command are in the client's
`com.minicom.app.config.controller.http.pv` package.
The many attribute names are XStream-mangled on the wire (underscores doubled,
etc.); `HttpUtils.replaceX*UnderlineTokens` lists every substitution.

#### 5.2.1 Writing configuration — IMPLEMENTED AND HARDWARE-VERIFIED (2026-09-19)

There is **no partial write**. `CMA_CONFIG` takes the entire `<config_data>`
document and applies all of it, so changing one setting means fetching the
configuration, editing it, and posting the whole thing back. That is what the
vendor's client does: `FetchCfgHttpCmd` → the dialogs mutate one XStream object
graph → `SaveCfgHttpCmd` serializes the lot. Implemented in
`netcmdr-mgmt`'s `config.rs` (the lossless document) and driven by `netcmdr <area> set`.

The full sequence, from `com.ext.SetConfigCmd`:

1. `CMA_PERMISSION level="config"` — exclusive write access. One session at a
   time; `reply_error` id `32003` means someone else has it.
2. `CMA_CONFIG` with the document.
3. `CMA_PERMISSION level="normal"` — released, **unless** the save rebooted the
   device, in which case the session is already gone.

The reply is a `PV_reply`; `need_boot="true"` means the device is rebooting to
apply the change (network settings and the session TCP port do this).

Three rules a writer has to follow, all learned from the vendor's client and
confirmed on hardware:

- **`<system_time>` is a command, not a setting.** `DatePanel.saveUIToPvXml`
  blanks all six attributes unless the user actually set the clock. Echoing back
  the time that was read would set the clock to a value that is stale by the
  length of the round trip. Verified: a dozen saves with blanked fields left the
  clock free-running.
- **`<aaa>` may be absent.** The firmware omits it entirely until LDAP or RADIUS
  is configured; `FetchCfgHttpCmd.addAaaStubToDeviceResponseLinesIfNeeded`
  inserts `<aaa><ldap mode="basic" … user_attrib="cn" …/><radius realm=""
  accounting="false"/></aaa>` after `<security_policy>` when it is.
- **A user's password is write-only.** It never appears in a fetch. Setting one
  means adding `password="…"` — in the clear — to that `<user>` for that one
  save. Omitting the attribute leaves the stored password alone.

Element order inside `<proxyview>` does **not** matter: XStream emits the
sections in its own field order, which is not the order the firmware sends them
in, and the device accepts both.

The `<aaa>` schema, which the sanitized fixture does not show because the
fixture's device had none:

```xml
<aaa>
  <ldap mode="basic|user|group" search_dn="" search_passw="" search_base=""
        user_attrib="cn" acl_attrib="">
    <server host="…" port="636" proto="ldap|ldaps|tls"/>
  </ldap>
  <radius realm="" accounting="false">
    <server host="…" port="1812" acc_port="1813" secret="…"/>
  </radius>
</aaa>
```

Both server lists are `<server>` on the wire; the client's XStream cannot have
two classes on one tag, so `HttpUtils.replaceXpvMessageServerTokens` renames the
LDAP ones to `<lserver>` on the way in (they are the ones carrying `proto=`) and
back on the way out. Working on the document directly, as `netcmdr-mgmt` does,
makes that substitution unnecessary.

`security_policy/@aaa_order` is the enabled sources in priority order, as the
numeric codes from `MethodDescriptor`: `0` LDAP, `1` RADIUS, `3` local (`2`
TACACS+ and `4` none exist in `XPvReply` but no UI offers them). Local is always
present and always first. The live unit reads `3,1`.

**A side effect worth knowing:** hiding a serial port (`serial_device/@active`
= `false`) also strips that port from every account's `starget_ports`, and
showing the port again does **not** put those grants back. Measured on firmware
2.2.1258.1.0; `netcmdr serial set --visible off` names the accounts that lose it.

Secrets — the LDAP bind password, RADIUS shared secrets and the SNMP community —
are stored in the clear and come back in **every** configuration fetch, so any
admin-level account can read them.

#### 5.2.2 SIU versions — IMPLEMENTED AND HARDWARE-VERIFIED (2026-09-19)

`<action type="roc_version"/>` followed by `<port number="N"/>` asks the device
to interrogate the Server Interface Unit on a port over the Cat5 link. It
answers with a `PV_STATUS`:

```xml
<PV_status SESSIONID="…"><status_list>
<port number="1" roc_hw="20373 0.2 20372 2.0 ID00080207"
      roc_fw="RoC II USB En v4.0 C8 B225 12/03/2013"/>
</status_list></PV_status>
```

Two things the vendor's client does not tell you, both measured on 2.2.1258.1.0:

- **One port per request.** `FetchRoccVersionsHttpCmd` appends a `<port>`
  element per port and sends them in one message, but the device answers for
  the **first one only** and silently drops the rest — asking for ports 1 and 2
  together returned just port 1, though both had a `VMROCC` attached. Reading a
  whole switch means one round trip per port, about two seconds each.
- **An empty port answers `roc_hw="Not responding" roc_fw="Not responding"`**
  rather than being left out, so the absence is reported, not implied.

Note also that `FetchRoccVersionsHttpCmd.composeData` emits `"\t< action
type=\"roc_version\"/>"` — with a space after the `<`, which is not
well-formed XML. The device accepts a properly formed `<action …/>` just as
happily, so `netcmdr-mgmt` sends that; the firmware's parser is evidently lenient
about the vendor's own typo rather than matching on the literal string.

**Legacy firmware (2013, from the old web UI, now 404):** a single ISAPI
endpoint `POST /config/` with `client=ConfigXML|AdminXML|SwitchXML|StatusXML|
LogOFF|RPMDoReboot`, responses wrapped in `CMA_Message`/`config_data`. Reference
schemas are in the 2013 web UI, under `config/` in the firmware's first
container. Kept for sibling devices that may
still run it, but not present on the current unit.

#### 5.2.3 Backup and restore — HARDWARE-VERIFIED (2026-09-20)

**`<action type="backup"/>` needs the configuration permission**, exactly as a
`CMA_CONFIG` write does, and answers `reply_error` id `32004` (*access denied*)
without it — which is what `netcmdr backup` used to do, because `Client::backup`
sent the action bare. The sequence is the one §5.2.1 lists for a save:
`CMA_PERMISSION level="config"`, the action, `CMA_PERMISSION level="normal"`.
It is a read, but the device gates it like a write. `restore_from_backup` and
the factory `restore` take the same bracket in `netcmdr-mgmt`; neither has been
run against hardware, for the obvious reason, and neither can give the
permission back afterwards because the device reboots.

**The blob is encrypted.** 15,319 bytes on this unit, and it is an OpenSSL
`Salted__` header in base64 — `openssl enc` output with an unknown passphrase,
not a document. Nothing in it is greppable, so it is no route to reading or
editing stored settings (the per-video-mode capture table among them, see §5.4)
until the passphrase is recovered from firmware. The vendor's own client treats
it the same way: write it to a file, hand it back verbatim on a restore.

### 5.4 Session framing

#### 5.4.0 `KVMMsg` — AUTHORITATIVE (current firmware)

Transcribed from the current client's own Java
(`com.minicom.plugin.serialization.KVMMsgHeaderEncoder` / `KVMMSgHeaderDecoder`,
`com.minicom.plugin.data.*`), implemented in `crates/netcmdr-proto/src/kvmmsg.rs`.
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

Ported to `crates/netcmdr-session/` and run against the live device: the viewer
`netcmdr-view` shows a target's screen and passes keyboard and mouse through.
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
64-byte status string). What it fixes — the phase, and the
capture position too, as long as no client set is outstanding when it runs — is
measured under "Analog capture position" below.

**The "noise" meter is computed client-side from a wire field.** The stock
client's Advanced Video dialog shows a "Noise / Level" progress bar, and nothing
on the wire is named "noise": the device supplies only `idx_size`, the
changed-tile count in every video message header, and the *client* turns it into
the reading. The whole calculation is `KVMClientProxy::SetNoiseLevel(bool)` at
`0x440c0` in the 2013 native library `libkvmipjproxy.so`, called once per
decoded frame:

```
tiles   = (source_cx * source_cy) / (tile_cx * tile_cy)   // tiles on screen
sum    += idx_size * 100 / tiles                          // per frame
changes+= idx_size                                        // per frame
                                                          // once a second:
level   = min(sum / elapsed_seconds, 100)
level   = 1 if level == 0 and changes != 0                // never read silent
```

So the level is a *rate*, not an average: per-frame change percentage multiplied
by the frame rate, capped at 100. A still desktop that nevertheless reads
non-zero is the point of the meter — that is tiles flipping because the
digitiser is picking up noise on the analog VGA link, which the device's
auto-adjust settles.

The bar only moves while a client-side gate at `this+0x5bd` is set, and the
*client's own* config handler (`KVMClientProxy::Config`) sets it when it sees
`<video_settings manual_adjust_dlg='open'/>` — the command the dialog sends on
open, and `<command name='gui' active='false'/>` on close. Both go to the device
too, but the gate is purely local, so a client that wants the reading all the
time can simply always compute it. Once a second the native library formats
`noise_level='%d' changes='%d'`, wraps it as `statistics`, and pushes it up
through JNI as `OnEvent("status_change", …)`; `EventsController` matches the
substring `statistics noise_level` and routes it to the dialog's `JProgressBar`.

`crates/netcmdr-session/src/video.rs` (`NoiseMeter`) transcribes this, ungated, and
`netcmdr-view` shows it as a bar in the menu bar rather than hiding it in a dialog.

#### Analog capture position — HARDWARE-MEASURED (2026-09-19)

The two capture-position knobs in `<video_settings>` are where the device starts
digitising each frame, and they are the one part of the video path its own
automatic adjustment makes *worse* rather than better. Everything below was
measured on a live B070-008-19-IP (firmware 2.2.1258.1.0) against two targets:
port 1 driving 1360×768@60 and port 2 driving 1024×768@75.

**What the numbers mean.** `hoffset` is the hsync width plus the back porch, in
pixel clocks from the sync edge; `voffset` is the same in lines. The reported
`hoffset_min`/`hoffset_max` are exactly the mode's blanking interval, so the
mode's whole timing can be read back out of them:

```
hsync  = hoffset_min - 1
hback  = hoffset - hoffset_min + 1
hfront = hoffset_max - hoffset + 1
```

Checked against CVT/VESA timings for both modes — eight numbers, all exact. The
practical consequence is that **a correct value is always inside the range the
device declares**, which is the only cheap guard there is on a measurement.

**One unit is one pixel or one line.** Lowering a value moves the captured
picture right (or down) and brings that many lanes of blanking into view at the
leading edge, so the error counts itself:

```
correct = probe + blank_lanes(picture at probe)
```

One probe and one addition, not a search. Every probe point tried agrees:

| Target | Mode | Device default | Probes → answer |
|---|---|---|---|
| 1 | 1360×768@60 | `hoffset` 368 | 360→4, 352→12, 348→16 ⇒ **364** |
| 2 | 1024×768@75 | `hoffset` 272 | 264→6, 256→14, 248→22, 268→2 ⇒ **270** |
| 1 | 1360×768@60 | `voffset` 24 | 23→2, 20→5, 16→9, 10→15, 7→18 ⇒ **25** |

**A set whose value equals the device's own default is silently dropped.** Four
reproductions across both axes, and six spellings of the number (`0368`, ` 368`,
`368.0`, `+368`, `368 `) — so the filter is numeric and device-side. The device
compares against its *stored* config, which an overlay set does not update, so
the default value is unreachable for the rest of the session and the config
document keeps reporting the stored figure no matter what has been set. A write
that carries both gains (below) updates the stored figure itself and so has no
unreachable value.

**The automatic adjustment measures the capture position correctly — but only
from a clean session.** Run with no client set outstanding, it finds the same
`hoffset` this section measures by hand and writes it to the stored table:
368 → **364** on target 1, with `phase` 4 → 8, and a fresh login reads it back.
Run a second time from 380 it lands on 364 again, so it converges rather than
drifting. It takes ~6.5 s, sends no `gui` progress at all, and its only signal
is a single `CONFIG_XML` — which is also the acknowledgement that something was
stored.

**A client set outstanding poisons it.** With `hoffset` overlaid at 352 — a
value confirmed at the picture — the adjustment produced **380** and stored
that. The device measures against the capture window the overlay moved but
corrects against its own stored figure, and the two errors add. This is the
confound behind the earlier reading of this section, which had it *reverting*
offsets to the table: every one of those runs followed a manual set in the same
session. **Anything driving these knobs must let the session's overlay go — by
reconnecting — before asking for an adjustment.**

**A set reaches the picture in 0.5–1.6 s**, variably. Anything driving these
knobs has to watch the framebuffer rather than sleep a fixed amount.

**Blanking is indistinguishable from black content.** Same level (max luma 16
over a floor of about 10) and the same temporal noise (sd 0.07–0.26 for both,
over 20 samples). There is no metric that separates them, so a measurement can
only be *range-checked*, not validated: on port 2, whose console leaves the top
of the screen empty, the vertical probe read 41 blank rows and produced 64 — past
`voffset_max` of 31, and refused on that ground alone.

**Overlay corrections die on logout and on a target switch**, the latter
because the device re-announces its stored figures along with the new port's
config. A committed write (below) survives both.

#### What makes a video setting stick — HARDWARE-TESTED (2026-09-20)

A `<video_settings>` write is committed to the device's stored config **exactly
when the element carries both `brightness` and `contrast`.** Anything else is a
session overlay: it reaches the picture, it is forgotten at logout, and the
config document keeps reporting the stored value.

The device says which it did. A committed write is answered with a fresh
`CONFIG_XML` push carrying the new value; an overlay draws no response at all.
That push is the only acknowledgement there is, and it is worth waiting for.

Measured by writing an `hoffset` one unit at a time, dropping the session and
reading `<video_settings>` back on a fresh login (target 1, 1360×768@60,
firmware 2.2.1258.1.0):

| `<config><video_settings .../></config>` sent | Pushed back | Survives a new login |
|---|---|---|
| every attribute the device itself reports | yes | **yes** |
| `brightness contrast voffset hoffset phase filter` | yes | **yes** |
| `brightness contrast voffset hoffset phase` | yes | **yes** |
| `brightness contrast hoffset` | yes | **yes** |
| `brightness hoffset` | no | no |
| `voffset hoffset phase` | no | no |
| `hoffset phase` | no | no |
| `cx cy fq hoffset` (the mode, named) | no | no |
| `hoffset` alone | no | no |

So it is not the whole document, not the mode identity, and not the number of
attributes — it is those two gain knobs. Presumably the firmware parses the
element into a fixed record and treats a missing gain as "this is not a
configuration".

**This is what the stock client does, and why it was missed.** Its Advanced
Video dialog applies each slider live through `AdvancedVideoCmd`, which sends
one attribute — the overlay path, which is what was reverse-engineered. Its
**OK** button goes somewhere else entirely:
`AdvancedVideoDlg.saveData()` → `FetchDataWorker.saveConfiguration(XVideoSettings)`
→ `sendXConfigToDevice()`, which re-serialises the *entire* `XConfig` it fetched
at open — servers, performance, video settings, mouse settings, macros, device —
as `<commands><command name='config'><config>…</config></command></commands>`.
Both gains ride along in it, so OK commits. Cancel simply never sends it.

`ConfigModel.getConfig()` is where that document comes from: the client keeps
the last `<client_config>` the device pushed, renamed to `<config>`, and hands a
copy of it to the dialog. A client that wants to write one knob should do the
same — take the pushed config, change the attribute, send the element back with
`brightness` and `contrast` on it.

An earlier round of this experiment concluded the opposite — that nothing a
client can send reaches the stored table — because every candidate it tried was
a *signal* sent alongside a one-attribute set (`Cmd::VideoStopAdjust` 65, the
`manual_adjust_dlg='open'`/`gui active='false'` bracket, its symmetric
`manual_adjust_dlg='close'`, a graceful `disconnect`). None of those is a
commit, and that part stands: 65 is the `state='end'` half of
`auto_video_adjust`, which aborts a running adjustment, and `manual_adjust_dlg`
and `gui active` only gate what the stock client draws. The commit was never in
the signal; it was in the element.

The config backup (§5.2.3) is therefore no longer the only route to the stored
table, and does not need its passphrase recovered to correct a capture position.

`crates/netcmdr-session/src/align.rs` implements the measurement and the state
machine that drives it; `netcmdr-view --align` and the viewer's *Video → Re-align
capture* are the two front ends. A run probes with session sets and ends in one
stored write carrying every axis it touched — the answer where it has one, the
figure the device started with where it does not — so a correction outlives the
session and an abandoned run cannot leave a probe behind. Verified against the
rig on 2026-09-20: from a deliberately wrong stored 372/20 on target 1, a
verified run wrote 364/25, and a fresh login read them back.
`Session::store_video_settings` is the write; the viewer's *Save to device*
in the manual adjustment window is the same thing by hand.

#### 5.4.2 `ITCMsg` / `CommIO` — LEGACY (2013 firmware, superseded)

The rest of this section describes the framing reverse-engineered from
`libkvmipjproxy.so` (2013). It is **superseded by `KVMMsg` in §5.4.0** on the
current firmware and is retained for history and for sibling devices that may
still run the old native client. It was read off disassembly of the serialize
and deserialize routines named below.

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
   `https://<ip>/targets.jnlp`; **pull those JARs first** and diff against the 2013
   client recovered from firmware.
2. Does the newer `plugin.jar` still ship `libkvmipjproxy.so`, and is it still i386?
3. Exact session framing on TCP 900 and the tile codec.
4. ~~The `/config/` write path.~~ **Answered.** `/config/` is gone; writing is
   `CMA_CONFIG` on `/message.pv`, a whole-document replace between two
   `CMA_PERMISSION` calls — §5.2.1.
5. Whether virtual media rides the same 900 socket or a separate port (901/902).

---

## 9. Known gaps in this research

- The 2017 Windows installers (the `.exe` files inside the vendor's non-browser
  client archive) are **install4j**
  packages; their payload is neither raw deflate, zlib, nor LZMA-alone, so the
  bundled jars were not extracted. Easiest route is to run the installer under
  Wine and copy the `user/` directory out. The 2013 client recovered from
  firmware is complete and architecturally identical, so this is low priority.
- No firmware newer than 2014 is archived anywhere reachable; Eaton retired the
  Tripp Lite support pages (all `tripplite.eaton.com/support/*` now 302 to a
  landing page). `assets.tripplite.com` still serves files directly if you know
  the filename.
- `k1sys/switch.rpc` is encrypted/opaque; not investigated.
