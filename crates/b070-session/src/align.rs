// Copyright (C) 2026 Doridian
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Aligning the capture window with the picture the target is actually drawing.
//!
//! The device digitises analog VGA. Where sampling starts is `hoffset` (pixel
//! clocks from the horizontal sync edge) and `voffset` (lines from the vertical
//! one), and both come from a video-mode table inside the firmware. Where the
//! target's timing differs from that table — 1360×768 is one such mode — the
//! capture window sits off the active area and an edge of the picture is cut
//! off. The device's own automatic adjustment does not fix this: it *restores*
//! the table value, which is the thing that is wrong.
//!
//! # The measurement
//!
//! Lowering an offset moves the captured picture right (or down) and brings the
//! blanking interval into view as dark lanes at the leading edge. One unit is
//! one pixel or one line, so the error counts itself:
//!
//! ```text
//! correct = probe + blank_lanes(picture at probe)
//! ```
//!
//! That is one probe and one addition — not a search. Measured against the
//! hardware, every probe point agrees: on a 1360×768 target whose correct
//! `hoffset` is 364, probing 360 showed 4 blank columns, 352 showed 12 and 348
//! showed 16.
//!
//! The device's `hoffset_min`/`hoffset_max` are exactly the mode's blanking
//! interval, so a correct answer is always inside the range it reports. An
//! answer outside it is a bad measurement, and refusing it is what keeps a dark
//! picture from producing nonsense — see [`Aligner`].
//!
//! # What makes this awkward
//!
//! * **Blanking and black content are indistinguishable.** Same level, same
//!   temporal noise. There is no metric that separates them; the range check
//!   above is the guard, and [`has_content`] is what keeps an empty screen from
//!   being measured at all.
//! * **A set equal to the device's own default is silently dropped.** The
//!   device compares against its stored config, which client sets never update,
//!   so the default value is unreachable for the rest of the session. The only
//!   way back to it is the device's own automatic adjustment, which is why
//!   [`Step::AutoAdjust`] exists.
//! * **A set reaches the picture 0.5–1.6 s later**, variably. Nothing here
//!   sleeps a fixed amount; it watches the picture instead.

use b070_proto::command::VideoSetting;

use crate::core::{Knob, VideoSettings};
use crate::time::{Duration, Instant};
use crate::video::Framebuffer;

/// How far into the picture a leading blank run is followed.
///
/// A correct answer is bounded by the mode's blanking interval, which is tens
/// of pixels; a run longer than this is dark *content*, not blanking, and the
/// measurement is refused rather than guessed at.
pub const MAX_BLANK_LANES: usize = 96;

/// How far above the frame's own black level a lane has to reach to count as
/// carrying picture.
///
/// Derived from the frame rather than hardcoded: the blanking floor measured
/// between 11 and 16 depending on the frame, and the gain the device is running
/// moves it.
const CONTENT_MARGIN: u32 = 16;

/// Lit pixels a frame needs before it is worth measuring — roughly two
/// characters of console text.
const MIN_CONTENT_PIXELS: usize = 256;

/// How long the lane profile must hold still before a set counts as landed.
const SETTLE_QUIET: Duration = Duration::from_millis(250);
/// How long to wait for it, before measuring whatever is on screen.
const SETTLE_TIMEOUT: Duration = Duration::from_millis(4000);
/// How long to wait for the device's own adjustment to answer. It takes about
/// 6.5 s and its only signal is a fresh config document.
const RESTORE_TIMEOUT: Duration = Duration::from_millis(12000);
/// How long to wait for a picture worth measuring.
///
/// A frame arrives as tiles, so the framebuffer is sized well before it is
/// filled: for the first moments of a session it is a blank screen that is
/// merely on its way to being a picture. Waiting this out is the difference
/// between measuring the target and refusing to.
const CONTENT_TIMEOUT: Duration = Duration::from_millis(8000);

/// Which of the two capture-position knobs an operation is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Horizontal: `hoffset`, in pixel clocks. Columns.
    H,
    /// Vertical: `voffset`, in lines. Rows.
    V,
}

