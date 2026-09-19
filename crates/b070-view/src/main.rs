//! `b070-view` — a KVM viewer for Tripp Lite NetCommander / Minicom Smart IP
//! switches.
//!
//! Logs in over the management API to obtain a session token, opens the KVM
//! session with that token, and shows the target's screen in a window with
//! keyboard and mouse passed through.

mod keys;
mod viewer;

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use b070_session::{Event, Session, SessionConfig};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "b070-view",
    about = "View and control a target on a Tripp Lite NetCommander IP KVM",
    long_about = "Opens a KVM session to one target and shows its screen.\n\n\
                  Keyboard and mouse are passed through. Ctrl+Alt+End sends \
                  Ctrl+Alt+Delete to the target, Ctrl+Alt+Home re-syncs the \
                  remote pointer with yours, and Ctrl+Alt+A runs the device's \
                  automatic video adjustment."
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
    /// captured image for the target's current video mode.
    #[arg(long)]
    auto_adjust: bool,
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
    let client = b070_mgmt::Client::connect(&args.host, &args.user, &args.password)
        .with_context(|| format!("logging in to {}", args.host))?;

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

    match &args.snapshot {
        Some(path) => snapshot(&session, args.frames, path),
        None => viewer::run(session),
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
