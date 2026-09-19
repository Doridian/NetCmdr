//! The session: opens the channels, logs in, and runs them.
//!
//! Transcribed from `com.minicom.plugin.kvm_impl.KVMFacadeImpl`,
//! `com.minicom.plugin.config_impl.{ConnectCommandHandler,
//! ConfigurationServiceImpl,KeepAliveTask}` and
//! `com.minicom.app.targets.ui.frame.TargetsTable`.
//!
//! # Connecting
//!
//! 1. Log in over HTTPS first (see `b070-mgmt`) and keep the `SESSIONID`.
//! 2. Open the **command** channel and send `VERIFY_USER`, whose payload is the
//!    username and that same session id — the device treats the session token as
//!    the password on this channel, which is why no real password is needed
//!    here.
//! 3. The device answers with a `CONFIG_XML` document carrying `userlevel`. Only
//!    admin and user levels may open a session; every other level is a refusal
//!    with a specific reason.
//! 4. Open the **video** and **keyboard/mouse** channels, in that order.
//! 5. Keep the session alive with a heartbeat on the command channel every 30
//!    seconds, or the device drops it.
//!
//! Each channel then runs on its own thread: the command channel both reads and
//! writes (so it polls with a short timeout and interleaves sends), video reads
//! and decodes, and keyboard/mouse only ever writes — the device sends nothing
//! back on it.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use b070_proto::command::{VideoFilter, VideoSetting};
use b070_proto::kvmmsg::{Cmd, CmdFlag, Header, MagicNumber, Message, SocketType};

use crate::channel::{Channel, ShutdownHandle};
use crate::input::{self, Input};
use crate::video::{
    parse_palette, Framebuffer, Noise, Rect, SharedFrame, SharedPalette, VideoDecoder,
};
use crate::Error;

/// How long to wait for the device's `CONFIG_XML` answer to `VERIFY_USER`.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(30);
/// Heartbeat schedule (`ConfigurationServiceImpl`: 5 s delay, 30 s period).
const HEARTBEAT_DELAY: Duration = Duration::from_secs(5);
const HEARTBEAT_PERIOD: Duration = Duration::from_secs(30);
/// How long the command channel blocks in a read before checking its outbox.
const CMD_POLL: Duration = Duration::from_millis(100);

/// What to connect to.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub host: String,
    /// Session port; the login reply advertises it, normally 900.
    pub port: u16,
    /// `SESSIONID` from the HTTPS login.
    pub session_id: String,
    pub username: String,
    /// Switch port (target) to view, 1-based.
    pub target_port: u32,
    /// Ask for an exclusive session, locking other users out.
    pub exclusive: bool,
    pub connect_timeout: Duration,
}

impl SessionConfig {
    pub fn new(
        host: impl Into<String>,
        port: u16,
        session_id: impl Into<String>,
        username: impl Into<String>,
        target_port: u32,
    ) -> Self {
        SessionConfig {
            host: host.into(),
            port,
            session_id: session_id.into(),
            username: username.into(),
            target_port,
            exclusive: true,
            connect_timeout: Duration::from_secs(20),
        }
    }
}

/// Something that happened on the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// The remote resolution changed; the framebuffer has been resized.
    Resolution { width: u16, height: u16 },
    /// A frame was decoded; the rectangle is what changed.
    Frame(Rect),
    /// The device reports no video signal from the target.
    NoSignal,
    /// How much of the picture is changing, reported once a second while
    /// frames are arriving. See [`Noise`].
    Noise(Noise),
    /// An on-screen message from the device, as a message id such as
    /// `detecting_signal` or `no_signal`.
    Gui(String),
    /// The device's client configuration document.
    ConfigXml(String),
    /// The session ended. Carries the device's reason where it gave one.
    Disconnected(String),
}

/// User levels (`UserLevel`), as reported by the `connection` element.
fn user_level_error(level: i32) -> Option<&'static str> {
    match level {
        1 | 2 => None, // admin, user
        0 => Some("login failed"),
        3 => Some("viewer level cannot open a session"),
        4 => Some("another user holds an exclusive session"),
        5 => Some("no targets are assigned to this user"),
        6 => Some("device is in LDAP mode"),
        7 => Some("device reported a connection error"),
        _ => Some("device refused the login"),
    }
}