impl Axis {
    pub fn setting(self) -> VideoSetting {
        match self {
            Axis::H => VideoSetting::HOffset,
            Axis::V => VideoSetting::VOffset,
        }
    }

    /// The device's own name for the knob, which is also what the report says.
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

    /// How far below the default to place the first probe.
    ///
    /// Large enough that the blanking is unmistakable, small enough not to
    /// throw the picture off the far edge. The vertical blanking interval is
    /// the smaller of the two, so its step is too.
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

/// Rec. 601 luma, which is all the picture is judged on: the blanking lanes are
/// dark in every channel, so hue carries no information here.
fn luma(px: u32) -> u32 {
    let (r, g, b) = ((px >> 16) & 0xFF, (px >> 8) & 0xFF, px & 0xFF);
    (r * 77 + g * 151 + b * 28) >> 8
}

/// Per-lane maximum luma, for the first `n` lanes from the leading edge.
///
/// The *maximum* and not the mean: one bright pixel anywhere in a column is
/// enough to prove that column is inside the active area, whereas a mean is
/// dragged under the threshold by the black around it.
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

/// The frame's own blanking level: the darkest any whole lane gets.
///
/// A picture with any blanking on screen has at least one lane that is nothing
/// but blanking, and that lane's maximum is the floor everything else is
/// measured against.
pub fn black_level(fb: &Framebuffer, axis: Axis) -> u32 {
    lane_maxima(fb, axis, usize::MAX)
        .into_iter()
        .min()
        .unwrap_or(0)
}

/// How many leading lanes carry nothing but the blanking level, and the level
/// they were judged against.
///
/// Counting stops at [`MAX_BLANK_LANES`]; a run that long is dark content, and
/// the caller refuses rather than believes it.
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

/// Whether there is anything in the picture to align to.
///
/// An all-black screen reads as blanking everywhere, and the arithmetic would
/// hand back whatever the lane cap is. Saying "nothing to align to" is the
/// honest answer; a target sitting at a bare console needs scrolling first.
pub fn has_content(fb: &Framebuffer) -> bool {
    let thresh = black_level(fb, Axis::H) + CONTENT_MARGIN;
    fb.pixels.iter().filter(|p| luma(**p) > thresh).count() >= MIN_CONTENT_PIXELS
}

/// Mean luma per lane, over the first `n` lanes — the cheap signal that says
/// whether the picture is still moving.
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

/// Mean absolute difference between two profiles. Analog noise keeps this well
/// under one level; a picture that has moved puts it far above.
fn profile_dist(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return f32::INFINITY;
    }
    a.iter().zip(b).map(|(x, y)| (x - y).abs()).sum::<f32>() / a.len() as f32
}

/// How far two profiles must differ to count as the picture having moved.
const MOVED: f32 = 0.5;

/// Watches the picture until it stops changing after a set.
struct Settle {
    axis: Axis,
    started: Instant,
    /// What the picture looked like when the set went out.
    before: Vec<f32>,
    last: Vec<f32>,
    /// Whether it has changed from `before` at all yet.
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

    /// True once the picture has landed — or once waiting for it has run out,
    /// which is not an error: a set the device drops changes nothing, and
    /// measuring the picture that is there is still the right thing to do.
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

/// What became of one axis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The knob was moved to `to`, leaving `blank` lanes at the leading edge —
    /// zero is what a correct answer gives.
    Corrected { to: i32, blank: usize },
    /// The device's own default was already the right value. It cannot be set
    /// while a session is open, so the device's own adjustment put it back.
    AlreadyCorrect,
    /// Nothing was changed, and why not.
    Refused(String),
}

/// What one axis was measured to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AxisReport {
    pub axis: Axis,
    /// The value the device's mode table gave, which is what it is being
    /// compared against.
    pub default: i32,
    pub outcome: Outcome,
}

impl AxisReport {
    /// One phrase for the status line.
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

/// What a whole run found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub axes: Vec<AxisReport>,
}

