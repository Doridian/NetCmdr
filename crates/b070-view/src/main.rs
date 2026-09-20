// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! `b070-view` — a KVM viewer for Tripp Lite NetCommander / Minicom Smart IP
//! switches.
//!
//! Logs in over the management API to obtain a session token, opens the KVM
//! session with that token, and shows the target's screen in a window with
//! keyboard and mouse passed through.

mod link;
mod render;
mod shell;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use b070_session::align::{self, Aligner, Step as AlignStep};
use b070_session::{Event, Session, SessionConfig, VideoSettings};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "b070-view",
    about = "View and control a target on a Tripp Lite NetCommander IP KVM",
    long_about = "Opens a KVM session to one target and shows its screen.\n\n\
                  Keyboard and mouse are passed through. Ctrl+Alt+End sends \
                  Ctrl+Alt+Delete to the target, Ctrl+Alt+Home re-syncs the \
                  remote pointer with yours, Ctrl+Alt+A runs the device's \
                  automatic video adjustment and Ctrl+Alt+Shift+A measures the \
                  capture offsets from the picture and corrects them.\n\n\
                  The window's menu bar covers the rest: key combinations your \
                  own desktop swallows, switching target without reconnecting, \
                  mouse calibration, video adjustment and target power control."
)]
struct Args {
    /// Device address.
    #[arg(long, env = "B070_HOST")]
    host: String,

    /// Username.
    #[arg(long, short, env = "B070_USER", default_value = "admin")]
    user: String,

    /// Password.
    #[arg(long, short, env = "B070_PASSWORD", default_value = "access")]
    password: String,

    /// Target switch port to view, 1-based.
    #[arg(long, short, default_value_t = 1)]
    target: u32,

    /// Session port. Defaults to the one the device advertises at login.
    #[arg(long)]
    session_port: Option<u16>,

    /// Share the target instead of taking an exclusive session.
    #[arg(long)]
    shared: bool,

    /// List the device's targets and exit.
    #[arg(long)]
    list: bool,

    /// Headless: capture frames to this PPM file instead of opening a window.
    #[arg(long, value_name = "FILE")]
    snapshot: Option<PathBuf>,

    /// With --snapshot, how many frames to collect before writing.
    #[arg(long, default_value_t = 10)]
    frames: u32,

    /// Run the device's automatic video adjustment on connect.
    ///
    /// The device digitises analog VGA; this re-centres and re-sharpens the
    /// captured image for the target's current video mode and stores the
    /// result. Its answer is only a good one from a session in which nothing
    /// has been set by hand — it measures against a capture window an earlier
    /// set has already moved, and corrects against its own stored figure.
    #[arg(long)]
    auto_adjust: bool,

    /// Align the capture window with the target's picture on connect.
    ///
    /// Measures where the picture actually starts, moves the device's sampling
    /// offsets onto it and writes them to the device, where they stay. The
    /// device's own --auto-adjust measures the same thing on its own terms;
    /// this one is measured here, from the picture, and checked against the
    /// range the device declares. Needs something on screen to measure
    /// against.
    #[arg(long)]
    align: bool,

    /// With --align, leave the vertical offset alone.
    ///
    /// A target sitting at a bare console has nothing in its top rows, so there
    /// is nothing to measure vertically until it has scrolled.
    #[arg(long)]
    align_horizontal: bool,

    /// With --align, prove each answer by stepping one below it and checking
    /// that exactly one blank lane appears. Roughly doubles the run.
    #[arg(long)]
    align_verify: bool,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "b070_view=info,b070_session=info".into()),
        )
        .init();

    let args = Args::parse();

    // The management login is what mints the session token; hold the client for
    // the whole session, because logging out would invalidate it.
    let client = Arc::new(
        b070_mgmt::Client::connect(&args.host, &args.user, &args.password)
            .with_context(|| format!("logging in to {}", args.host))?,
    );

    if args.list {
        for t in client.targets().context("reading targets")? {
            println!("{:>3}  {}", t.port, t.name);
        }
        return Ok(());
    }

    let port = args
        .session_port
        .or(client.auth_info().port)
        .unwrap_or(b070_proto::DEFAULT_SESSION_PORT);

    let mut cfg = SessionConfig::new(
        &args.host,
        port,
        client.session_id(),
        &args.user,
        args.target,
    );
    cfg.exclusive = !args.shared;

    tracing::info!(host = %args.host, port, target = args.target, "opening session");
    let session = Session::connect(cfg).context("opening the KVM session")?;

    if args.auto_adjust {
        tracing::info!("running automatic video adjustment");
        session.auto_adjust_video()?;
    }

    if args.align {
        align_capture(
            &session,
            align::Options {
                vertical: !args.align_horizontal,
                verify: args.align_verify,
            },
        )?;
    }

    match &args.snapshot {
        Some(path) => snapshot(&session, args.frames, path),
        None => shell::run(
            session,
            shell::Config {
                host: args.host,
                user: args.user,
                target: args.target,
                exclusive: !args.shared,
                client,
            },
        ),
    }
}

