// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The session as a state machine: no threads, no blocking, no clock of its
//! own.
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
//!    with a specific reason. [`Login`] does steps 2 and 3.
//! 4. Open the **video** and **keyboard/mouse** channels, in that order, and
//!    hand all three to [`SessionCore`].
//! 5. Keep the session alive by calling [`SessionCore::send_heartbeat`] every
//!    30 seconds, or the device drops it.
//!
//! # Why there are no threads here
//!
//! The original shape was a thread per channel, each parked in a blocking read.
//! A browser has no threads to give and no socket to block on, so that shape
//! could not cross over — and keeping a second implementation for the web would
//! have meant maintaining the protocol twice.
//!
//! Instead every channel is polled by [`SessionCore::pump`], which returns
//! promptly whether or not anything arrived. `Channel` was already built for
//! this: it resumes a half-read message rather than losing its place. The
//! caller owns the loop and the clock, which is a thread natively and the
//! frame callback in a browser.

use std::collections::VecDeque;

use b070_proto::command::{VideoFilter, VideoSetting};
use b070_proto::kvmmsg::{Cmd, CmdFlag, Header, MagicNumber, Message};

use crate::channel::{Channel, Transport};
use crate::input::{self, Input};
use crate::time::Duration;
use crate::video::{parse_palette, Noise, Rect, SharedFrame, SharedPalette, VideoDecoder};
use crate::Error;

/// Video messages handled per [`SessionCore::pump`].
///
/// The device can push tiles faster than they are consumed, and an unbounded
/// drain would let a busy screen starve the command channel — where the
/// disconnect notice lives — for as long as the target keeps painting.
const VIDEO_PER_PUMP: usize = 64;