impl Report {
    /// Whether anything actually moved.
    pub fn changed(&self) -> bool {
        self.axes
            .iter()
            .any(|a| matches!(a.outcome, Outcome::Corrected { .. }))
    }

    /// The whole run in one line, for the status bar.
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
    /// Set this knob to this value, then go on polling.
    Set(VideoSetting, i32),
    /// Run the device's own automatic adjustment — the only way back to a
    /// default value once a session has moved off it. Tell the aligner when its
    /// config document arrives, with [`Aligner::config_arrived`].
    AutoAdjust,
    /// Nothing to do yet; poll again with the next frame.
    Wait,
    /// Finished. The knobs are where they are going to stay.
    Done(Report),
    /// Gave up without finishing, and why.
    Failed(String),
}

/// How much of a run to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Options {
    /// Measure `voffset` as well as `hoffset`.
    ///
    /// Worth turning off for a target showing a bare console: the top rows are
    /// empty, so there is nothing to measure vertically until it has scrolled.
    pub vertical: bool,
    /// After setting the answer, step one below it and confirm that exactly one
    /// blank lane appears.
    ///
    /// This proves the answer rather than assuming it, and it is what the
    /// measurement was established with. It roughly doubles the run — 18.6 s
    /// against 11 s on the hardware — so it is off unless asked for.
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
    /// Waiting for a frame worth measuring.
    Start,
    /// Put this axis' first probe on screen.
    Begin(Axis),
    /// A probe is on screen; measure it once it settles.
    Probe {
        axis: Axis,
        probe: i32,
    },
    /// Both axes measured; work out what to set.
    Decide,
    /// The device's own adjustment is restoring a default.
    Restoring {
        until: Instant,
    },
    /// Set the next answer, if there is one left.
    Apply,
    /// An answer is on screen; check the leading edge came out clean.
    Confirm {
        axis: Axis,
        answer: i32,
    },
    /// One below the answer is on screen; check exactly one lane went blank.
    Check {
        axis: Axis,
        answer: i32,
    },
    /// The answer has been put back after a check; carry on.
    Resume,
    /// Abandoned, and why. Whatever was moved is restored first.
    Abort {
        why: String,
    },
    Done(Report),
    Failed(String),
}

/// Measures the capture offsets from the picture and corrects them.
///
/// This is a **polled** state machine and never blocks: the browser client
/// pumps its session from the frame callback and has nowhere to block. Each
/// call hands back one [`Step`]; the caller carries it out and calls again with
/// the next frame.
///
/// ```no_run
/// # use b070_session::align::{Aligner, Options, Step};
/// # use b070_session::{Framebuffer, VideoSettings};
/// # use b070_session::time::Instant;
/// # fn go(fb: &Framebuffer, v: &VideoSettings) {
/// let mut aligner = Aligner::new(Options::default());
/// loop {
///     match aligner.poll(Instant::now(), fb, v) {
///         Step::Set(setting, value) => { /* session.set_video_number(setting, value) */ }
///         Step::AutoAdjust => { /* session.auto_adjust_video() */ }
///         Step::Wait => {}
///         Step::Done(report) => { println!("{}", report.summary()); break }
///         Step::Failed(why) => { println!("{why}"); break }
///     }
/// }
/// # }
/// ```
///
/// The knob values in the [`VideoSettings`] passed in are always the device's
/// *defaults*, whatever has been set since: the device does not fold a client's
/// sets back into the config it publishes. That is what makes the comparison at
/// the end meaningful.
pub struct Aligner {
    opts: Options,
    phase: Phase,
    /// Set once a frame has been seen, so the wait for one can time out.
    deadline: Option<Instant>,
    settle: Option<Settle>,
    /// What each axis was measured to be, in the order they were done.
    measured: Vec<AxisReport>,
    /// Answers still to be set, oldest first.
    pending: Vec<(Axis, i32)>,
    /// Whether any knob has been moved off the device's default yet — a run
    /// abandoned after that has to hand the knobs back.
    moved: bool,
    /// Set by [`Aligner::config_arrived`] while the device is restoring.
    restored: bool,
}

