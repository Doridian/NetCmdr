// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! The native session: one thread pumping a [`SessionCore`] over non-blocking
//! sockets, backing off when idle (see `backoff`). Protocol logic lives in
//! [`crate::core`].

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

/// Read timeout while waiting for login, the only blocking read.
const LOGIN_POLL: Duration = Duration::from_millis(100);

enum Outgoing {
    Cmd(Message),
    Input(Input),
}

pub struct Session {
    /// Closed when the session thread stops.
    pub events: Receiver<Event>,
    frame: SharedFrame,
    /// `Option` so [`Session::disconnect`] can drop it before joining.
    outbox: Option<Sender<Outgoing>>,
    stop: Arc<AtomicBool>,
    shutdowns: Vec<ShutdownHandle>,
    thread: Option<JoinHandle<()>>,
}

impl Session {
    /// Log in on the command channel, then open video and input.
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

        // Non-blocking from here, so a quiet channel can't stall the others.
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

    /// Lock briefly; the session thread blits under the same lock.
    pub fn frame(&self) -> &SharedFrame {
        &self.frame
    }

    pub fn send_input(&self, ev: Input) -> Result<(), Error> {
        self.queue(Outgoing::Input(ev))
    }

    /// Full repaint (`<command name='video_refresh'/>`).
    pub fn refresh_screen(&self) -> Result<(), Error> {
        self.queue(Outgoing::Cmd(outbound::refresh_screen()))
    }

    /// Automatic video adjustment
    /// (`<command name='auto_video_adjust' state='start'/>`). Re-centres and
    /// re-sharpens the picture; progress arrives as [`Event::Gui`].
    pub fn auto_adjust_video(&self) -> Result<(), Error> {
        self.queue(Outgoing::Cmd(outbound::auto_adjust_video()))
    }

    pub fn stop_auto_adjust(&self) -> Result<(), Error> {
        self.queue(Outgoing::Cmd(outbound::stop_auto_adjust()))
    }

    /// Set a video setting for this session only (e.g.
    /// `<video_settings hoffset='364'/>`).
    ///
    /// Not reported back, and a value equal to the stored one is silently
    /// dropped. Use [`Session::store_video_settings`] to persist.
    pub fn set_video_setting(&self, setting: VideoSetting, value: &str) -> Result<(), Error> {
        self.queue(Outgoing::Cmd(outbound::set_video_setting(setting, value)))
    }

    /// See [`Session::set_video_setting`].
    pub fn set_video_number(&self, setting: VideoSetting, value: i32) -> Result<(), Error> {
        self.set_video_setting(setting, &value.to_string())
    }

    /// Persist video settings across logout, target switch and reconnect.
    ///
    /// The device only stores elements carrying both gains; they're taken from
    /// `current` ([`Event::ConfigXml`] → [`crate::parse_video_settings`]).
    /// Acknowledged by a fresh [`Event::ConfigXml`]; silence on refusal.
    pub fn store_video_settings(
        &self,
        current: &VideoSettings,
        changes: &[(VideoSetting, i32)],
    ) -> Result<(), Error> {
        self.queue(Outgoing::Cmd(outbound::store_video_settings(
            current, changes,
        )))
    }

    /// [`VideoFilter::Auto`] runs auto-adjust, as the vendor client does.
    pub fn set_video_filter(&self, filter: VideoFilter) -> Result<(), Error> {
        match filter {
            VideoFilter::Auto => self.auto_adjust_video(),
            other => self.set_video_setting(VideoSetting::Filter, other.as_str()),
        }
    }

    /// Switch target on the same channels, without logging in again. The
    /// device sends a fresh `CONFIG_XML` and key frame. It needs the target's
    /// name as well as its port.
    pub fn change_target(&self, name: &str, port: u32) -> Result<(), Error> {
        self.queue(Outgoing::Cmd(outbound::change_target(name, port)))
    }

    /// Mouse calibration
    /// (`<command name='calibrate' state='start' mode='calibrate'/>`). Learns
    /// the target's acceleration curve; progress arrives as [`Event::Gui`].
    pub fn calibrate_mouse(&self) -> Result<(), Error> {
        self.queue(Outgoing::Cmd(outbound::calibrate_mouse()))
    }

    /// Align the remote pointer with the local one
    /// (`<command name='align' state='start' mode='align'/>`).
    pub fn align_mouse(&self) -> Result<(), Error> {
        self.queue(Outgoing::Cmd(outbound::align_mouse()))
    }

    pub fn set_exclusive(&self, exclusive: bool) -> Result<(), Error> {
        self.queue(Outgoing::Cmd(outbound::set_exclusive(exclusive)))
    }

    /// Send an app-level `<commands>` document.
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

    /// Stop the session and join its thread. Drop the sender, then the
    /// sockets, then join.
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

/// Sleep after `idle` empty pumps. Busy sessions never reach the longer
/// sleeps; an idle one settles at 125 wakeups/s.
fn backoff(idle: u32) -> Duration {
    match idle {
        0..=64 => Duration::from_micros(250),
        65..=256 => Duration::from_millis(2),
        _ => Duration::from_millis(8),
    }
}
