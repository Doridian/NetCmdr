// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The native session: one thread driving a [`SessionCore`].
//!
//! All the protocol lives in [`crate::core`]; this is the part that only makes
//! sense with an operating system under it — real sockets, a thread to pump
//! them, and channels so other threads can reach the session.
//!
//! It used to be three threads, one per channel, each parked in a blocking
//! read. That shape could not follow the client into a browser, so the loop
//! moved into `SessionCore::pump` and what remains here is a single thread
//! calling it. The sockets are therefore **non-blocking** and the thread backs
//! off when idle rather than parking; see [`backoff`] for what that costs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use netcmdr_proto::command::{VideoFilter, VideoSetting};
use netcmdr_proto::kvmmsg::{Message, SocketType};

use crate::core::{
    outbound, Login, SessionCore, VideoSettings, HEARTBEAT_DELAY, HEARTBEAT_PERIOD, LOGIN_TIMEOUT,
};
use crate::input::Input;
use crate::tcp::{ShutdownHandle, Stream, TcpChannel};
use crate::time::{Duration, Instant};
use crate::video::{Framebuffer, SharedFrame, SharedPalette};
use crate::{Error, Event, SessionConfig};

/// How long the command channel parks in a read while waiting for the login.
///
/// This is the one place a socket still blocks: there is nothing else to do
/// until the device answers, and a deadline still has to be checked.
const LOGIN_POLL: Duration = Duration::from_millis(100);

/// Something for the session thread to send.
enum Outgoing {
    Cmd(Message),
    Input(Input),
}

/// A live KVM session.
pub struct Session {
    /// Events from the session's thread. The sender is dropped when it stops.
    pub events: Receiver<Event>,
    frame: SharedFrame,
    /// Held in an `Option` so [`Session::disconnect`] can drop the sender
    /// before joining.
    outbox: Option<Sender<Outgoing>>,
    stop: Arc<AtomicBool>,
    shutdowns: Vec<ShutdownHandle>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    /// Open a session: log in on the command channel, then bring up video and
    /// input.
    pub fn connect(cfg: SessionConfig) -> Result<Session, Error> {
        let cmd = TcpChannel::connect(
            &cfg.host,
            cfg.port,
            &cfg.session_id,
            SocketType::Cmd,
            cfg.connect_timeout,
        )?;
        cmd.set_read_timeout(Some(LOGIN_POLL))?;

        let mut login = Login::begin(cmd, &cfg)?;
        let deadline = Instant::now() + LOGIN_TIMEOUT;
        let config_xml = loop {
            if let Some(xml) = login.poll()? {
                break xml;
            }
            if Instant::now() >= deadline {
                return Err(Error::LoginTimeout);
            }
        };
        let cmd = login.finish();

        let video = TcpChannel::connect(
            &cfg.host,
            cfg.port,
            &cfg.session_id,
            SocketType::Video,
            cfg.connect_timeout,
        )?;
        let km = TcpChannel::connect(
            &cfg.host,
            cfg.port,
            &cfg.session_id,
            SocketType::KeyboardMouse,
            cfg.connect_timeout,
        )?;

        // From here the pump owns all three and must never park in one of
        // them, or a quiet video channel would stall the command channel.
        cmd.set_nonblocking(true)?;
        video.set_nonblocking(true)?;
        km.set_nonblocking(true)?;

        let shutdowns = vec![
            cmd.shutdown_handle()?,
            video.shutdown_handle()?,
            km.shutdown_handle()?,
        ];

        let frame: SharedFrame = Arc::new(Mutex::new(Framebuffer::new()));
        let palette: SharedPalette = Arc::new(Mutex::new(None));
        let (events_tx, events) = mpsc::channel();
        let (outbox_tx, outbox_rx) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));

        let core = SessionCore::new(cmd, video, km, frame.clone(), palette, config_xml);
        let thread = spawn_pump(core, outbox_rx, events_tx, stop.clone());

        Ok(Session {
            events,
            frame,
            outbox: Some(outbox_tx),
            stop,
            shutdowns,
            thread: Some(thread),
        })
    }

    /// The shared framebuffer. Lock it briefly to draw, then let go — the
    /// session thread takes the same lock to blit each frame.
    pub fn frame(&self) -> &SharedFrame {
        &self.frame
    }

    /// Queue an input event for the keyboard/mouse channel.
    pub fn send_input(&self, ev: Input) -> Result<(), Error> {
        self.queue(Outgoing::Input(ev))
    }

    /// Ask the device for a full repaint (`<command name='video_refresh'/>`).
    pub fn refresh_screen(&self) -> Result<(), Error> {
        self.queue(Outgoing::Cmd(outbound::refresh_screen()))
    }

    /// Run the device's automatic video adjustment
    /// (`<command name='auto_video_adjust' state='start'/>`).
    ///
    /// The device digitises analog VGA, so its sampling phase and horizontal
    /// position drift per target and per mode; this is what re-centres and
    /// re-sharpens the captured image. Progress arrives as [`Event::Gui`].
    pub fn auto_adjust_video(&self) -> Result<(), Error> {
        self.queue(Outgoing::Cmd(outbound::auto_adjust_video()))
    }

    /// Stop an adjustment in progress.
    pub fn stop_auto_adjust(&self) -> Result<(), Error> {
        self.queue(Outgoing::Cmd(outbound::stop_auto_adjust()))
    }

    /// Set one analog video knob by hand, for this session
    /// (`<video_settings hoffset='364'/>` and friends).
    ///
    /// The change applies immediately and lasts until logout; the device does
    /// not announce it and goes on reporting its stored figure, so the caller
    /// keeps its own copy of the value. Because the comparison is against that
    /// stored figure, a set *equal* to it is silently dropped.
    ///
    /// [`Session::store_video_settings`] is the one that lasts — and the one to
    /// use when the value being set may be the stored one.
    pub fn set_video_setting(&self, setting: VideoSetting, value: &str) -> Result<(), Error> {
        self.queue(Outgoing::Cmd(outbound::set_video_setting(setting, value)))
    }

    /// Set a numeric video knob. See [`Session::set_video_setting`].
    pub fn set_video_number(&self, setting: VideoSetting, value: i32) -> Result<(), Error> {
        self.set_video_setting(setting, &value.to_string())
    }

    /// Write video knobs into the device's stored configuration.
    ///
    /// Unlike [`Session::set_video_setting`] this survives logout, a target
    /// switch and a reconnect, because the device stores it rather than
    /// overlaying it — which it does for any `<video_settings>` element
    /// carrying both gains. `current` is the device's own last report
    /// ([`Event::ConfigXml`] → [`crate::parse_video_settings`]); the gains are
    /// taken from it, so a caller moving a capture offset need not think about
    /// the rule.
    ///
    /// The device answers with a fresh [`Event::ConfigXml`] carrying the new
    /// values, which is the only acknowledgement there is. Nothing comes back
    /// when it declines.
    pub fn store_video_settings(
        &self,
        current: &VideoSettings,
        changes: &[(VideoSetting, i32)],
    ) -> Result<(), Error> {
        self.queue(Outgoing::Cmd(outbound::store_video_settings(
            current, changes,
        )))
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
        self.queue(Outgoing::Cmd(outbound::change_target(name, port)))
    }

    /// Run the device's mouse calibration
    /// (`<command name='calibrate' state='start' mode='calibrate'/>`).
    ///
    /// The device moves the remote pointer around a known path to learn the
    /// target's acceleration curve. Progress arrives as [`Event::Gui`].
    pub fn calibrate_mouse(&self) -> Result<(), Error> {
        self.queue(Outgoing::Cmd(outbound::calibrate_mouse()))
    }

    /// Align the remote pointer with the local one
    /// (`<command name='align' state='start' mode='align'/>`).
    pub fn align_mouse(&self) -> Result<(), Error> {
        self.queue(Outgoing::Cmd(outbound::align_mouse()))
    }

    /// Claim or release exclusive use of the target mid-session.
    pub fn set_exclusive(&self, exclusive: bool) -> Result<(), Error> {
        self.queue(Outgoing::Cmd(outbound::set_exclusive(exclusive)))
    }

    /// Send an app-level `<commands>` document on the command channel.
    pub fn send_xml(&self, xml: &str) -> Result<(), Error> {
        self.queue(Outgoing::Cmd(outbound::xml(xml)))
    }

    /// Send a `<command name='config'>` document (`ConfigCommandHandler`).
    pub fn send_config_xml(&self, xml: &str) -> Result<(), Error> {
        self.queue(Outgoing::Cmd(outbound::config_xml(xml)))
    }

    fn queue(&self, item: Outgoing) -> Result<(), Error> {
        self.outbox
            .as_ref()
            .ok_or(Error::Closed)?
            .send(item)
            .map_err(|_| Error::Closed)
    }

    /// Stop the session and wait for its thread.
    ///
    /// Order matters: the sender goes first so the pump sees its outbox close,
    /// then the sockets are dropped to break any read in progress, and only
    /// then can the join complete.
    pub fn disconnect(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.outbox.take();
        for s in &self.shutdowns {
            s.shutdown();
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.disconnect();
    }
}