impl Aligner {
    pub fn new(opts: Options) -> Aligner {
        Aligner {
            opts,
            phase: Phase::Start,
            deadline: None,
            settle: None,
            measured: Vec::new(),
            pending: Vec::new(),
            moved: false,
            restored: false,
        }
    }

    /// Whether the run is still going, and so still wants frames.
    pub fn is_running(&self) -> bool {
        !matches!(self.phase, Phase::Done(_) | Phase::Failed(_))
    }

    /// The device sent a fresh config document.
    ///
    /// That document is the only thing the device sends when its own adjustment
    /// finishes, so it is what ends the wait for one.
    pub fn config_arrived(&mut self) {
        self.restored = true;
    }

    /// Abandon the run.
    ///
    /// Anything already moved is handed back to the device's own adjustment
    /// first, so a run cut short does not leave the capture sitting on a probe
    /// value. Keep polling until a terminal [`Step`] comes back.
    pub fn abort(&mut self, why: impl Into<String>) {
        if self.is_running() {
            self.settle = None;
            self.phase = Phase::Abort { why: why.into() };
        }
    }

    /// Advance the run by whatever this frame allows.
    pub fn poll(&mut self, now: Instant, fb: &Framebuffer, v: &VideoSettings) -> Step {
        if let Some(settle) = &mut self.settle {
            if !settle.poll(now, fb) {
                return Step::Wait;
            }
            self.settle = None;
        }
        self.advance(now, fb, v)
    }

    /// Emit a set, and start watching for the picture to follow it.
    ///
    /// Nothing here ever asks for the device's own default: the device compares
    /// a set against its stored config and silently drops one that matches, so
    /// such a set would never land and the wait for it would only time out.
    fn set(&mut self, now: Instant, fb: &Framebuffer, axis: Axis, value: i32) -> Step {
        self.moved = true;
        self.settle = Some(Settle::new(now, axis, fb));
        Step::Set(axis.setting(), value)
    }

    fn finish_axis(&mut self, axis: Axis, default: i32, outcome: Outcome) {
        tracing::info!(axis = axis.name(), default, ?outcome, "axis measured");
        self.measured.push(AxisReport {
            axis,
            default,
            outcome,
        });
    }

    /// Start on an axis, or fall through to the next one when this one offers
    /// no room. Returns the step to hand back, if it produced one.
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
        Some(self.set(now, fb, axis, probe))
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
                        // Not deep enough yet: the capture window still starts
                        // inside the active area. Go further and look again.
                        if probe > knob.min {
                            let deeper = (probe - axis.step()).max(knob.min);
                            self.phase = Phase::Probe {
                                axis,
                                probe: deeper,
                            };
                            return self.set(now, fb, axis, deeper);
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

                    // `probe + blank` is the sampling start that puts the first
                    // active lane at the edge. The device's own range is the
                    // mode's blanking interval, so a real answer is inside it
                    // and anything outside is a misread picture.
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
                        self.pending.push((axis, answer));
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
                    // Probing moved every axis off its default. Any axis that
                    // is not about to be set to something else has to be put
                    // back, and the device's own adjustment is the only thing
                    // that can do it — a set of the default value is dropped.
                    let needs_restore = self
                        .measured
                        .iter()
                        .any(|a| !matches!(a.outcome, Outcome::Corrected { .. }));
                    if needs_restore && self.moved {
                        tracing::info!("running the device's adjustment to restore a default");
                        self.restored = false;
                        self.phase = Phase::Restoring {
                            until: now + RESTORE_TIMEOUT,
                        };
                        return Step::AutoAdjust;
                    }
                    self.phase = Phase::Apply;
                    continue;
                }

                Phase::Restoring { until } => {
                    if !self.restored && now <= until {
                        return Step::Wait;
                    }
                    if !self.restored {
                        tracing::warn!("the device's adjustment never answered");
                    }
                    self.phase = Phase::Apply;
                    continue;
                }