/// A live KVM session.
pub struct Session {
    /// Events from the session's threads. The sender is dropped when they stop.
    pub events: Receiver<Event>,
    frame: SharedFrame,
    /// Held in an `Option` so [`Session::disconnect`] can drop the senders
    /// *before* joining: the writer threads park on their channel, and closing
    /// it is what wakes them.
    cmd_tx: Option<Sender<Message>>,
    input_tx: Option<Sender<Message>>,
    stop: Arc<AtomicBool>,
    shutdowns: Vec<ShutdownHandle>,
    threads: Vec<JoinHandle<()>>,
}

impl Session {
    /// Open a session: log in on the command channel, then bring up video and
    /// input.
    pub fn connect(cfg: SessionConfig) -> Result<Session, Error> {
        let mut cmd = Channel::connect(
            &cfg.host,
            cfg.port,
            &cfg.session_id,
            SocketType::Cmd,
            cfg.connect_timeout,
        )?;

        // `ConnectCommandHandler`: MessageFormat " {0} ; {1} ; 0 ; 1 ; -1 ; {2}; {3}"
        // with (user, session id, switch port, exclusive).
        let verify = format!(
            " {} ; {} ; 0 ; 1 ; -1 ; {}; {}",
            cfg.username,
            cfg.session_id,
            cfg.target_port,
            u8::from(cfg.exclusive)
        );
        cmd.send(&Message::new(
            CmdFlag::RawData,
            Cmd::VerifyUser,
            verify.into_bytes(),
        ))?;

        let config_xml = await_login(&mut cmd)?;
        if let Some(level) = find_user_level(&config_xml) {
            if let Some(reason) = user_level_error(level) {
                return Err(Error::LoginRefused(reason));
            }
        } else {
            return Err(Error::LoginRefused("device sent no user level"));
        }

        let video = Channel::connect(
            &cfg.host,
            cfg.port,
            &cfg.session_id,
            SocketType::Video,
            cfg.connect_timeout,
        )?;
        let km = Channel::connect(
            &cfg.host,
            cfg.port,
            &cfg.session_id,
            SocketType::KeyboardMouse,
            cfg.connect_timeout,
        )?;

        // The reads below block indefinitely; shutdown closes the sockets.
        video.set_read_timeout(None)?;
        cmd.set_read_timeout(Some(CMD_POLL))?;

        let frame: SharedFrame = Arc::new(Mutex::new(Framebuffer::new()));
        let palette: SharedPalette = Arc::new(Mutex::new(None));
        let (events_tx, events) = mpsc::channel();
        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (input_tx, input_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let shutdowns = vec![
            cmd.shutdown_handle()?,
            video.shutdown_handle()?,
            km.shutdown_handle()?,
        ];

        events_tx.send(Event::ConfigXml(config_xml)).ok();

        let threads = vec![
            spawn_cmd_thread(
                cmd,
                cmd_rx,
                events_tx.clone(),
                frame.clone(),
                palette.clone(),
                stop.clone(),
            ),
            spawn_video_thread(video, frame.clone(), palette, events_tx, stop.clone()),
            spawn_input_thread(km, input_rx, stop.clone()),
        ];

        Ok(Session {
            events,
            frame,
            cmd_tx: Some(cmd_tx),
            input_tx: Some(input_tx),
            stop,
            shutdowns,
            threads,
        })
    }

    /// The shared framebuffer. Lock it briefly to draw, then let go — the video
    /// thread takes the same lock to blit each frame.
    pub fn frame(&self) -> &SharedFrame {
        &self.frame
    }

    /// Queue an input event for the keyboard/mouse channel.
    pub fn send_input(&self, ev: Input) -> Result<(), Error> {
        self.input_tx
            .as_ref()
            .ok_or(Error::Closed)?
            .send(input::encode(ev))
            .map_err(|_| Error::Closed)
    }

    /// Ask the device for a full repaint (`<command name='video_refresh'/>`).
    pub fn refresh_screen(&self) -> Result<(), Error> {
        self.send_command(Cmd::RefreshScreen)
    }

    /// Run the device's automatic video adjustment
    /// (`<command name='auto_video_adjust' state='start'/>`).
    ///
    /// The device digitises analog VGA, so its sampling phase and horizontal
    /// position drift per target and per mode; this is what re-centres and
    /// re-sharpens the captured image. Progress arrives as `Event::Gui`.
    pub fn auto_adjust_video(&self) -> Result<(), Error> {
        self.send_command(Cmd::VideoAuto)
    }

    /// Stop an adjustment in progress.
    pub fn stop_auto_adjust(&self) -> Result<(), Error> {
        self.send_command(Cmd::VideoStopAdjust)
    }

    /// Set one analog video knob by hand
    /// (`<video_settings hoffset='364'/>` and friends).
    ///
    /// This is what [`Session::auto_adjust_video`] cannot do for you. On the
    /// firmware this was verified against, the automatic adjustment leaves
    /// `hoffset` exactly as the device's video-mode table set it — so where
    /// that table is wrong for the mode, as it is for 1360×768, the edge it
    /// cuts off can only be recovered here.
    ///
    /// The change applies immediately and lasts for the session; the device
    /// does not announce it, so the caller keeps its own copy of the value. It
    /// is **not** persisted: the next login reads the device's default again.
    pub fn set_video_setting(&self, setting: VideoSetting, value: &str) -> Result<(), Error> {
        self.send_config_xml(&b070_proto::command::set_video_setting(setting, value))
    }

    /// Set a numeric video knob. See [`Session::set_video_setting`].
    pub fn set_video_number(&self, setting: VideoSetting, value: i32) -> Result<(), Error> {
        self.set_video_setting(setting, &value.to_string())
    }

    /// Choose the capture's noise filter.
    ///
    /// [`VideoFilter::Auto`] is not a value the device accepts: the vendor's
    /// client answers that choice by running the automatic adjustment, which
    /// picks a filter itself, and so does this.
    pub fn set_video_filter(&self, filter: VideoFilter) -> Result<(), Error> {
        match filter {
            VideoFilter::Auto => self.auto_adjust_video(),
            other => self.set_video_setting(VideoSetting::Filter, other.as_str()),
        }
    }

    /// Switch the session to another target without tearing it down.
    ///
    /// This is what the stock client's target menu does: the device re-points
    /// the same three channels at another switch port and sends a fresh
    /// `CONFIG_XML` plus a key frame. The session id, the sockets and the
    /// heartbeat all survive, so there is no second login.
    ///
    /// The device wants the target's *name* as well as its port; it echoes the
    /// name back in the config document.
    pub fn change_target(&self, name: &str, port: u32) -> Result<(), Error> {
        self.send_xml(&b070_proto::command::change_port(name, &port.to_string()))
    }

    /// Run the device's mouse calibration
    /// (`<command name='calibrate' state='start' mode='calibrate'/>`).
    ///
    /// The device moves the remote pointer around a known path to learn the
    /// target's acceleration curve. Progress arrives as [`Event::Gui`].
    pub fn calibrate_mouse(&self) -> Result<(), Error> {
        self.send_xml(&b070_proto::command::calibrate_mouse())
    }

    /// Align the remote pointer with the local one
    /// (`<command name='align' state='start' mode='align'/>`).
    pub fn align_mouse(&self) -> Result<(), Error> {
        self.send_xml(&b070_proto::command::align_mouse())
    }

    /// Claim or release exclusive use of the target mid-session.
    ///
    /// Unlike the commands above this one is a *config* document, which the
    /// device expects under [`Cmd::ConfigXml`] rather than [`Cmd::MsgCmdXml`]
    /// (`ConfigCommandHandler` vs `DefaultCommandHandler`).
    pub fn set_exclusive(&self, exclusive: bool) -> Result<(), Error> {
        let e = if exclusive {
            b070_proto::command::Exclusivity::Exclusive
        } else {
            b070_proto::command::Exclusivity::Shared
        };
        self.send_config_xml(&b070_proto::command::set_exclusivity(e))
    }

    /// Send an app-level `<commands>` document on the command channel.
    ///
    /// The stock client hands these strings to its plugin facade, which
    /// forwards anything without a dedicated handler verbatim as
    /// [`Cmd::MsgCmdXml`] (`DefaultCommandHandler`). Use
    /// [`Session::send_config_xml`] for `<command name='config'>` documents,
    /// which take a different command code.
    pub fn send_xml(&self, xml: &str) -> Result<(), Error> {
        self.send_payload(CmdFlag::RawData, Cmd::MsgCmdXml, xml.as_bytes().to_vec())
    }

    /// Send a `<command name='config'>` document (`ConfigCommandHandler`).
    pub fn send_config_xml(&self, xml: &str) -> Result<(), Error> {
        self.send_payload(CmdFlag::RawData, Cmd::ConfigXml, xml.as_bytes().to_vec())
    }

    fn send_command(&self, cmd: Cmd) -> Result<(), Error> {
        self.send_payload(CmdFlag::General, cmd, Vec::new())
    }

    fn send_payload(&self, flag: CmdFlag, cmd: Cmd, payload: Vec<u8>) -> Result<(), Error> {
        self.cmd_tx
            .as_ref()
            .ok_or(Error::Closed)?
            .send(Message::new(flag, cmd, payload))
            .map_err(|_| Error::Closed)
    }

    /// Stop the session and wait for its threads.
    ///
    /// Order matters: the senders go first so the threads parked on them wake,
    /// then the sockets are closed to unblock the readers, and only then can the
    /// joins complete.
    pub fn disconnect(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.cmd_tx.take();
        self.input_tx.take();
        for s in &self.shutdowns {
            s.shutdown();
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// Read command-channel messages until the device sends its `CONFIG_XML`.
fn await_login(cmd: &mut Channel) -> Result<String, Error> {
    let deadline = Instant::now() + LOGIN_TIMEOUT;
    cmd.set_read_timeout(Some(CMD_POLL))?;
    while Instant::now() < deadline {
        match cmd.poll()? {
            None => continue,
            Some(msg) => {
                if msg.header.magic != MagicNumber::Tag {
                    continue;
                }
                match (msg.header.cmd_flag, msg.header.command) {
                    (CmdFlag::RawData, Cmd::ConfigXml) => {
                        return Ok(String::from_utf8_lossy(&msg.payload).into_owned());
                    }
                    (CmdFlag::Disconnect, _) => {
                        return Err(Error::LoginRefused("device closed the session"));
                    }
                    other => tracing::debug!(?other, "message while waiting for login"),
                }
            }
        }
    }
    Err(Error::LoginTimeout)
}

/// One target as the *session's* own config document describes it.
///
/// The client config the device pushes on the command channel carries a
/// `<server servername='…' port='…' active='…'/>` per switch port
/// (`com.minicom.app.video.data.xml.cfg.XServer`). This is the authoritative
/// answer to "which target am I looking at", and it updates itself after a
/// [`Session::change_target`] — no management request needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Server {
    pub port: u32,
    pub name: String,
    pub active: bool,
}

/// Pull the `<server>` list out of a client config document.
///
/// Returns an empty vector when the document has none; not every config the
/// device sends carries them, so callers keep whatever they had.
pub fn parse_servers(xml: &str) -> Vec<Server> {
    use quick_xml::events::Event as XmlEvent;
    let mut out = Vec::new();
    let mut reader = quick_xml::Reader::from_str(xml);
    loop {
        match reader.read_event() {
            Ok(XmlEvent::Start(e)) | Ok(XmlEvent::Empty(e)) => {
                if e.name().as_ref() != b"server" {
                    continue;
                }
                let (mut port, mut name, mut active) = (None, String::new(), false);
                for attr in e.attributes().flatten() {
                    let value = String::from_utf8_lossy(&attr.value).trim().to_string();
                    match attr.key.as_ref() {
                        b"port" => port = value.parse().ok(),
                        b"servername" => name = value,
                        b"active" => active = value.eq_ignore_ascii_case("true"),
                        _ => {}
                    }
                }
                if let Some(port) = port {
                    out.push(Server { port, name, active });
                }
            }
            Ok(XmlEvent::Eof) | Err(_) => return out,
            _ => {}
        }
    }
}

/// One numeric video knob, with the range the device will accept for it.
///
/// `min` and `max` are inclusive. Where the device reports no range — it gives
/// one only for the two offsets — these carry the range the vendor's own
/// dialog hardcodes for that control.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Knob {
    pub value: i32,
    pub min: i32,
    pub max: i32,
}

impl Knob {
    fn new(value: i32, min: i32, max: i32) -> Knob {
        Knob { value, min, max }
    }

    /// True when the device left no room to move — there is nothing to offer.
    pub fn is_fixed(&self) -> bool {
        self.min >= self.max
    }

    pub fn clamp(&self, value: i32) -> i32 {
        value.clamp(self.min, self.max)
    }
}

/// The analog capture settings for the mode the target is currently driving.
///
/// The device pushes these as `<video_settings …/>` inside the client config,
/// at login and again whenever it has re-derived them — after an auto-adjust,
/// or after the target changes video mode. They are *per session*: a value set
/// through [`Session::set_video_setting`] applies at once but is gone on the
/// next login, which is why the device's own defaults are what a fresh session
/// starts from.
///
/// Mirrors `com.minicom.app.video.data.xml.cfg.XVideoSettings`, minus the
/// `scale` and `blacklevel` fields, which that class carries but the vendor's
/// dialog gives no control over and this device answers with a sentinel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoSettings {
    /// The mode the device says it is digitising, and its refresh rate in Hz.
    pub width: u16,
    pub height: u16,
    pub refresh: u16,
    pub brightness: Knob,
    pub contrast: Knob,
    /// Where sampling starts on each scan line, in pixels. Lowering it moves
    /// the captured picture **right**, which is what recovers a left edge the
    /// device is cutting off.
    pub hoffset: Knob,
    pub voffset: Knob,
    /// Sampling phase within the pixel clock. The device reports no range for
    /// this one; the vendor's dialog fixes it at 0–31.
    pub phase: Knob,
    pub filter: VideoFilter,
}

/// Pull `<video_settings>` out of a client config document.
///
/// `None` when the document has none — not every config the device sends
/// carries them, and the caller should keep whatever it had.
pub fn parse_video_settings(xml: &str) -> Option<VideoSettings> {
    use quick_xml::events::Event as XmlEvent;

    let mut reader = quick_xml::Reader::from_str(xml);
    loop {
        let e = match reader.read_event() {
            Ok(XmlEvent::Start(e)) | Ok(XmlEvent::Empty(e)) => e,
            Ok(XmlEvent::Eof) | Err(_) => return None,
            _ => continue,
        };
        if e.name().as_ref() != b"video_settings" {
            continue;
        }

        let mut a: Vec<(Vec<u8>, String)> = Vec::new();
        for attr in e.attributes().flatten() {
            a.push((
                attr.key.as_ref().to_vec(),
                String::from_utf8_lossy(&attr.value).trim().to_string(),
            ));
        }
        let get = |name: &str| {
            a.iter()
                .find(|(k, _)| k == name.as_bytes())
                .map(|(_, v)| v.as_str())
        };
        let num = |name: &str| get(name).and_then(|v| v.parse::<i32>().ok());
        // A range the device declines to give is not a reason to lock the
        // control: offer a window around wherever it currently sits instead.
        let ranged = |name: &str, fallback: i32| {
            let value = num(name).unwrap_or(0);
            Knob::new(
                value,
                num(&format!("{name}_min")).unwrap_or(value - fallback),
                num(&format!("{name}_max")).unwrap_or(value + fallback),
            )
        };

        return Some(VideoSettings {
            width: num("cx").unwrap_or(0).clamp(0, u16::MAX as i32) as u16,
            height: num("cy").unwrap_or(0).clamp(0, u16::MAX as i32) as u16,
            refresh: num("fq").unwrap_or(0).clamp(0, u16::MAX as i32) as u16,
            // The vendor's sliders are fixed 0–100 and it clamps the reported
            // value into that range rather than widening the slider.
            brightness: Knob::new(num("brightness").unwrap_or(50).clamp(0, 100), 0, 100),
            contrast: Knob::new(num("contrast").unwrap_or(50).clamp(0, 100), 0, 100),
            hoffset: ranged("hoffset", 64),
            voffset: ranged("voffset", 32),
            phase: Knob::new(num("phase").unwrap_or(0).clamp(0, 31), 0, 31),
            filter: get("filter")
                .and_then(VideoFilter::parse)
                .unwrap_or(VideoFilter::Auto),
        });
    }
}

/// Pull `userlevel` out of the `connection` element of the client config.
fn find_user_level(xml: &str) -> Option<i32> {
    use quick_xml::events::Event as XmlEvent;
    let mut reader = quick_xml::Reader::from_str(xml);
    loop {
        match reader.read_event() {
            Ok(XmlEvent::Start(e)) | Ok(XmlEvent::Empty(e)) => {
                if e.name().as_ref() != b"connection" {
                    continue;
                }
                for attr in e.attributes().flatten() {
                    if attr.key.as_ref() == b"userlevel" {
                        return String::from_utf8_lossy(&attr.value).trim().parse().ok();
                    }
                }
            }
            Ok(XmlEvent::Eof) | Err(_) => return None,
            _ => {}
        }
    }
}

/// `KVMMsgFactory.createHeartbeatMessage`.
fn heartbeat() -> Message {
    Message {
        header: Header {
            magic: MagicNumber::Heartbeat,
            size: 0,
            server_timestamp: i32::MAX as u32,
            client_timestamp: 14070,
            version: b070_proto::kvmmsg::VERSION,
            cmd_flag: CmdFlag::KeyFrame,
            command: Cmd::Undefined,
            // SocketType.eGeneral
            id: u32::MAX,
            key_frame: 0,
        },
        payload: Vec::new(),
    }
}

fn spawn_cmd_thread(
    mut cmd: Channel,
    outbox: Receiver<Message>,
    events: Sender<Event>,
    frame: SharedFrame,
    palette: SharedPalette,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut next_beat = Instant::now() + HEARTBEAT_DELAY;
        let reason = loop {
            if stop.load(Ordering::SeqCst) {
                break String::from("closed locally");
            }

            match cmd.poll() {
                Ok(Some(msg)) => {
                    if let Some(reason) = handle_cmd_message(&msg, &events, &frame, &palette) {
                        break reason;
                    }
                }
                Ok(None) => {}
                Err(e) => break e.to_string(),
            }

            loop {
                match outbox.try_recv() {
                    Ok(msg) => {
                        if let Err(e) = cmd.send(&msg) {
                            tracing::warn!(error = %e, "command send failed");
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break,
                }
            }

            if Instant::now() >= next_beat {
                next_beat = Instant::now() + HEARTBEAT_PERIOD;
                if let Err(e) = cmd.send(&heartbeat()) {
                    break format!("heartbeat failed: {e}");
                }
            }
        };
        tracing::info!(%reason, "command channel stopped");
        events.send(Event::Disconnected(reason)).ok();
    })
}

/// Route one command-channel message. Returns a reason when the session ends.
fn handle_cmd_message(
    msg: &Message,
    events: &Sender<Event>,
    frame: &SharedFrame,
    palette: &SharedPalette,
) -> Option<String> {
    if msg.header.magic != MagicNumber::Tag {
        // Heartbeat and goodbye carry nothing we act on.
        return None;
    }
    match msg.header.cmd_flag {
        CmdFlag::Palette => match parse_palette(&msg.payload) {
            Ok(lut) => {
                if let Ok(mut slot) = palette.lock() {
                    *slot = Some(lut);
                }
            }
            Err(e) => tracing::warn!(error = %e, "bad palette"),
        },
        CmdFlag::NoSignal => {
            if let Ok(mut f) = frame.lock() {
                f.pixels.fill(0);
            }
            events.send(Event::NoSignal).ok();
        }
        CmdFlag::GuiMsg => {
            // The command code selects a message id; the client maps a small
            // set of them and ignores the rest.
            let id = match msg.header.command.code() {
                2048 => "calibrating",
                2049 => "initializing",
                2050 => "access",
                _ => "",
            };
            events.send(Event::Gui(id.to_string())).ok();
        }
        CmdFlag::RawData => match msg.header.command {
            Cmd::ConfigXml => {
                events
                    .send(Event::ConfigXml(
                        String::from_utf8_lossy(&msg.payload).into_owned(),
                    ))
                    .ok();
            }
            Cmd::GoLocal => return Some("disconnected by a local user".into()),
            // `RPMAutoAdjStruct`: command, status, then a 64-byte status string.
            Cmd::ChangeServer => {
                if msg.payload.len() >= 68 {
                    let text = String::from_utf8_lossy(&msg.payload[4..68]);
                    let text = text.trim_end_matches(char::from(0)).trim();
                    if !text.is_empty() {
                        events
                            .send(Event::Gui(format!("video adjust: {text}")))
                            .ok();
                    }
                }
            }
            other => tracing::debug!(?other, len = msg.payload.len(), "command channel data"),
        },
        CmdFlag::Disconnect => return Some("device disconnected the session".into()),
        other => tracing::debug!(?other, "unhandled command channel message"),
    }
    None
}

fn spawn_video_thread(
    mut video: Channel,
    frame: SharedFrame,
    palette: SharedPalette,
    events: Sender<Event>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut decoder = VideoDecoder::new(frame, palette);
        loop {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            let msg = match video.recv() {
                Ok(m) => m,
                Err(e) => {
                    if !stop.load(Ordering::SeqCst) {
                        tracing::info!(error = %e, "video channel stopped");
                    }
                    break;
                }
            };
            if msg.header.magic != MagicNumber::Tag {
                continue;
            }
            match msg.header.cmd_flag {
                CmdFlag::Bmp => match decoder.decode(&msg.payload) {
                    Ok(rect) => {
                        if let Some((width, height)) = decoder.take_resolution_change() {
                            events.send(Event::Resolution { width, height }).ok();
                        }
                        if let Some(noise) = decoder.take_noise() {
                            events.send(Event::Noise(noise)).ok();
                        }
                        if !rect.is_empty() && events.send(Event::Frame(rect)).is_err() {
                            break;
                        }
                    }
                    // A bad frame is recoverable: the channel stays framed, and
                    // the next key frame repaints over it.
                    Err(e) => tracing::warn!(error = %e, "video frame dropped"),
                },
                CmdFlag::Palette => match parse_palette(&msg.payload) {
                    Ok(lut) => {
                        if let Ok(mut slot) = decoder.palette_slot().lock() {
                            *slot = Some(lut);
                        }
                    }
                    Err(e) => tracing::warn!(error = %e, "bad palette on video channel"),
                },
                other => tracing::debug!(?other, "unhandled video channel message"),
            }
        }
    })
}