/// Run the session until it ends or is stopped.
fn spawn_pump(
    mut core: SessionCore<Stream>,
    outbox: Receiver<Outgoing>,
    events: Sender<Event>,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut next_beat = Instant::now() + HEARTBEAT_DELAY;
        let mut idle = 0u32;

        loop {
            if stop.load(Ordering::SeqCst) {
                events
                    .send(Event::Disconnected("closed locally".into()))
                    .ok();
                return;
            }

            let mut moved = core.pump();

            while let Some(event) = core.next_event() {
                let ended = matches!(event, Event::Disconnected(_));
                if events.send(event).is_err() || ended {
                    return;
                }
            }

            while let Ok(item) = outbox.try_recv() {
                moved = true;
                let sent = match item {
                    Outgoing::Cmd(msg) => core.send(&msg),
                    Outgoing::Input(ev) => core.send_input(ev),
                };
                if let Err(e) = sent {
                    tracing::warn!(error = %e, "send failed");
                }
            }

            if Instant::now() >= next_beat {
                next_beat = Instant::now() + HEARTBEAT_PERIOD;
                if let Err(e) = core.send_heartbeat() {
                    events
                        .send(Event::Disconnected(format!("heartbeat failed: {e}")))
                        .ok();
                    return;
                }
            }

            if moved {
                idle = 0;
            } else {
                idle = idle.saturating_add(1);
                std::thread::sleep(backoff(idle));
            }
        }
    })
}

/// How long to sleep after `idle` pumps with nothing to do.
///
/// Replacing three blocking readers with one poller trades a little idle CPU
/// for a shape that also runs in a browser. The trade is bounded: while video
/// is arriving the loop never reaches the later bands, so the latency that
/// matters — a keystroke while the screen is busy — is never paid. A session
/// watching a blank screensaver settles at 125 wakeups a second.
fn backoff(idle: u32) -> Duration {
    match idle {
        0..=64 => Duration::from_micros(250),
        65..=256 => Duration::from_millis(2),
        _ => Duration::from_millis(8),
    }
}