                Phase::Apply => match self.pending.first().copied() {
                    Some((axis, answer)) => {
                        self.pending.remove(0);
                        self.phase = Phase::Confirm { axis, answer };
                        return self.set(now, fb, axis, answer);
                    }
                    None => {
                        let report = Report {
                            axes: std::mem::take(&mut self.measured),
                        };
                        tracing::info!(summary = %report.summary(), "alignment finished");
                        self.phase = Phase::Done(report);
                        continue;
                    }
                },

                Phase::Confirm { axis, answer } => {
                    let (blank, _) = blank_lanes(fb, axis);
                    if blank != 0 {
                        tracing::warn!(
                            axis = axis.name(),
                            answer,
                            blank,
                            "the leading edge is still blank after setting the answer"
                        );
                        if let Some(a) = self.measured.iter_mut().find(|a| a.axis == axis) {
                            a.outcome = Outcome::Corrected { to: answer, blank };
                        }
                    }

                    let knob = axis.knob(v);
                    // One below the answer must show exactly one blank lane.
                    // Unless that value is the device's default, which it drops
                    // — the picture would not move and the check would read as
                    // a failure rather than as the set never happening.
                    let below = answer - 1;
                    if self.opts.verify && below >= knob.min && below != knob.value {
                        self.phase = Phase::Check { axis, answer };
                        return self.set(now, fb, axis, below);
                    }
                    self.phase = Phase::Apply;
                    continue;
                }

                Phase::Check { axis, answer } => {
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
                    self.phase = Phase::Resume;
                    return self.set(now, fb, axis, answer);
                }

                Phase::Resume => {
                    self.phase = Phase::Apply;
                    continue;
                }

                Phase::Abort { why } => {
                    self.pending.clear();
                    if self.moved {
                        self.moved = false;
                        tracing::info!(%why, "abandoning; handing the capture back to the device");
                        self.phase = Phase::Abort { why };
                        return Step::AutoAdjust;
                    }
                    self.phase = Phase::Failed(why);
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
    /// The blanking level the fake device digitises. Deliberately not zero:
    /// the real one sits around 11–16 and everything here is relative to it.
    const FLOOR: u32 = 12;
    const LIT: u32 = 0x00_30_C0_30;

    /// One frame as the device would digitise it with `blank_cols` columns and
    /// `blank_rows` rows of blanking showing at the leading edges.
    ///
    /// The active area is drawn as character cells — bright in the first half
    /// of each cell, dark in the second — so that the first active lane is
    /// bright (that is what makes the count meaningful) while plenty of later
    /// lanes are fully dark (that is what gives [`black_level`] the floor
    /// rather than the content).
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

    /// The device, as far as the aligner can tell: a capture window that can be
    /// moved, a stored default it silently refuses to be set to, and an
    /// automatic adjustment that puts both knobs back.
    struct Fake {
        settings: VideoSettings,
        at_h: i32,
        at_v: i32,
        correct_h: i32,
        correct_v: i32,
        auto_adjusts: u32,
        /// Every set the aligner asked for, to prove none was a dropped one.
        sets: Vec<(VideoSetting, i32)>,
        /// Overrides the picture entirely, for the nothing-to-measure case.
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
                auto_adjusts: 0,
                sets: Vec::new(),
                blank_screen: false,
            }
        }

        fn default_for(&self, setting: VideoSetting) -> i32 {
            match setting {
                VideoSetting::HOffset => self.settings.hoffset.value,
                VideoSetting::VOffset => self.settings.voffset.value,
                other => panic!("the aligner set {other:?}, which is not its to touch"),
            }
        }

        fn set(&mut self, setting: VideoSetting, value: i32) {
            self.sets.push((setting, value));
            // The device compares against its stored config, which a client's
            // sets never update, so this one goes nowhere.
            if value == self.default_for(setting) {
                return;
            }
            match setting {
                VideoSetting::HOffset => self.at_h = value,
                VideoSetting::VOffset => self.at_v = value,
                _ => unreachable!(),
            }
        }