fn spawn_input_thread(
    mut km: Channel,
    inbox: Receiver<Message>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        // `recv` returns an error once the session drops its sender, which is
        // how this thread is asked to stop.
        while let Ok(msg) = inbox.recv() {
            if stop.load(Ordering::SeqCst) {
                break;
            }
            if let Err(e) = km.send(&msg) {
                tracing::warn!(error = %e, "input send failed");
                break;
            }
        }
        tracing::debug!("input channel stopped");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `<video_settings>` element exactly as a B070-008-19-IP on firmware
    /// 2.2.1258.1.0 sends it, for a target driving 1360x768.
    const REAL_VIDEO_SETTINGS: &str = r#"<client_config><video_settings \
brightness='49' contrast='50' cx='1360' cy='768' fq='60' voffset='24' \
voffset_min='7' voffset_max='26' hoffset='368' hoffset_min='113' \
hoffset_max='431' scale='-10000000' scale_min='-10000000' \
scale_max='-10000000' phase='4' filter='low'/></client_config>"#;

    #[test]
    fn reads_the_capture_settings_the_device_sends() {
        let v = parse_video_settings(REAL_VIDEO_SETTINGS).expect("a settings element");
        assert_eq!((v.width, v.height, v.refresh), (1360, 768, 60));
        assert_eq!(
            v.hoffset,
            Knob {
                value: 368,
                min: 113,
                max: 431
            }
        );
        assert_eq!(
            v.voffset,
            Knob {
                value: 24,
                min: 7,
                max: 26
            }
        );
        assert_eq!(v.brightness.value, 49);
        assert_eq!(v.contrast.value, 50);
        assert_eq!(v.filter, VideoFilter::Low);
    }

    /// The device reports no range for the phase, and the two it does report
    /// are the only ones it gives. The rest come from the vendor's dialog.
    #[test]
    fn ranges_the_device_leaves_out_come_from_the_vendors_own_limits() {
        let v = parse_video_settings(REAL_VIDEO_SETTINGS).expect("a settings element");
        assert_eq!(
            v.phase,
            Knob {
                value: 4,
                min: 0,
                max: 31
            }
        );
        assert_eq!((v.brightness.min, v.brightness.max), (0, 100));
        assert_eq!((v.contrast.min, v.contrast.max), (0, 100));
    }

    /// An offset with no range is still worth a control: it gets a window
    /// around wherever the device has it, rather than being locked.
    #[test]
    fn an_offset_without_a_range_still_gets_one() {
        let v =
            parse_video_settings("<client_config><video_settings hoffset='200'/></client_config>")
                .expect("a settings element");
        assert_eq!(
            v.hoffset,
            Knob {
                value: 200,
                min: 136,
                max: 264
            }
        );
        assert!(!v.hoffset.is_fixed());
    }

    #[test]
    fn a_config_without_video_settings_yields_none() {
        assert!(
            parse_video_settings("<client_config><connection userlevel='1'/></client_config>")
                .is_none()
        );
    }

    /// Values outside the vendor's own slider range are clamped, not widened —
    /// what its dialog does when the device reports a brightness above 100.
    #[test]
    fn out_of_range_values_are_clamped_into_the_control() {
        let v = parse_video_settings(
            "<client_config><video_settings brightness='140' phase='99'/></client_config>",
        )
        .expect("a settings element");
        assert_eq!(v.brightness.value, 100);
        assert_eq!(v.phase.value, 31);
    }

    #[test]
    fn a_fixed_knob_is_recognised_as_having_nowhere_to_go() {
        assert!(Knob {
            value: 5,
            min: 5,
            max: 5
        }
        .is_fixed());
        assert!(!Knob {
            value: 5,
            min: 4,
            max: 6
        }
        .is_fixed());
        assert_eq!(
            Knob {
                value: 5,
                min: 0,
                max: 10
            }
            .clamp(99),
            10
        );
    }

    #[test]
    fn finds_the_user_level() {
        let xml = r#"<client_config><version_info v="1"/>
            <connection userlevel="2" idletimout="0"/></client_config>"#;
        assert_eq!(find_user_level(xml), Some(2));
    }

    #[test]
    fn finds_the_servers_and_which_one_is_active() {
        let xml = r#"<client_config><servers>
            <server servername="Server 01" port="1" active="false"/>
            <server servername="Server 02" port="2" active="true"/>
            </servers></client_config>"#;
        let servers = parse_servers(xml);
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[1].name, "Server 02");
        assert!(servers[1].active);
        assert!(!servers[0].active);
    }

    #[test]
    fn a_config_without_servers_yields_none_rather_than_an_error() {
        assert!(
            parse_servers("<client_config><connection userlevel='1'/></client_config>").is_empty()
        );
        assert!(parse_servers("not xml at all").is_empty());
    }

    #[test]
    fn missing_user_level_is_none() {
        assert_eq!(
            find_user_level("<client_config><connection/></client_config>"),
            None
        );
        assert_eq!(find_user_level("not xml at all"), None);
    }

    #[test]
    fn only_admin_and_user_may_connect() {
        assert!(user_level_error(1).is_none());
        assert!(user_level_error(2).is_none());
        assert!(user_level_error(0).is_some(), "wrong name/password");
        assert!(user_level_error(3).is_some(), "viewer");
        assert!(user_level_error(4).is_some(), "exclusive session held");
    }

    #[test]
    fn the_heartbeat_matches_the_clients() {
        let b = heartbeat().header.encode();
        assert_eq!(&b[0..4], &77777i32.to_le_bytes(), "ALX_HEARTBEAT");
        assert_eq!(&b[4..8], &[0, 0, 0, 0], "no payload");
        assert_eq!(&b[8..12], &(i32::MAX as u32).to_le_bytes());
        assert_eq!(&b[12..16], &14070u32.to_le_bytes());
    }

    #[test]
    fn the_verify_user_payload_matches_the_message_format() {
        // " {0} ; {1} ; 0 ; 1 ; -1 ; {2}; {3}"
        let cfg = SessionConfig::new("h", 900, "SESS", "admin", 3);
        let s = format!(
            " {} ; {} ; 0 ; 1 ; -1 ; {}; {}",
            cfg.username,
            cfg.session_id,
            cfg.target_port,
            u8::from(cfg.exclusive)
        );
        assert_eq!(s, " admin ; SESS ; 0 ; 1 ; -1 ; 3; 1");
    }
}
