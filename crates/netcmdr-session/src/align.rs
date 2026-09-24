// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Aligns the capture window with the target's picture.
//!
//! The device samples analog VGA starting at `hoffset` (pixel clocks after
//! hsync) and `voffset` (lines after vsync), taken from a firmware mode table.
//! When the target's timing differs from the table (e.g. 1360×768), an edge of
//! the picture is cut off. This measures the offsets from the picture itself.
//!
//! # The measurement
//!
//! Lowering an offset shifts the picture right/down and exposes blanking as
//! dark lanes at the leading edge, one lane per unit:
//!
//! ```text
//! correct = probe + blank_lanes(picture at probe)
//! ```
//!
//! One probe, no search. On hardware (1360×768, correct `hoffset` 364):
//! probing 360 showed 4 blank columns, 352 showed 12, 348 showed 16.
//!
//! `hoffset_min`/`hoffset_max` are exactly the mode's blanking interval, so an
//! answer outside that range is a bad measurement and is refused.
//!
//! # Caveats
//!
//! * **Blanking and black content look identical.** The range check and
//!   [`has_content`] are the only guards.
//! * **A session set equal to the stored value is silently dropped**, so a
//!   probe can't be undone by a session set. Use a stored write
//!   ([`Step::Store`]), which also persists the answer.
//! * **A set takes 0.5–1.6 s to show up.** We watch the picture rather than
//!   sleeping.

use netcmdr_proto::command::VideoSetting;

use crate::core::{Knob, VideoSettings};
use crate::time::{Duration, Instant};
use crate::video::Framebuffer;

/// Longest leading blank run counted. Blanking is tens of pixels; anything
/// longer is dark content and the measurement is refused.
pub const MAX_BLANK_LANES: usize = 96;

/// Margin above the frame's black level for a lane to count as picture. The
/// floor varies (11–16 measured) with frame and gain, so it's relative.
const CONTENT_MARGIN: u32 = 16;

/// Lit pixels needed to be worth measuring (~two console characters).
const MIN_CONTENT_PIXELS: usize = 256;

/// How long the lane profile must hold still before a set counts as landed.
const SETTLE_QUIET: Duration = Duration::from_millis(250);
/// Give up waiting and measure what's there.
const SETTLE_TIMEOUT: Duration = Duration::from_millis(4000);
/// How long to wait for content. The framebuffer is sized before its tiles
/// arrive, so early in a session it's blank.
const CONTENT_TIMEOUT: Duration = Duration::from_millis(8000);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// `hoffset`, in pixel clocks (columns).
    H,
    /// `voffset`, in lines (rows).
    V,
}

impl Axis {
    pub fn setting(self) -> VideoSetting {
        match self {
            Axis::H => VideoSetting::HOffset,
            Axis::V => VideoSetting::VOffset,
        }
    }

    /// The device's name for the knob.
    pub fn name(self) -> &'static str {
        match self {
            Axis::H => "hoffset",
            Axis::V => "voffset",
        }
    }

    fn knob(self, v: &VideoSettings) -> Knob {
        match self {
            Axis::H => v.hoffset,
            Axis::V => v.voffset,
        }
    }

    /// How far below the default to probe. Vertical blanking is shorter, so
    /// its step is smaller.
    fn step(self) -> i32 {
        match self {
            Axis::H => 16,
            Axis::V => 8,
        }
    }

    /// Lanes of the settle profile, counted from the leading edge.
    fn profile_lanes(self) -> usize {
        match self {
            Axis::H => 64,
            Axis::V => 48,
        }
    }

    fn next(self) -> Option<Axis> {
        match self {
            Axis::H => Some(Axis::V),
            Axis::V => None,
        }
    }
}

// --------------------------------------------------------------------- metrics

/// Rec. 601 luma.
fn luma(px: u32) -> u32 {
    let (r, g, b) = ((px >> 16) & 0xFF, (px >> 8) & 0xFF, px & 0xFF);
    (r * 77 + g * 151 + b * 28) >> 8
}

/// Per-lane maximum luma for the first `n` lanes. Max, not mean: one bright
/// pixel proves a lane is active.
pub fn lane_maxima(fb: &Framebuffer, axis: Axis, n: usize) -> Vec<u32> {
    let (w, h) = (fb.width as usize, fb.height as usize);
    if w == 0 || h == 0 || fb.pixels.len() < w * h {
        return Vec::new();
    }
    match axis {
        Axis::H => (0..n.min(w))
            .map(|x| {
                (0..h)
                    .map(|y| luma(fb.pixels[y * w + x]))
                    .max()
                    .unwrap_or(0)
            })
            .collect(),
        Axis::V => (0..n.min(h))
            .map(|y| {
                fb.pixels[y * w..(y + 1) * w]
                    .iter()
                    .map(|p| luma(*p))
                    .max()
                    .unwrap_or(0)
            })
            .collect(),
    }
}