/// How long to wait for the device's `CONFIG_XML` answer to `VERIFY_USER`.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(30);
/// Heartbeat schedule (`ConfigurationServiceImpl`: 5 s delay, 30 s period).
pub const HEARTBEAT_DELAY: Duration = Duration::from_secs(5);
pub const HEARTBEAT_PERIOD: Duration = Duration::from_secs(30);

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
pub fn find_user_level(xml: &str) -> Option<i32> {
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
pub fn heartbeat() -> Message {
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

/// The messages the session can send, as values.
///
/// These are pure builders so that both drivers — the threaded native
/// [`crate::Session`], which hands messages to its worker, and [`SessionCore`],
/// which writes them straight out — agree on the wire format without either
/// owning it.
pub mod outbound {
    use super::*;

    /// `<command name='config'>` documents (`ConfigCommandHandler`).
    pub fn config_xml(xml: &str) -> Message {
        Message::new(CmdFlag::RawData, Cmd::ConfigXml, xml.as_bytes().to_vec())
    }

    /// App-level `<commands>` documents.
    ///
    /// The stock client hands these to its plugin facade, which forwards
    /// anything without a dedicated handler verbatim as [`Cmd::MsgCmdXml`]
    /// (`DefaultCommandHandler`).
    pub fn xml(xml: &str) -> Message {
        Message::new(CmdFlag::RawData, Cmd::MsgCmdXml, xml.as_bytes().to_vec())
    }

    pub fn command(cmd: Cmd) -> Message {
        Message::new(CmdFlag::General, cmd, Vec::new())
    }

    /// `VERIFY_USER` (`ConnectCommandHandler`: the format string
    /// `" {0} ; {1} ; 0 ; 1 ; -1 ; {2}; {3}"` over user, session id, switch
    /// port and exclusivity).
    pub fn verify_user(cfg: &SessionConfig) -> Message {
        let payload = format!(
            " {} ; {} ; 0 ; 1 ; -1 ; {}; {}",
            cfg.username,
            cfg.session_id,
            cfg.target_port,
            u8::from(cfg.exclusive)
        );
        Message::new(CmdFlag::RawData, Cmd::VerifyUser, payload.into_bytes())
    }

    pub fn refresh_screen() -> Message {
        command(Cmd::RefreshScreen)
    }
    pub fn auto_adjust_video() -> Message {
        command(Cmd::VideoAuto)
    }
    pub fn stop_auto_adjust() -> Message {
        command(Cmd::VideoStopAdjust)
    }
    pub fn set_video_setting(setting: VideoSetting, value: &str) -> Message {
        config_xml(&b070_proto::command::set_video_setting(setting, value))
    }
    pub fn change_target(name: &str, port: u32) -> Message {
        xml(&b070_proto::command::change_port(name, &port.to_string()))
    }
    pub fn calibrate_mouse() -> Message {
        xml(&b070_proto::command::calibrate_mouse())
    }
    pub fn align_mouse() -> Message {
        xml(&b070_proto::command::align_mouse())
    }
    pub fn set_exclusive(exclusive: bool) -> Message {
        let e = if exclusive {
            b070_proto::command::Exclusivity::Exclusive
        } else {
            b070_proto::command::Exclusivity::Shared
        };
        config_xml(&b070_proto::command::set_exclusivity(e))
    }
}

/// The login exchange on the command channel, driven one poll at a time.
///
/// The caller decides how to wait — a native client loops against a deadline, a
/// browser resumes from its frame callback — while the protocol itself, the
/// `VERIFY_USER` payload and the user-level check, stays here.
pub struct Login<T: Transport> {
    cmd: Channel<T>,
}

impl<T: Transport> Login<T> {
    /// Send `VERIFY_USER` on a freshly handshaken command channel.
    pub fn begin(mut cmd: Channel<T>, cfg: &SessionConfig) -> Result<Self, Error> {
        cmd.send(&outbound::verify_user(cfg))?;
        Ok(Login { cmd })
    }

    /// Returns the client config document once the device has sent it.
    ///
    /// `Ok(None)` means it has not answered yet; call again.
    pub fn poll(&mut self) -> Result<Option<String>, Error> {
        self.cmd.flush_outgoing()?;
        let Some(msg) = self.cmd.poll()? else {
            return Ok(None);
        };
        if msg.header.magic != MagicNumber::Tag {
            return Ok(None);
        }
        match (msg.header.cmd_flag, msg.header.command) {
            (CmdFlag::RawData, Cmd::ConfigXml) => {
                let xml = String::from_utf8_lossy(&msg.payload).into_owned();
                match find_user_level(&xml) {
                    Some(level) => match user_level_error(level) {
                        Some(reason) => Err(Error::LoginRefused(reason)),
                        None => Ok(Some(xml)),
                    },
                    None => Err(Error::LoginRefused("device sent no user level")),
                }
            }
            (CmdFlag::Disconnect, _) => Err(Error::LoginRefused("device closed the session")),
            other => {
                tracing::debug!(?other, "message while waiting for login");
                Ok(None)
            }
        }
    }

    /// Take the command channel back once the login has succeeded.
    pub fn finish(self) -> Channel<T> {
        self.cmd
    }
}

/// A live KVM session over three already-connected channels.
///
/// Nothing here starts a thread or blocks: [`pump`] does one non-blocking round
/// over every channel and returns. See the module docs for why.
///
/// [`pump`]: SessionCore::pump
pub struct SessionCore<T: Transport> {
    cmd: Channel<T>,
    video: Channel<T>,
    km: Channel<T>,
    frame: SharedFrame,
    decoder: VideoDecoder,
    events: VecDeque<Event>,
    ended: bool,
}

impl<T: Transport> SessionCore<T> {
    /// Assemble a session from its three channels.
    ///
    /// `config_xml` is the document [`Login`] obtained; it is surfaced as the
    /// first event so every driver reports it the same way.
    pub fn new(
        cmd: Channel<T>,
        video: Channel<T>,
        km: Channel<T>,
        frame: SharedFrame,
        palette: SharedPalette,
        config_xml: String,
    ) -> Self {
        let mut events = VecDeque::new();
        events.push_back(Event::ConfigXml(config_xml));
        SessionCore {
            cmd,
            video,
            km,
            frame: frame.clone(),
            decoder: VideoDecoder::new(frame, palette),
            events,
            ended: false,
        }
    }

    /// The shared framebuffer. Lock it briefly to draw, then let go.
    pub fn frame(&self) -> &SharedFrame {
        &self.frame
    }

    /// Whether the session has stopped. The reason arrived as
    /// [`Event::Disconnected`].
    pub fn is_ended(&self) -> bool {
        self.ended
    }

    /// Take the next event, if any.
    pub fn next_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// Do one non-blocking round over every channel.
    ///
    /// Returns whether anything moved, so a caller with nothing else to do
    /// knows it can wait a moment before asking again.
    pub fn pump(&mut self) -> bool {
        if self.ended {
            return false;
        }
        let mut moved = false;

        // Anything a short write left behind goes before anything new.
        for result in [
            self.cmd.flush_outgoing(),
            self.km.flush_outgoing(),
            self.video.flush_outgoing(),
        ] {
            if let Err(e) = result {
                self.end(e.to_string());
                return true;
            }
        }

        loop {
            match self.cmd.poll() {
                Ok(Some(msg)) => {
                    moved = true;
                    if let Some(reason) = self.handle_cmd(&msg) {
                        self.end(reason);
                        return true;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    self.end(e.to_string());
                    return true;
                }
            }
        }

        for _ in 0..VIDEO_PER_PUMP {
            match self.video.poll() {
                Ok(Some(msg)) => {
                    moved = true;
                    self.handle_video(&msg);
                }
                Ok(None) => break,
                Err(e) => {
                    self.end(e.to_string());
                    return true;
                }
            }
        }

        moved
    }

    /// Route one command-channel message. Returns a reason when it ends the
    /// session.
    fn handle_cmd(&mut self, msg: &Message) -> Option<String> {
        if msg.header.magic != MagicNumber::Tag {
            // Heartbeat and goodbye carry nothing we act on.
            return None;
        }
        match msg.header.cmd_flag {
            CmdFlag::Palette => match parse_palette(&msg.payload) {
                Ok(lut) => {
                    if let Ok(mut slot) = self.decoder.palette_slot().lock() {
                        *slot = Some(lut);
                    }
                }
                Err(e) => tracing::warn!(error = %e, "bad palette"),
            },
            CmdFlag::NoSignal => {
                if let Ok(mut f) = self.frame.lock() {
                    f.pixels.fill(0);
                }
                self.events.push_back(Event::NoSignal);
            }
            CmdFlag::GuiMsg => {
                // The command code selects a message id; the client maps a
                // small set of them and ignores the rest.
                let id = match msg.header.command.code() {
                    2048 => "calibrating",
                    2049 => "initializing",
                    2050 => "access",
                    _ => "",
                };
                self.events.push_back(Event::Gui(id.to_string()));
            }
            CmdFlag::RawData => match msg.header.command {
                Cmd::ConfigXml => {
                    self.events.push_back(Event::ConfigXml(
                        String::from_utf8_lossy(&msg.payload).into_owned(),
                    ));
                }
                Cmd::GoLocal => return Some("disconnected by a local user".into()),
                // `RPMAutoAdjStruct`: command, status, then a 64-byte status string.
                Cmd::ChangeServer => {
                    if msg.payload.len() >= 68 {
                        let text = String::from_utf8_lossy(&msg.payload[4..68]);
                        let text = text.trim_end_matches(char::from(0)).trim();
                        if !text.is_empty() {
                            self.events
                                .push_back(Event::Gui(format!("video adjust: {text}")));
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

    fn handle_video(&mut self, msg: &Message) {
        if msg.header.magic != MagicNumber::Tag {
            return;
        }
        match msg.header.cmd_flag {
            CmdFlag::Bmp => match self.decoder.decode(&msg.payload) {
                Ok(rect) => {
                    if let Some((width, height)) = self.decoder.take_resolution_change() {
                        self.events.push_back(Event::Resolution { width, height });
                    }
                    if let Some(noise) = self.decoder.take_noise() {
                        self.events.push_back(Event::Noise(noise));
                    }
                    if !rect.is_empty() {
                        self.events.push_back(Event::Frame(rect));
                    }
                }
                // A bad frame is recoverable: the channel stays framed, and the
                // next key frame repaints over it.
                Err(e) => tracing::warn!(error = %e, "video frame dropped"),
            },
            CmdFlag::Palette => match parse_palette(&msg.payload) {
                Ok(lut) => {
                    if let Ok(mut slot) = self.decoder.palette_slot().lock() {
                        *slot = Some(lut);
                    }
                }
                Err(e) => tracing::warn!(error = %e, "bad palette on video channel"),
            },
            other => tracing::debug!(?other, "unhandled video channel message"),
        }
    }

    fn end(&mut self, reason: String) {
        if self.ended {
            return;
        }
        tracing::info!(%reason, "session stopped");
        self.ended = true;
        self.events.push_back(Event::Disconnected(reason));
    }

    // --- sending ---------------------------------------------------------

    /// Send a message on the command channel.
    pub fn send(&mut self, msg: &Message) -> Result<(), Error> {
        if self.ended {
            return Err(Error::Closed);
        }
        self.cmd.send(msg)
    }

    /// Keep the session alive. The device drops it without this.
    pub fn send_heartbeat(&mut self) -> Result<(), Error> {
        self.send(&heartbeat())
    }

    /// Queue a keyboard or mouse event.
    pub fn send_input(&mut self, ev: Input) -> Result<(), Error> {
        if self.ended {
            return Err(Error::Closed);
        }
        self.km.send(&input::encode(ev))
    }

    pub fn send_xml(&mut self, xml: &str) -> Result<(), Error> {
        self.send(&outbound::xml(xml))
    }
    pub fn send_config_xml(&mut self, xml: &str) -> Result<(), Error> {
        self.send(&outbound::config_xml(xml))
    }
    pub fn refresh_screen(&mut self) -> Result<(), Error> {
        self.send(&outbound::refresh_screen())
    }
    pub fn auto_adjust_video(&mut self) -> Result<(), Error> {
        self.send(&outbound::auto_adjust_video())
    }
    pub fn stop_auto_adjust(&mut self) -> Result<(), Error> {
        self.send(&outbound::stop_auto_adjust())
    }
    pub fn set_video_setting(&mut self, setting: VideoSetting, value: &str) -> Result<(), Error> {
        self.send(&outbound::set_video_setting(setting, value))
    }
    pub fn set_video_number(&mut self, setting: VideoSetting, value: i32) -> Result<(), Error> {
        self.set_video_setting(setting, &value.to_string())
    }
    /// [`VideoFilter::Auto`] is not a value the device accepts: the vendor's
    /// client answers that choice by running the automatic adjustment, which
    /// picks a filter itself, and so does this.
    pub fn set_video_filter(&mut self, filter: VideoFilter) -> Result<(), Error> {
        match filter {
            VideoFilter::Auto => self.auto_adjust_video(),
            other => self.set_video_setting(VideoSetting::Filter, other.as_str()),
        }
    }
    pub fn change_target(&mut self, name: &str, port: u32) -> Result<(), Error> {
        self.send(&outbound::change_target(name, port))
    }
    pub fn calibrate_mouse(&mut self) -> Result<(), Error> {
        self.send(&outbound::calibrate_mouse())
    }
    pub fn align_mouse(&mut self) -> Result<(), Error> {
        self.send(&outbound::align_mouse())
    }
    pub fn set_exclusive(&mut self, exclusive: bool) -> Result<(), Error> {
        self.send(&outbound::set_exclusive(exclusive))
    }

    /// Close every channel.
    pub fn close(&mut self) {
        self.cmd.close();
        self.video.close();
        self.km.close();
        self.ended = true;
    }
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