        fn auto_adjust(&mut self) {
            self.auto_adjusts += 1;
            self.at_h = self.settings.hoffset.value;
            self.at_v = self.settings.voffset.value;
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

    /// Drive a whole run against the fake and hand back how it ended.
    fn run(fake: &mut Fake, opts: Options) -> Step {
        let mut aligner = Aligner::new(opts);
        let base = Instant::now();
        // One tick is longer than `SETTLE_QUIET`, so a set that moves the
        // picture lands in two polls and one that does not runs out the
        // settle timeout — both as they do against the device.
        for tick in 1..4000u32 {
            let now = base + Duration::from_millis(300 * tick as u64);
            let fb = fake.frame();
            let v = fake.settings.clone();
            match aligner.poll(now, &fb, &v) {
                Step::Set(setting, value) => fake.set(setting, value),
                Step::AutoAdjust => {
                    fake.auto_adjust();
                    aligner.config_arrived();
                }
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
        // Lift the whole picture: the floor has to come up with it, or every
        // lane reads as content.
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
        assert_eq!(fake.auto_adjusts, 0, "nothing needed putting back");
    }

    #[test]
    fn it_probes_deeper_until_blanking_appears() {
        // 16 below the default is still inside the active area; the answer is
        // only reachable from the second probe down.
        let mut fake = Fake::new(340, 24);
        let report = done(run(&mut fake, horizontal()));

        assert_eq!(fake.at_h, 340);
        assert_eq!(report.summary(), "hoffset 368 → 340");
        let probes: Vec<i32> = fake.sets.iter().map(|(_, v)| *v).collect();
        assert_eq!(probes, vec![352, 336, 340]);
    }

    #[test]
    fn both_axes_in_one_run() {
        let mut fake = Fake::new(364, 25);
        let report = done(run(&mut fake, Options::default()));

        assert_eq!((fake.at_h, fake.at_v), (364, 25));
        assert_eq!(report.summary(), "hoffset 368 → 364; voffset 24 → 25");
        assert_eq!(fake.auto_adjusts, 0);
    }

    /// The default is the one value a session cannot set, so an axis that was
    /// right all along has to be handed back to the device's own adjustment.
    #[test]
    fn an_already_correct_axis_goes_back_through_the_devices_adjustment() {
        let mut fake = Fake::new(368, 24);
        let report = done(run(&mut fake, horizontal()));

        assert_eq!(report.summary(), "hoffset already correct at 368");
        assert!(!report.changed());
        assert_eq!(fake.auto_adjusts, 1);
        assert_eq!(fake.at_h, 368, "the probe was not left behind");
    }

    #[test]
    fn a_correct_axis_does_not_cost_the_other_one_its_correction() {
        // The restore puts *both* knobs back, so the vertical answer has to be
        // set after it rather than before.
        let mut fake = Fake::new(368, 25);
        let report = done(run(&mut fake, Options::default()));

        assert_eq!(fake.auto_adjusts, 1);
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
                !fake.sets.iter().any(|(s, v)| *v == fake.default_for(*s)),
                "correct_h={correct_h}: asked for the device's own default, \
                 which it silently drops"
            );
        }
    }

    #[test]
    fn an_answer_past_the_devices_own_range_is_refused() {
        // 440 is inside the picture but outside `hoffset_max` — the range is
        // the mode's blanking interval, so this is a misread, not an answer.
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
        assert_eq!(fake.auto_adjusts, 1);
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
        let values: Vec<i32> = fake.sets.iter().map(|(_, v)| *v).collect();
        assert_eq!(values, vec![352, 364, 363, 364]);
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
                    // A mode change mid-probe invalidates everything measured.
                    if !aborted {
                        aborted = true;
                        aligner.abort("the video mode changed");
                    }
                }
                Step::AutoAdjust => {
                    fake.auto_adjust();
                    aligner.config_arrived();
                }
                Step::Wait => {}
                Step::Failed(why) => {
                    assert_eq!(why, "the video mode changed");
                    assert_eq!(fake.at_h, 368, "the probe was not left on screen");
                    assert_eq!(fake.auto_adjusts, 1);
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
        assert_eq!(
            fake.auto_adjusts, 0,
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