/// The frame's blanking level: the lowest lane maximum.
pub fn black_level(fb: &Framebuffer, axis: Axis) -> u32 {
    lane_maxima(fb, axis, usize::MAX)
        .into_iter()
        .min()
        .unwrap_or(0)
}

/// Count of leading blank lanes (capped at [`MAX_BLANK_LANES`]) and the black
/// level used.
pub fn blank_lanes(fb: &Framebuffer, axis: Axis) -> (usize, u32) {
    let black = black_level(fb, axis);
    let span = match axis {
        Axis::H => fb.width as usize,
        Axis::V => fb.height as usize,
    };
    let n = lane_maxima(fb, axis, span.min(MAX_BLANK_LANES))
        .into_iter()
        .take_while(|m| *m <= black + CONTENT_MARGIN)
        .count();
    (n, black)
}

/// Whether the picture has anything to align to. An all-black screen would
/// otherwise measure as the lane cap.
pub fn has_content(fb: &Framebuffer) -> bool {
    let thresh = black_level(fb, Axis::H) + CONTENT_MARGIN;
    fb.pixels.iter().filter(|p| luma(**p) > thresh).count() >= MIN_CONTENT_PIXELS
}

/// Mean luma per lane for the first `n` lanes; used to detect motion.
fn lane_profile(fb: &Framebuffer, axis: Axis, n: usize) -> Vec<f32> {
    let (w, h) = (fb.width as usize, fb.height as usize);
    if w == 0 || h == 0 || fb.pixels.len() < w * h {
        return Vec::new();
    }
    match axis {
        Axis::H => (0..n.min(w))
            .map(|x| {
                let sum: u64 = (0..h).map(|y| luma(fb.pixels[y * w + x]) as u64).sum();
                sum as f32 / h as f32
            })
            .collect(),
        Axis::V => (0..n.min(h))
            .map(|y| {
                let sum: u64 = fb.pixels[y * w..(y + 1) * w]
                    .iter()
                    .map(|p| luma(*p) as u64)
                    .sum();
                sum as f32 / w as f32
            })
            .collect(),
    }
}

/// Mean absolute difference between two profiles. Noise stays well under 1.
fn profile_dist(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return f32::INFINITY;
    }
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f32>() / a.len() as f32
}

const MOVED: f32 = 0.5;

/// Waits for the picture to stop changing after a set.
struct Settle {
    axis: Axis,
    started: Instant,
    /// Profile when the set was sent.
    before: Vec<f32>,
    last: Vec<f32>,
    /// Changed from `before` yet.
    moved: bool,
    quiet_since: Instant,
}

impl Settle {
    fn new(now: Instant, axis: Axis, fb: &Framebuffer) -> Settle {
        let before = lane_profile(fb, axis, axis.profile_lanes());
        Settle {
            axis,
            started: now,
            last: before.clone(),
            before,
            moved: false,
            quiet_since: now,
        }
    }

    /// True once the picture has settled or the wait timed out. A timeout
    /// isn't an error: a dropped set changes nothing.
    fn poll(&mut self, now: Instant, fb: &Framebuffer) -> bool {
        let profile = lane_profile(fb, self.axis, self.axis.profile_lanes());
        if profile_dist(&profile, &self.last) > MOVED {
            self.quiet_since = now;
            if profile_dist(&profile, &self.before) > MOVED {
                self.moved = true;
            }
        } else if self.moved && now.saturating_duration_since(self.quiet_since) > SETTLE_QUIET {
            self.last = profile;
            return true;
        }
        self.last = profile;
        if now.saturating_duration_since(self.started) > SETTLE_TIMEOUT {
            tracing::debug!(
                axis = self.axis.name(),
                moved = self.moved,
                "settle timed out"
            );
            return true;
        }
        false
    }
}

