//! The session: opens the channels, logs in, and runs them.
//!
//! Transcribed from `com.minicom.plugin.kvm_impl.KVMFacadeImpl`,
//! `com.minicom.plugin.config_impl.{ConnectCommandHandler,
//! ConfigurationServiceImpl,KeepAliveTask}` and
//! `com.minicom.app.targets.ui.frame.TargetsTable`
//! (`research/decompiled/app-live/`).
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

use b070_proto::kvmmsg::{Cmd, CmdFlag, Header, MagicNumber, Message, SocketType};

use crate::channel::{Channel, ShutdownHandle};
use crate::input::{self, Input};
use crate::video::{parse_palette, Framebuffer, Rect, SharedFrame, SharedPalette, VideoDecoder};
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

    fn send_command(&self, cmd: Cmd) -> Result<(), Error> {
        self.cmd_tx
            .as_ref()
            .ok_or(Error::Closed)?
            .send(Message::new(CmdFlag::General, cmd, Vec::new()))
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

    #[test]
    fn finds_the_user_level() {
        let xml = r#"<client_config><version_info v="1"/>
            <connection userlevel="2" idletimout="0"/></client_config>"#;
        assert_eq!(find_user_level(xml), Some(2));
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