/// Measure the capture offsets from the picture and correct them.
///
/// Drives the same state machine the window uses ([`b070_session::align`]) from
/// a plain loop: it never blocks, so pumping it is just a matter of handing it
/// a frame and carrying out the one step it asks for.
fn align_capture(session: &Session, opts: align::Options) -> Result<()> {
    /// Long enough for both axes with verification, which measured 18.6 s on
    /// the hardware, plus a restore through the device's own adjustment.
    const TIMEOUT: Duration = Duration::from_secs(90);
    /// How long to wait for the device to announce the mode it is digitising.
    const SETTINGS_TIMEOUT: Duration = Duration::from_secs(15);

    let mut video: Option<VideoSettings> = None;
    let mut aligner: Option<Aligner> = None;
    // The mode the run was armed on. The device announces the mode it is
    // already on at the start of every session, so it is a *change* of mode
    // that invalidates a run, not an announcement of one.
    let mut armed_on: Option<(u16, u16)> = None;
    let started = Instant::now();

    loop {
        // Drain whatever has arrived.
        loop {
            match session.events.recv_timeout(Duration::from_millis(20)) {
                Ok(Event::ConfigXml(xml)) => {
                    if let Some(v) = b070_session::parse_video_settings(&xml) {
                        tracing::info!(
                            width = v.width,
                            height = v.height,
                            refresh = v.refresh,
                            hoffset = v.hoffset.value,
                            voffset = v.voffset.value,
                            "device capture settings"
                        );
                        video = Some(v);
                    }
                }
                Ok(Event::Resolution { width, height }) => {
                    tracing::info!(width, height, "resolution");
                    if armed_on.is_some_and(|had| had != (width, height)) {
                        if let Some(a) = &mut aligner {
                            a.abort("the video mode changed");
                        }
                    }
                }
                Ok(Event::Gui(msg)) if !msg.is_empty() => tracing::debug!(%msg, "device message"),
                Ok(Event::NoSignal) => anyhow::bail!("no video signal from the target"),
                Ok(Event::Disconnected(reason)) => anyhow::bail!("session ended: {reason}"),
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => break,
                Err(e) => anyhow::bail!("session ended: {e}"),
            }
        }

        let Some(v) = &video else {
            anyhow::ensure!(
                started.elapsed() < SETTINGS_TIMEOUT,
                "the device never reported its capture settings"
            );
            continue;
        };

        let step = {
            let frame = session.frame().lock().expect("framebuffer lock");
            // Nothing is armed until there is a picture: until then the mode
            // the run belongs to is not settled either.
            if frame.width == 0 || frame.height == 0 {
                anyhow::ensure!(
                    started.elapsed() < SETTINGS_TIMEOUT,
                    "the device never sent a picture to align to"
                );
                continue;
            }
            let aligner = match &mut aligner {
                Some(a) => a,
                none => {
                    armed_on = Some((frame.width, frame.height));
                    none.insert(Aligner::new(opts))
                }
            };
            aligner.poll(Instant::now(), &frame, v)
        };
        match step {
            AlignStep::Set(setting, value) => {
                tracing::debug!(setting = setting.as_str(), value, "setting");
                session.set_video_number(setting, value)?;
            }
            AlignStep::Store(changes) => {
                tracing::info!(?changes, "writing the capture position to the device");
                session.store_video_settings(v, &changes)?;
            }
            AlignStep::Wait => {}
            AlignStep::Done(report) => {
                println!("{}", report.summary());
                return Ok(());
            }
            AlignStep::Failed(why) => anyhow::bail!("could not align the capture: {why}"),
        }

        anyhow::ensure!(
            started.elapsed() < TIMEOUT,
            "the alignment did not finish within {TIMEOUT:?}"
        );
    }
}

/// Collect frames without a window and write the framebuffer as a PPM.
///
/// This is the quickest way to check the video pipeline against real hardware:
/// no windowing system needed, and the output is trivially comparable.
fn snapshot(session: &Session, frames: u32, path: &PathBuf) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut seen = 0;

    while seen < frames && Instant::now() < deadline {
        match session.events.recv_timeout(Duration::from_millis(500)) {
            Ok(Event::Frame(rect)) => {
                seen += 1;
                tracing::debug!(?rect, seen, "frame");
            }
            Ok(Event::Resolution { width, height }) => {
                tracing::info!(width, height, "resolution")
            }
            Ok(Event::Noise(n)) => {
                tracing::info!(level = n.level, changes = n.changes, "video noise")
            }
            Ok(Event::NoSignal) => tracing::warn!("no video signal from the target"),
            Ok(Event::Gui(msg)) => tracing::info!(%msg, "device message"),
            Ok(Event::Disconnected(reason)) => {
                anyhow::bail!("session ended: {reason}")
            }
            Ok(Event::ConfigXml(_)) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => anyhow::bail!("session ended: {e}"),
        }
    }

    let frame = session.frame().lock().expect("framebuffer lock");
    anyhow::ensure!(frame.width > 0 && frame.height > 0, "no video received");

    let mut out = std::io::BufWriter::new(
        std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))?,
    );
    write!(out, "P6\n{} {}\n255\n", frame.width, frame.height)?;
    for px in &frame.pixels {
        out.write_all(&[(px >> 16) as u8, (px >> 8) as u8, *px as u8])?;
    }
    out.flush()?;

    println!(
        "wrote {} ({}x{}, {seen} frames)",
        path.display(),
        frame.width,
        frame.height
    );
    Ok(())
}