// ---------------------------------------------------------------------- report

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Moved to `to`, leaving `blank` leading lanes (0 if correct).
    Corrected { to: i32, blank: usize },
    /// The stored value was already right; the probe was undone.
    AlreadyCorrect,
    /// Nothing changed, with the reason.
    Refused(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AxisReport {
    pub axis: Axis,
    /// The device's value before the run.
    pub default: i32,
    pub outcome: Outcome,
}

impl AxisReport {
    pub fn line(&self) -> String {
        let name = self.axis.name();
        match &self.outcome {
            Outcome::Corrected { to, blank: 0 } => format!("{name} {} → {to}", self.default),
            Outcome::Corrected { to, blank } => {
                format!("{name} {} → {to} (still {blank} blank)", self.default)
            }
            Outcome::AlreadyCorrect => format!("{name} already correct at {}", self.default),
            Outcome::Refused(why) => format!("{name}: {why}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub axes: Vec<AxisReport>,
}

impl Report {
    pub fn changed(&self) -> bool {
        self.axes
            .iter()
            .any(|a| matches!(a.outcome, Outcome::Corrected { .. }))
    }

    /// One line for the status bar.
    pub fn summary(&self) -> String {
        if self.axes.is_empty() {
            return "nothing to align".into();
        }
        self.axes
            .iter()
            .map(AxisReport::line)
            .collect::<Vec<_>>()
            .join("; ")
    }
}

// ---------------------------------------------------------------------- driver

/// What the caller should do next.
#[derive(Debug, Clone, PartialEq)]
pub enum Step {
    /// Session-only set (a probe): `Session::set_video_number`.
    Set(VideoSetting, i32),
    /// Stored write: `Session::store_video_settings(settings, &changes)`.
    ///
    /// Every run ends with one. It restores probed axes (a session set of the
    /// stored value would be dropped) and persists corrections.
    Store(Vec<(VideoSetting, i32)>),
    /// Poll again with the next frame.
    Wait,
    Done(Report),
    Failed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Measure `voffset` too. Turn off for a bare console, whose top rows are
    /// empty.
    pub vertical: bool,
    /// After applying, step one below and check exactly one lane goes blank.
    /// Roughly doubles the run (18.6 s vs 11 s on hardware).
    pub verify: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            vertical: true,
            verify: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Phase {
    /// Waiting for content.
    Start,
    Begin(Axis),
    /// Measure the probe once it settles.
    Probe {
        axis: Axis,
        probe: i32,
    },
    /// Work out what to store.
    Decide,
    /// Check the stored write left clean edges.
    Confirm,
    NextVerify,
    /// Check one below the answer shows exactly one blank lane.
    Verify {
        axis: Axis,
        answer: i32,
    },
    /// Restore anything moved, then fail.
    Abort {
        why: String,
    },
    Done(Report),
    Failed(String),
}

/// Measures and corrects the capture offsets.
///
/// A non-blocking polled state machine (the browser can't block). Each poll
/// returns one [`Step`] for the caller to carry out.
///
/// ```no_run
/// # use netcmdr_session::align::{Aligner, Options, Step};
/// # use netcmdr_session::{Framebuffer, VideoSettings};
/// # use netcmdr_session::time::Instant;
/// # fn go(fb: &Framebuffer, v: &VideoSettings) {
/// let mut aligner = Aligner::new(Options::default());
/// loop {
///     match aligner.poll(Instant::now(), fb, v) {
///         Step::Set(setting, value) => { /* session.set_video_number(setting, value) */ }
///         Step::Store(changes) => { /* session.store_video_settings(v, &changes) */ }
///         Step::Wait => {}
///         Step::Done(report) => { println!("{}", report.summary()); break }
///         Step::Failed(why) => { println!("{why}"); break }
///     }
/// }
/// # }
/// ```
///
/// [`VideoSettings`] holds the *stored* values, which session sets don't
/// change. A [`Step::Store`] does, so each axis's default is recorded when it
/// is measured.
pub struct Aligner {
    opts: Options,
    phase: Phase,
    /// Content wait deadline, set on first poll.
    deadline: Option<Instant>,
    settle: Option<Settle>,
    measured: Vec<AxisReport>,
    /// Probed axes and their original values; each gets its answer or its
    /// original back when the run ends.
    touched: Vec<(Axis, i32)>,
    /// Answers pending [`Options::verify`].
    to_verify: Vec<(Axis, i32)>,
}

impl Aligner {
    pub fn new(opts: Options) -> Aligner {
        Aligner {
            opts,
            phase: Phase::Start,
            deadline: None,
            settle: None,
            measured: Vec::new(),
            touched: Vec::new(),
            to_verify: Vec::new(),
        }
    }

    pub fn is_running(&self) -> bool {
        !matches!(self.phase, Phase::Done(_) | Phase::Failed(_))
    }

    /// Abandon the run, restoring anything moved. Keep polling until a
    /// terminal [`Step`].
    pub fn abort(&mut self, why: impl Into<String>) {
        if self.is_running() {
            self.settle = None;
            self.phase = Phase::Abort { why: why.into() };
        }
    }

    pub fn poll(&mut self, now: Instant, fb: &Framebuffer, v: &VideoSettings) -> Step {
        if let Some(settle) = &mut self.settle {
            if !settle.poll(now, fb) {
                return Step::Wait;
            }
            self.settle = None;
        }
        self.advance(now, fb, v)
    }

    /// Emit a probe and wait for it to land. Never called with the stored
    /// value, which the device would drop; restoring is [`Aligner::store`]'s
    /// job.
    fn set(
        &mut self,
        now: Instant,
        fb: &Framebuffer,
        v: &VideoSettings,
        axis: Axis,
        value: i32,
    ) -> Step {
        if !self.touched.iter().any(|(a, _)| *a == axis) {
            self.touched.push((axis, axis.knob(v).value));
        }
        self.settle = Some(Settle::new(now, axis, fb));
        Step::Set(axis.setting(), value)
    }

    /// Emit one stored write for all axes and wait for it to land.
    fn store(&mut self, now: Instant, fb: &Framebuffer, targets: &[(Axis, i32)]) -> Step {
        if let Some((axis, _)) = targets.first() {
            self.settle = Some(Settle::new(now, *axis, fb));
        }
        Aligner::store_step(targets)
    }

    /// The stored write without a settle wait, for aborts.
    fn store_step(targets: &[(Axis, i32)]) -> Step {
        Step::Store(
            targets
                .iter()
                .map(|(axis, value)| (axis.setting(), *value))
                .collect(),
        )
    }

    /// Final value for each touched axis: its answer, else its original.
    fn targets(&self) -> Vec<(Axis, i32)> {
        self.touched
            .iter()
            .map(|(axis, was)| {
                let answer = self
                    .measured
                    .iter()
                    .find(|a| a.axis == *axis)
                    .and_then(|a| match a.outcome {
                        Outcome::Corrected { to, .. } => Some(to),
                        _ => None,
                    });
                (*axis, answer.unwrap_or(*was))
            })
            .collect()
    }

    fn done(&mut self) -> Phase {
        let report = Report {
            axes: std::mem::take(&mut self.measured),
        };
        tracing::info!(summary = %report.summary(), "alignment finished");
        Phase::Done(report)
    }

    fn finish_axis(&mut self, axis: Axis, default: i32, outcome: Outcome) {
        tracing::info!(axis = axis.name(), default, ?outcome, "axis measured");
        self.measured.push(AxisReport {
            axis,
            default,
            outcome,
        });
    }

    /// Start probing an axis, or skip it if there's no room below its default.
    fn begin_axis(
        &mut self,
        now: Instant,
        fb: &Framebuffer,
        v: &VideoSettings,
        axis: Axis,
    ) -> Option<Step> {
        let knob = axis.knob(v);
        let probe = (knob.value - axis.step()).max(knob.min);
        if knob.is_fixed() || probe >= knob.value {
            self.finish_axis(
                axis,
                knob.value,
                Outcome::Refused("the device leaves no room below its default".into()),
            );
            self.next_axis(axis);
            return None;
        }
        tracing::debug!(
            axis = axis.name(),
            default = knob.value,
            probe,
            min = knob.min,
            max = knob.max,
            "probing"
        );
        self.phase = Phase::Probe { axis, probe };
        Some(self.set(now, fb, v, axis, probe))
    }

    fn next_axis(&mut self, done: Axis) {
        self.phase = match done.next().filter(|_| self.opts.vertical) {
            Some(next) => Phase::Begin(next),
            None => Phase::Decide,
        };
    }

    fn advance(&mut self, now: Instant, fb: &Framebuffer, v: &VideoSettings) -> Step {
        loop {
            match self.phase.clone() {
                Phase::Start => {
                    let deadline = *self.deadline.get_or_insert(now + CONTENT_TIMEOUT);
                    let sized = fb.width > 0 && fb.height > 0;
                    if !sized || !has_content(fb) {
                        if now <= deadline {
                            return Step::Wait;
                        }
                        self.phase = Phase::Failed(if sized {
                            "nothing to align to: the picture is blank. Give the target \
                             something to draw — a console has to be scrolled before its \
                             top rows can be measured."
                                .into()
                        } else {
                            "no picture to align to".to_string()
                        });
                        continue;
                    }
                    self.phase = Phase::Begin(Axis::H);
                    continue;
                }

                Phase::Begin(axis) => match self.begin_axis(now, fb, v, axis) {
                    Some(step) => return step,
                    None => continue,
                },

                Phase::Probe { axis, probe } => {
                    let knob = axis.knob(v);
                    let (blank, black) = blank_lanes(fb, axis);
                    tracing::debug!(axis = axis.name(), probe, blank, black, "probe measured");

                    if blank == 0 {
                        // Still inside the active area; probe deeper.
                        if probe > knob.min {
                            let deeper = (probe - axis.step()).max(knob.min);
                            self.phase = Phase::Probe {
                                axis,
                                probe: deeper,
                            };
                            return self.set(now, fb, v, axis, deeper);
                        }
                        self.finish_axis(
                            axis,
                            knob.value,
                            Outcome::Refused(
                                "no blanking appeared anywhere in the device's range".into(),
                            ),
                        );
                        self.next_axis(axis);
                        continue;
                    }

                    if blank >= MAX_BLANK_LANES {
                        self.finish_axis(
                            axis,
                            knob.value,
                            Outcome::Refused(format!(
                                "the picture is dark for more than {MAX_BLANK_LANES} \
                                 {}, which is content and not blanking",
                                match axis {
                                    Axis::H => "columns",
                                    Axis::V => "rows",
                                }
                            )),
                        );
                        self.next_axis(axis);
                        continue;
                    }

                    // The device's range is the blanking interval, so an
                    // answer outside it is a misread.
                    let answer = probe + blank as i32;
                    if answer > knob.max {
                        self.finish_axis(
                            axis,
                            knob.value,
                            Outcome::Refused(format!(
                                "measured {answer}, past the device's own limit of {}",
                                knob.max
                            )),
                        );
                        self.next_axis(axis);
                        continue;
                    }

                    if answer == knob.value {
                        self.finish_axis(axis, knob.value, Outcome::AlreadyCorrect);
                    } else {
                        self.finish_axis(
                            axis,
                            knob.value,
                            Outcome::Corrected {
                                to: answer,
                                blank: 0,
                            },
                        );
                    }
                    self.next_axis(axis);
                    continue;
                }

                Phase::Decide => {
                    // One stored write moves every probed axis to its answer
                    // or back to its original, and persists the answers.
                    let targets = self.targets();
                    if targets.is_empty() {
                        self.phase = self.done();
                        continue;
                    }
                    tracing::debug!(?targets, "storing");
                    self.phase = Phase::Confirm;
                    return self.store(now, fb, &targets);
                }

                Phase::Confirm => {
                    // A corrected axis that still shows blanking is reported,
                    // not undone; it's still the best answer we have.
                    let corrected: Vec<(Axis, i32)> = self
                        .measured
                        .iter()
                        .filter_map(|a| match a.outcome {
                            Outcome::Corrected { to, .. } => Some((a.axis, to)),
                            _ => None,
                        })
                        .collect();
                    for (axis, answer) in &corrected {
                        let (blank, _) = blank_lanes(fb, *axis);
                        if blank != 0 {
                            tracing::warn!(
                                axis = axis.name(),
                                answer,
                                blank,
                                "the leading edge is still blank after the write"
                            );
                            if let Some(a) = self.measured.iter_mut().find(|a| a.axis == *axis) {
                                a.outcome = Outcome::Corrected { to: *answer, blank };
                            }
                        }
                    }
                    if self.opts.verify {
                        self.to_verify = corrected;
                    }
                    self.phase = Phase::NextVerify;
                    continue;
                }

                Phase::NextVerify => match self.to_verify.first().copied() {
                    Some((axis, answer)) => {
                        self.to_verify.remove(0);
                        let knob = axis.knob(v);
                        // Skip if one below is the stored value; the device
                        // would drop the set.
                        let below = answer - 1;
                        if below < knob.min || below == knob.value {
                            continue;
                        }
                        self.phase = Phase::Verify { axis, answer };
                        return self.set(now, fb, v, axis, below);
                    }
                    None => {
                        self.phase = self.done();
                        continue;
                    }
                },

                Phase::Verify { axis, answer } => {
                    let (blank, _) = blank_lanes(fb, axis);
                    if blank == 1 {
                        tracing::info!(axis = axis.name(), answer, "answer verified");
                    } else {
                        tracing::warn!(
                            axis = axis.name(),
                            answer,
                            blank,
                            "one below the answer should have shown exactly one blank lane"
                        );
                    }
                    // The answer is now stored, so only a stored write can
                    // return to it.
                    self.phase = Phase::NextVerify;
                    return self.store(now, fb, &[(axis, answer)]);
                }

                Phase::Abort { why } => {
                    self.to_verify.clear();
                    let targets: Vec<(Axis, i32)> = std::mem::take(&mut self.touched);
                    self.phase = Phase::Failed(why.clone());
                    if !targets.is_empty() {
                        tracing::info!(%why, ?targets, "abandoning; putting the capture back");
                        return Aligner::store_step(&targets);
                    }
                    continue;
                }

                Phase::Done(report) => return Step::Done(report),
                Phase::Failed(why) => return Step::Failed(why),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::parse_video_settings;

    const W: usize = 320;
    const H: usize = 240;
    /// Nonzero, like the real 11–16.
    const FLOOR: u32 = 12;
    const LIT: u32 = 0x00_30_C0_30;

    /// A frame with `blank_cols`/`blank_rows` of leading blanking. The active
    /// area is half-lit character cells, so the first active lane is bright
    /// and later dark lanes give [`black_level`] its floor.
    fn render(blank_cols: usize, blank_rows: usize) -> Framebuffer {
        let mut fb = Framebuffer::new();
        fb.resize(W as u16, H as u16);
        for y in 0..H {
            for x in 0..W {
                let dark = FLOOR << 16 | FLOOR << 8 | FLOOR;
                fb.pixels[y * W + x] = if x < blank_cols || y < blank_rows {
                    dark
                } else {
                    let (ax, ay) = (x - blank_cols, y - blank_rows);
                    if ax % 8 < 4 && ay % 16 < 8 {
                        LIT
                    } else {
                        dark
                    }
                };
            }
        }
        fb
    }

    fn settings(h: (i32, i32, i32), v: (i32, i32, i32)) -> VideoSettings {
        let xml = format!(
            "<client_config><video_settings cx='1360' cy='768' fq='60' \
             hoffset='{}' hoffset_min='{}' hoffset_max='{}' \
             voffset='{}' voffset_min='{}' voffset_max='{}' \
             brightness='50' contrast='50' phase='4' filter='none'/></client_config>",
            h.0, h.1, h.2, v.0, v.1, v.2
        );
        parse_video_settings(&xml).expect("settings")
    }

    /// Fake device: session sets matching the stored value are dropped;
    /// stored writes move both capture and config.
    struct Fake {
        settings: VideoSettings,
        at_h: i32,
        at_v: i32,
        correct_h: i32,
        correct_v: i32,
        sets: Vec<(VideoSetting, i32)>,
        stores: Vec<Vec<(VideoSetting, i32)>>,
        blank_screen: bool,
    }

    impl Fake {
        fn new(correct_h: i32, correct_v: i32) -> Fake {
            Fake {
                settings: settings((368, 113, 431), (24, 7, 26)),
                at_h: 368,
                at_v: 24,
                correct_h,
                correct_v,
                sets: Vec::new(),
                stores: Vec::new(),
                blank_screen: false,
            }
        }

        fn stored(&self, setting: VideoSetting) -> i32 {
            match setting {
                VideoSetting::HOffset => self.settings.hoffset.value,
                VideoSetting::VOffset => self.settings.voffset.value,
                other => panic!("the aligner touched {other:?}, which is not its to touch"),
            }
        }

        fn point_capture(&mut self, setting: VideoSetting, value: i32) {
            match setting {
                VideoSetting::HOffset => self.at_h = value,
                VideoSetting::VOffset => self.at_v = value,
                other => panic!("the aligner touched {other:?}, which is not its to touch"),
            }
        }

        fn set(&mut self, setting: VideoSetting, value: i32) {
            self.sets.push((setting, value));
            // Dropped, like the real device.
            if value == self.stored(setting) {
                return;
            }
            self.point_capture(setting, value);
        }

        fn store(&mut self, changes: &[(VideoSetting, i32)]) {
            self.stores.push(changes.to_vec());
            for (setting, value) in changes {
                self.point_capture(*setting, *value);
                match setting {
                    VideoSetting::HOffset => self.settings.hoffset.value = *value,
                    VideoSetting::VOffset => self.settings.voffset.value = *value,
                    _ => unreachable!(),
                }
            }
        }

        fn frame(&self) -> Framebuffer {
            if self.blank_screen {
                let mut fb = Framebuffer::new();
                fb.resize(W as u16, H as u16);
                return fb;
            }
            render(
                (self.correct_h - self.at_h).max(0) as usize,
                (self.correct_v - self.at_v).max(0) as usize,
            )
        }
    }

    fn run(fake: &mut Fake, opts: Options) -> Step {
        let mut aligner = Aligner::new(opts);
        let base = Instant::now();
        // Ticks exceed `SETTLE_QUIET`: a moving set lands in two polls, a
        // dropped one hits the settle timeout.
        for tick in 1..4000u32 {
            let now = base + Duration::from_millis(300 * tick as u64);
            let fb = fake.frame();
            let v = fake.settings.clone();
            match aligner.poll(now, &fb, &v) {
                Step::Set(setting, value) => fake.set(setting, value),
                Step::Store(changes) => fake.store(&changes),
                Step::Wait => {}
                end => return end,
            }
        }
        panic!("the aligner never finished");
    }

    fn done(step: Step) -> Report {
        match step {
            Step::Done(report) => report,
            other => panic!("expected a report, got {other:?}"),
        }
    }

    fn horizontal() -> Options {
        Options {
            vertical: false,
            verify: false,
        }
    }

    // ------------------------------------------------------------- metrics

    #[test]
    fn blank_lanes_counts_the_leading_blanking_and_nothing_else() {
        let fb = render(12, 5);
        assert_eq!(blank_lanes(&fb, Axis::H), (12, FLOOR));
        assert_eq!(blank_lanes(&fb, Axis::V), (5, FLOOR));
    }

    #[test]
    fn a_picture_at_the_edge_has_no_blank_lanes() {
        let fb = render(0, 0);
        assert_eq!(blank_lanes(&fb, Axis::H).0, 0);
        assert_eq!(blank_lanes(&fb, Axis::V).0, 0);
    }

    #[test]
    fn the_black_level_is_the_frames_own_floor_not_a_constant() {
        // Lifting the picture must lift the floor too.
        let mut fb = render(8, 0);
        for px in &mut fb.pixels {
            *px += 0x00_20_20_20;
        }
        assert!(black_level(&fb, Axis::H) > FLOOR);
        assert_eq!(blank_lanes(&fb, Axis::H).0, 8);
    }

    #[test]
    fn a_long_dark_run_is_content_not_blanking() {
        let fb = render(200, 0);
        assert_eq!(blank_lanes(&fb, Axis::H).0, MAX_BLANK_LANES);
    }

    #[test]
    fn an_empty_picture_has_nothing_to_align_to() {
        let mut fb = Framebuffer::new();
        fb.resize(W as u16, H as u16);
        assert!(!has_content(&fb));
        assert!(has_content(&render(4, 4)));
    }

    #[test]
    fn metrics_survive_a_framebuffer_with_no_pixels() {
        let fb = Framebuffer::new();
        assert_eq!(lane_maxima(&fb, Axis::H, 8), Vec::<u32>::new());
        assert_eq!(blank_lanes(&fb, Axis::H), (0, 0));
        assert!(!has_content(&fb));
    }

    // -------------------------------------------------------------- driver

    #[test]
    fn one_probe_and_one_addition_finds_the_offset() {
        let mut fake = Fake::new(364, 24);
        let report = done(run(&mut fake, horizontal()));

        assert_eq!(fake.at_h, 364);
        assert_eq!(report.summary(), "hoffset 368 → 364");
        assert!(report.changed());
        assert_eq!(
            fake.stores,
            vec![vec![(VideoSetting::HOffset, 364)]],
            "one write, and the device kept it"
        );
        assert_eq!(fake.settings.hoffset.value, 364, "the device stored it");
    }

    #[test]
    fn it_probes_deeper_until_blanking_appears() {
        // The first probe is still inside the active area.
        let mut fake = Fake::new(340, 24);
        let report = done(run(&mut fake, horizontal()));

        assert_eq!(fake.at_h, 340);
        assert_eq!(report.summary(), "hoffset 368 → 340");
        let probes: Vec<i32> = fake.sets.iter().map(|(_, v)| *v).collect();
        assert_eq!(probes, vec![352, 336], "two probes, then the answer");
        assert_eq!(fake.stores, vec![vec![(VideoSetting::HOffset, 340)]]);
    }

    #[test]
    fn both_axes_in_one_run() {
        let mut fake = Fake::new(364, 25);
        let report = done(run(&mut fake, Options::default()));

        assert_eq!((fake.at_h, fake.at_v), (364, 25));
        assert_eq!(report.summary(), "hoffset 368 → 364; voffset 24 → 25");
        assert_eq!(
            fake.stores,
            vec![vec![
                (VideoSetting::HOffset, 364),
                (VideoSetting::VOffset, 25)
            ]],
            "both axes in the one write the device takes"
        );
    }

    #[test]
    fn an_already_correct_axis_is_put_back_by_a_stored_write() {
        let mut fake = Fake::new(368, 24);
        let report = done(run(&mut fake, horizontal()));

        assert_eq!(report.summary(), "hoffset already correct at 368");
        assert!(!report.changed());
        assert_eq!(fake.stores, vec![vec![(VideoSetting::HOffset, 368)]]);
        assert_eq!(fake.at_h, 368, "the probe was not left behind");
    }

    #[test]
    fn a_correct_axis_does_not_cost_the_other_one_its_correction() {
        let mut fake = Fake::new(368, 25);
        let report = done(run(&mut fake, Options::default()));

        assert_eq!(
            fake.stores,
            vec![vec![
                (VideoSetting::HOffset, 368),
                (VideoSetting::VOffset, 25)
            ]]
        );
        assert_eq!((fake.at_h, fake.at_v), (368, 25));
        assert_eq!(
            report.summary(),
            "hoffset already correct at 368; voffset 24 → 25"
        );
    }

    #[test]
    fn it_never_asks_for_a_value_the_device_would_drop() {
        for correct_h in [368, 364, 340, 369] {
            let mut fake = Fake::new(correct_h, 24);
            run(
                &mut fake,
                Options {
                    vertical: false,
                    verify: true,
                },
            );
            assert!(
                !fake
                    .sets
                    .iter()
                    .any(|(setting, value)| *value == fake.stored(*setting)),
                "correct_h={correct_h}: asked for the value the device was \
                 holding as a session set, which it silently drops"
            );
        }
    }

    #[test]
    fn an_answer_past_the_devices_own_range_is_refused() {
        // Past `hoffset_max`.
        let mut fake = Fake::new(440, 24);
        let report = done(run(&mut fake, horizontal()));

        assert!(!report.changed());
        assert!(
            report
                .summary()
                .contains("past the device's own limit of 431"),
            "{}",
            report.summary()
        );
        assert_eq!(fake.at_h, 368, "the probe was put back");
        assert_eq!(fake.stores, vec![vec![(VideoSetting::HOffset, 368)]]);
    }

    #[test]
    fn a_dark_screen_is_refused_rather_than_guessed_at() {
        let mut fake = Fake::new(364, 24);
        fake.blank_screen = true;

        match run(&mut fake, horizontal()) {
            Step::Failed(why) => assert!(why.contains("nothing to align to"), "{why}"),
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(fake.sets.is_empty(), "nothing was touched");
    }

    #[test]
    fn verifying_steps_one_below_the_answer_and_comes_back() {
        let mut fake = Fake::new(364, 24);
        let report = done(run(
            &mut fake,
            Options {
                vertical: false,
                verify: true,
            },
        ));

        assert_eq!(report.summary(), "hoffset 368 → 364");
        assert_eq!(fake.at_h, 364, "left on the answer, not on the check");
        // Probe and check are session sets; the answer is stored twice.
        let probes: Vec<i32> = fake.sets.iter().map(|(_, v)| *v).collect();
        assert_eq!(probes, vec![352, 363]);
        assert_eq!(
            fake.stores,
            vec![
                vec![(VideoSetting::HOffset, 364)],
                vec![(VideoSetting::HOffset, 364)]
            ]
        );
    }

    #[test]
    fn a_run_cut_short_hands_the_capture_back() {
        let mut fake = Fake::new(364, 24);
        let mut aligner = Aligner::new(horizontal());
        let base = Instant::now();
        let mut aborted = false;

        for tick in 1..100u32 {
            let now = base + Duration::from_millis(300 * tick as u64);
            let fb = fake.frame();
            let v = fake.settings.clone();
            match aligner.poll(now, &fb, &v) {
                Step::Set(setting, value) => {
                    fake.set(setting, value);
                    if !aborted {
                        aborted = true;
                        aligner.abort("the video mode changed");
                    }
                }
                Step::Store(changes) => fake.store(&changes),
                Step::Wait => {}
                Step::Failed(why) => {
                    assert_eq!(why, "the video mode changed");
                    assert_eq!(fake.at_h, 368, "the probe was not left on screen");
                    assert_eq!(fake.stores, vec![vec![(VideoSetting::HOffset, 368)]]);
                    return;
                }
                other => panic!("expected the abort to land, got {other:?}"),
            }
        }
        panic!("the abort never finished");
    }

    #[test]
    fn a_knob_with_no_room_below_its_default_is_refused() {
        let mut fake = Fake::new(364, 24);
        fake.settings = settings((113, 113, 431), (24, 7, 26));
        fake.at_h = 113;
        let report = done(run(&mut fake, horizontal()));

        assert!(
            report.summary().contains("no room below its default"),
            "{}",
            report.summary()
        );
        assert!(fake.sets.is_empty());
        assert!(
            fake.stores.is_empty(),
            "nothing was moved, so nothing to undo"
        );
    }

    #[test]
    fn a_finished_run_stays_finished() {
        let mut fake = Fake::new(364, 24);
        let mut aligner = Aligner::new(horizontal());
        let base = Instant::now();
        for tick in 1..200u32 {
            let now = base + Duration::from_millis(300 * tick as u64);
            let fb = fake.frame();
            let v = fake.settings.clone();
            match aligner.poll(now, &fb, &v) {
                Step::Set(s, value) => fake.set(s, value),
                Step::Done(_) => {
                    assert!(!aligner.is_running());
                    let fb = fake.frame();
                    let again = aligner.poll(now, &fb, &fake.settings.clone());
                    assert!(matches!(again, Step::Done(_)), "{again:?}");
                    return;
                }
                _ => {}
            }
        }
        panic!("never finished");
    }
}
