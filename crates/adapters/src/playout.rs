//! The camera's playout (C4, S1; `docs/_INDEX.md` Owed 89; `_FINDINGS.md` F-039): a small queue of
//! the camera's decoded frames and audio, played on a clock slaved to the camera's PTS. Before C4 the
//! camera slot held the newest frame and each tick showed it, so any arrival jitter at the input aired
//! 1:1 (Owed 81): a late-then-burst delivery from Cloudflare repeated frames while late and overwrote
//! all but one of the burst. The audio ring was played first in, first out, so the same burst left it
//! high (audio trailing the picture) or skipped it, and the late ticks padded it.
//!
//! **The rule.** The queue holds frames in PTS order. The clock maps the camera's media time onto the
//! programme's tick time: it anchors at the first tick that sees media, `PLAYOUT_TARGET_MS` behind the
//! newest arrival, and from then the media time due at a tick advances with the tick time. Each tick
//! airs the newest frame whose PTS is due and drops the older due ones; with no new frame due it keeps
//! the frame on air (a repeat). So a frame up to the target late still airs on time: a tick repeats
//! only on a real underrun (the frame due has not arrived), and a frame is dropped only when a newer
//! one is due at the same tick (an underrun's late frames, a source faster than the programme) or the
//! queue overflows. Audio is taken from the same clock: each period plays the audio stamped for that
//! period, the noise filler where none arrived, and audio that arrives after its period has played is
//! dropped (late).
//!
//! **Re-anchoring** (the numbers are `domain::config`'s): a new camera generation starts a fresh clock
//! and drops what the old one queued (never carried across); the first arrival after a gap longer than
//! `PLAYOUT_REANCHOR_GAP_MS` re-anchors, and so does an arrival more than `PLAYOUT_CAP_MS` ahead of the
//! clock or behind it (the camera's clock drifting against the tick grid, or a jump in its PTS). A
//! tick whose time is earlier than the anchor's (a second programme on the same store) starts a fresh
//! clock too.
//!
//! **Bounds.** The queue's stamped frames span at most `PLAYOUT_CAP_MS` and number at most
//! `PLAYOUT_MAX_FRAMES`, and the audio ring holds at most `AUDIO_RING_BYTES`: past either, the oldest
//! go, counted (overflow).
//!
//! **No timer, no thread** (AGENTS.md §4): the programme's tick calls [`Playout::tick`] and
//! [`Playout::take_audio_due`] with its own tick time, and the input's sample callbacks push. Nothing
//! here reads a clock or decides a phase: the domain's phase machine still reads decode time
//! (`InputFrame`), and the programme still picks Emit, Repeat or a slate.
//!
//! **Unstamped media** (the programme's own tests, a frame with no PTS) keeps the old slot's rule: an
//! unstamped frame is due at once and the newest wins (it drops the stamped queue and the clock), and
//! a ring whose oldest chunk is unstamped is played first in, first out.
//!
//! **Counts** ([`Counts`]) are raw: the programme hands each tick's to `Stats::on_camera_tick`, which
//! keeps them only while the camera is on air (PR #60's slate rule).
use std::collections::VecDeque;
use std::fmt;

use domain::config::{
    PLAYOUT_AUDIO_SLACK_MS, PLAYOUT_CAP_MS, PLAYOUT_MAX_FRAMES, PLAYOUT_REANCHOR_GAP_MS, PLAYOUT_TARGET_MS,
};

use crate::frame_store::{AUDIO_FRAME_BYTES, AUDIO_RATE, AUDIO_RING_BYTES};

const NS_PER_MS: i128 = 1_000_000;
const NS_PER_S: i128 = 1_000_000_000;

fn ms_ns(ms: u64) -> i128 {
    i128::from(ms) * NS_PER_MS
}

/// The 48 kHz audio sample a media time (ns) falls in, floored.
fn sample_at(ns: i128) -> i128 {
    (ns * i128::from(AUDIO_RATE)).div_euclid(NS_PER_S)
}

/// The media time (ns) of a 48 kHz sample index, floored.
pub fn sample_ns(sample: i128) -> i128 {
    (sample * NS_PER_S).div_euclid(i128::from(AUDIO_RATE))
}

/// Whole samples in `bytes` of S16LE stereo.
fn samples_in(bytes: usize) -> i128 {
    (bytes / AUDIO_FRAME_BYTES) as i128
}

fn bytes_of(samples: i128) -> usize {
    samples as usize * AUDIO_FRAME_BYTES
}

/// The clock: media time `media` is due at programme time `at` (both ns), and both advance together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Clock {
    at: i128,
    media: i128,
}

impl Clock {
    fn due(&self, at: i128) -> i128 {
        self.media + (at - self.at)
    }
}

struct Frame<V> {
    /// The frame's media time (ns); `None`: unstamped, due at once.
    pts: Option<i128>,
    v: V,
}

/// Audio as pushed: whole S16LE stereo frames, `off` bytes of it already played or dropped.
struct Chunk {
    /// The sample index of `data[0]`; `None`: unstamped.
    start: Option<i128>,
    data: Vec<u8>,
    off: usize,
}

impl Chunk {
    fn len(&self) -> usize {
        self.data.len() - self.off
    }

    /// The sample index of the chunk's first byte not yet played.
    fn pos(&self) -> Option<i128> {
        self.start.map(|s| s + samples_in(self.off))
    }

    fn end(&self) -> Option<i128> {
        self.start.map(|s| s + samples_in(self.data.len()))
    }
}

/// What one tick or one audio take did. Raw: the programme keeps them only while the camera is on air.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Camera frames that left the queue without airing: due at a tick with a newer one (an
    /// underrun's late frames, a source faster than the programme), past the cap, a new generation's
    /// drop of the old queue, or an unstamped frame replaced before a tick.
    pub unaired: u64,
    /// 1 when the tick aired no new frame and kept the last one on air.
    pub repeated: u64,
    /// The clock re-anchored after a gap or on drift past the cap (a first anchor and a new
    /// generation's are not counted).
    pub reanchors: u64,
    /// Noise in place of audio the clock had due and the ring did not hold.
    pub audio_pad_bytes: u64,
    /// Audio dropped unplayed because its period had already played (it arrived late, or the clock
    /// moved past it).
    pub audio_late_bytes: u64,
    /// Audio dropped because the ring was full (`AUDIO_RING_BYTES`).
    pub audio_overflow_bytes: u64,
}

impl Counts {
    pub fn add(&mut self, o: &Counts) {
        self.unaired += o.unaired;
        self.repeated += o.repeated;
        self.reanchors += o.reanchors;
        self.audio_pad_bytes += o.audio_pad_bytes;
        self.audio_late_bytes += o.audio_late_bytes;
        self.audio_overflow_bytes += o.audio_overflow_bytes;
    }
}

/// One tick's picture.
#[derive(Debug, Clone, PartialEq)]
pub struct Tick<V> {
    /// The frame on air after this tick: the newest due, or the one already on air; `None` before any
    /// frame has aired.
    pub frame: Option<V>,
    /// A frame left the queue for air at this tick.
    pub fresh: bool,
    /// What the tick did, and what the pushes since the last tick dropped.
    pub counts: Counts,
}

/// One period of audio.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTake {
    pub bytes: Vec<u8>,
    /// The ring's real bytes in `bytes` (the rest is the filler).
    pub real: usize,
    pub counts: Counts,
}

/// The camera's playout queue and clock; `V` is a decoded frame (`gst::Sample` live).
pub struct Playout<V> {
    /// The camera generation whose media is queued; `None` before the first stamped push.
    gen: Option<u64>,
    video: VecDeque<Frame<V>>,
    /// The newest frame pushed, aired or not (`FrameStore::camera`).
    newest: Option<V>,
    /// Frames pushed so far, every generation's (`FrameStore::camera_written`).
    written: u64,
    /// The frame on air and its media time.
    on_air: Option<(Option<i128>, V)>,
    audio: VecDeque<Chunk>,
    audio_bytes: usize,
    clock: Option<Clock>,
    /// The next audio sample to play, once the paced audio has played.
    cursor: Option<i128>,
    /// The newest media time pushed since the last tick (video PTS or audio chunk start).
    newest_since: Option<i128>,
    /// The programme time of the last tick that saw an arrival.
    last_arrival_at: Option<i128>,
    /// The newest media time pushed in this generation (a jump back past the cap restarts the queue).
    newest_pts: Option<i128>,
    /// What the pushes since the last tick dropped (handed on with the next tick's counts).
    pending: Counts,
}

impl<V> Default for Playout<V> {
    fn default() -> Self {
        Self {
            gen: None,
            video: VecDeque::new(),
            newest: None,
            written: 0,
            on_air: None,
            audio: VecDeque::new(),
            audio_bytes: 0,
            clock: None,
            cursor: None,
            newest_since: None,
            last_arrival_at: None,
            newest_pts: None,
            pending: Counts::default(),
        }
    }
}

impl<V> fmt::Debug for Playout<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Playout")
            .field("gen", &self.gen)
            .field("frames", &self.video.len())
            .field("audio_bytes", &self.audio_bytes)
            .field("clock", &self.clock)
            .field("on_air", &self.on_air.is_some())
            .finish_non_exhaustive()
    }
}

impl<V: Clone> Playout<V> {
    /// A decoded camera frame of generation `gen` (`None`: a caller with no generation, the
    /// programme's tests), stamped `pts` ns of media time (`None`: due at once).
    pub fn push_video(&mut self, gen: Option<u64>, pts: Option<u64>, v: V) {
        self.enter(gen);
        self.written += 1;
        self.newest = Some(v.clone());
        let Some(t) = pts.map(i128::from) else {
            // The old slot's rule: due at once, and the newest wins. A stream with no stamps has no
            // clock, so the stamped queue goes with it.
            self.pending.unaired += self.video.len() as u64;
            self.video.clear();
            self.clock = None;
            self.cursor = None;
            self.video.push_back(Frame { pts: None, v });
            return;
        };
        self.discontinuity(t);
        // PTS order (a decoder hands frames out in it; an out-of-order one is placed, not appended).
        // Unstamped frames stay in front: they are due at once.
        let at = self
            .video
            .iter()
            .rposition(|f| f.pts.is_none_or(|p| p <= t))
            .map_or(0, |i| i + 1);
        self.video.insert(at, Frame { pts: Some(t), v });
        self.arrival(t);
        while self.video.len() > PLAYOUT_MAX_FRAMES || self.stamped_span() > ms_ns(PLAYOUT_CAP_MS) {
            self.video.pop_front();
            self.pending.unaired += 1;
        }
    }

    /// Audio of generation `gen`, whole S16LE stereo frames (the caller refuses anything else),
    /// stamped `pts` ns of media time (`None`: played first in, first out).
    pub fn push_audio(&mut self, gen: Option<u64>, pts: Option<u64>, bytes: &[u8]) {
        self.enter(gen);
        if bytes.is_empty() {
            return;
        }
        if let Some(t) = pts {
            self.discontinuity(i128::from(t));
        }
        let start = pts.map(|t| sample_at(i128::from(t)));
        // Two chunks whose stamps differ by rounding are one stream: the later starts where the
        // earlier ends.
        let start = match (start, self.audio.back().and_then(Chunk::end)) {
            (Some(s), Some(e)) if (s - e).abs() <= sample_at(ms_ns(PLAYOUT_AUDIO_SLACK_MS)) => Some(e),
            (s, _) => s,
        };
        if let Some(t) = pts {
            self.arrival(i128::from(t));
        }
        self.audio.push_back(Chunk {
            start,
            data: bytes.to_vec(),
            off: 0,
        });
        self.audio_bytes += bytes.len();
        let over = self.audio_bytes.saturating_sub(AUDIO_RING_BYTES);
        if over > 0 {
            self.drop_audio_front(over);
            self.pending.audio_overflow_bytes += over as u64;
        }
    }

    /// The programme's tick at `at_ns` (its frame's PTS): the clock's rules, then the picture.
    pub fn tick(&mut self, at_ns: u64) -> Tick<V> {
        let at = i128::from(at_ns);
        let mut counts = std::mem::take(&mut self.pending);
        if self.clock.is_some_and(|c| at < c.at) {
            self.clock = None;
            self.cursor = None;
            self.last_arrival_at = None;
        }
        if let Some(newest) = self.newest_since.take() {
            let gap = self
                .last_arrival_at
                .is_some_and(|t| at - t > ms_ns(PLAYOUT_REANCHOR_GAP_MS));
            self.last_arrival_at = Some(at);
            match self.clock {
                None => self.anchor(at, newest),
                Some(c) => {
                    let lead = newest - c.due(at);
                    let cap = ms_ns(PLAYOUT_CAP_MS);
                    if gap || lead > cap || lead < -cap {
                        self.anchor(at, newest);
                        counts.reanchors += 1;
                    }
                }
            }
        }
        let due = self.clock.map(|c| c.due(at));
        let mut aired = None;
        let mut popped = 0u64;
        while let Some(f) = self.video.front() {
            let ready = match f.pts {
                None => true,
                Some(p) => due.is_some_and(|d| p <= d),
            };
            if !ready {
                break;
            }
            aired = self.video.pop_front();
            popped += 1;
        }
        let fresh = aired.is_some();
        if let Some(f) = aired {
            counts.unaired += popped - 1;
            self.on_air = Some((f.pts, f.v));
        } else if self.on_air.is_some() {
            counts.repeated = 1;
        }
        Tick {
            frame: self.on_air.as_ref().map(|(_, v)| v.clone()),
            fresh,
            counts,
        }
    }

    /// One period of audio, `n` bytes, for the programme's tick at `at_ns`: the audio the clock has
    /// due then, with `pad` writing the filler wherever the ring has none. Unstamped audio (or none
    /// with no clock) is played first in, first out, the filler first.
    pub fn take_audio_due(&mut self, at_ns: u64, n: usize, pad: impl FnMut(&mut [u8])) -> AudioTake {
        let stamped = self.audio.front().map(|c| c.start.is_some());
        match (self.clock, stamped) {
            (_, Some(false)) => self.take_audio_fifo(n, pad),
            (None, Some(true)) => {
                let mut pad = pad;
                let mut bytes = vec![0u8; n];
                pad(&mut bytes);
                AudioTake {
                    bytes,
                    real: 0,
                    counts: Counts {
                        audio_pad_bytes: n as u64,
                        ..Counts::default()
                    },
                }
            }
            (None, None) => self.take_audio_fifo(n, pad),
            (Some(c), _) => self.take_paced(c.due(i128::from(at_ns)), n, pad),
        }
    }

    /// `n` bytes first in, first out: the ring's oldest `min(n, level)` bytes, after `n − level` bytes
    /// of `pad` when the ring holds less (the filler first: the bytes kept are the newest, so they stay
    /// at the end). The old ring's rule, whatever the stamps.
    pub fn take_audio_fifo(&mut self, n: usize, pad: impl FnOnce(&mut [u8])) -> AudioTake {
        let mut bytes = vec![0u8; n];
        let real = n.min(self.audio_bytes);
        let gap = n - real;
        let mut w = gap;
        while w < n {
            let Some(c) = self.audio.front_mut() else { break };
            let k = (n - w).min(c.len());
            bytes[w..w + k].copy_from_slice(&c.data[c.off..c.off + k]);
            c.off += k;
            w += k;
            if c.len() == 0 {
                self.audio.pop_front();
            }
        }
        self.audio_bytes -= real;
        pad(&mut bytes[..gap]);
        AudioTake {
            bytes,
            real,
            counts: Counts {
                audio_pad_bytes: gap as u64,
                ..Counts::default()
            },
        }
    }

    /// Empties the ring; the bytes it dropped.
    pub fn clear_audio(&mut self) -> usize {
        let dropped = self.audio_bytes;
        self.audio.clear();
        self.audio_bytes = 0;
        dropped
    }

    pub fn audio_level(&self) -> usize {
        self.audio_bytes
    }

    /// The newest frame pushed, aired or not.
    pub fn newest(&self) -> Option<&V> {
        self.newest.as_ref()
    }

    /// Frames pushed so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Frames queued, not yet aired or dropped.
    pub fn queued(&self) -> usize {
        self.video.len()
    }

    /// The media time (ns) the clock has due at programme time `at_ns`; `None` with no clock.
    pub fn due_ns(&self, at_ns: u64) -> Option<i128> {
        self.clock.map(|c| c.due(i128::from(at_ns)))
    }

    /// The media time (ns) of the frame on air, when it was stamped.
    pub fn on_air_pts(&self) -> Option<i128> {
        self.on_air.as_ref().and_then(|(p, _)| *p)
    }

    /// The media time (ns) the next paced audio period starts at, once one has played.
    pub fn audio_cursor_ns(&self) -> Option<i128> {
        self.cursor.map(sample_ns)
    }

    /// The media time (ns) just past the ring's newest stamped audio.
    pub fn audio_end_ns(&self) -> Option<i128> {
        self.audio.back().and_then(Chunk::end).map(sample_ns)
    }

    fn take_paced(&mut self, due: i128, n: usize, mut pad: impl FnMut(&mut [u8])) -> AudioTake {
        let mut counts = Counts::default();
        let n_s = samples_in(n);
        let due_s = sample_at(due);
        let cur = match self.cursor {
            Some(k) if (k - due_s).abs() <= sample_at(ms_ns(PLAYOUT_AUDIO_SLACK_MS)) => k,
            _ => due_s,
        };
        counts.audio_late_bytes += self.drop_audio_before(cur) as u64;
        let mut bytes = vec![0u8; n];
        let (mut w, mut real) = (0i128, 0usize);
        while w < n_s {
            let here = cur + w;
            match self.audio.front().and_then(Chunk::pos) {
                // A chunk that starts before the play position (stamps that overlap past the slack):
                // its overlap is late.
                Some(pos) if pos < here => {
                    counts.audio_late_bytes += self.drop_audio_before(here) as u64;
                }
                Some(pos) if pos == here => {
                    let Some(c) = self.audio.front_mut() else { break };
                    let k = samples_in(c.len()).min(n_s - w);
                    let (from, to) = (bytes_of(w), bytes_of(w + k));
                    bytes[from..to].copy_from_slice(&c.data[c.off..c.off + (to - from)]);
                    c.off += to - from;
                    if c.len() == 0 {
                        self.audio.pop_front();
                    }
                    self.audio_bytes -= to - from;
                    real += to - from;
                    w += k;
                }
                next => {
                    let gap = next.map_or(n_s - w, |pos| (pos - here).min(n_s - w));
                    pad(&mut bytes[bytes_of(w)..bytes_of(w + gap)]);
                    counts.audio_pad_bytes += bytes_of(gap) as u64;
                    w += gap;
                }
            }
        }
        // A length that is not whole samples: the filler ends it.
        if bytes_of(n_s) < n {
            pad(&mut bytes[bytes_of(n_s)..]);
            counts.audio_pad_bytes += (n - bytes_of(n_s)) as u64;
        }
        self.cursor = Some(cur + n_s);
        AudioTake { bytes, real, counts }
    }

    /// Drops the stamped audio before sample `cur` (stopping at an unstamped chunk); the bytes dropped.
    fn drop_audio_before(&mut self, cur: i128) -> usize {
        let mut dropped = 0;
        while let Some(c) = self.audio.front_mut() {
            let (Some(pos), Some(end)) = (c.pos(), c.end()) else {
                break;
            };
            if end <= cur {
                dropped += c.len();
                self.audio.pop_front();
            } else if pos < cur {
                let k = bytes_of(cur - pos);
                c.off += k;
                dropped += k;
                break;
            } else {
                break;
            }
        }
        self.audio_bytes -= dropped;
        dropped
    }

    fn drop_audio_front(&mut self, mut n: usize) {
        while n > 0 {
            let Some(c) = self.audio.front_mut() else { break };
            let k = n.min(c.len());
            c.off += k;
            n -= k;
            self.audio_bytes -= k;
            if c.len() == 0 {
                self.audio.pop_front();
            }
        }
    }

    /// A new camera generation: a fresh clock, and the old one's queue dropped (its frames counted as
    /// unaired; its audio the input's loss policy has already emptied, or this drops it uncounted).
    /// The first generation keeps what unstamped pushes queued before it.
    fn enter(&mut self, gen: Option<u64>) {
        let Some(g) = gen else { return };
        if self.gen == Some(g) {
            return;
        }
        if self.gen.is_some() {
            self.restart();
        } else {
            self.unanchor();
        }
        self.gen = Some(g);
    }

    /// A stamp more than the cap behind the newest of its generation: the camera's PTS jumped back
    /// (the queue would hold the new frames behind the old ones, and the span bound would drop the
    /// new). The queue restarts as for a new generation, counted as a re-anchor.
    fn discontinuity(&mut self, t: i128) {
        if self.newest_pts.is_some_and(|n| t < n - ms_ns(PLAYOUT_CAP_MS)) {
            self.restart();
            self.pending.reanchors += 1;
        }
        self.newest_pts = Some(self.newest_pts.map_or(t, |n| n.max(t)));
    }

    /// Drops the queue (counted) and the audio, and starts a fresh clock.
    fn restart(&mut self) {
        self.pending.unaired += self.video.len() as u64;
        self.video.clear();
        self.clear_audio();
        self.unanchor();
    }

    fn unanchor(&mut self) {
        self.clock = None;
        self.cursor = None;
        self.newest_since = None;
        self.last_arrival_at = None;
        self.newest_pts = None;
    }

    fn arrival(&mut self, t: i128) {
        self.newest_since = Some(self.newest_since.map_or(t, |n| n.max(t)));
    }

    fn anchor(&mut self, at: i128, newest: i128) {
        self.clock = Some(Clock {
            at,
            media: newest - ms_ns(PLAYOUT_TARGET_MS),
        });
    }

    /// The media span of the stamped frames queued.
    fn stamped_span(&self) -> i128 {
        let first = self.video.iter().find_map(|f| f.pts);
        let last = self.video.back().and_then(|f| f.pts);
        match (first, last) {
            (Some(a), Some(b)) => b - a,
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    //! The rule's own statement is the oracle (`expected`): frame k is due `PLAYOUT_TARGET_MS` after
    //! the clock puts it, the clock anchored at the first tick that sees media, the target behind the
    //! newest frame that tick saw; a frame airs at the first tick that both has it and has it due; a
    //! tick with several airs the newest and drops the rest, and a tick with none, a frame on air,
    //! repeats. It runs on the schedule alone, never on the queue. Every number is from config, the
    //! schedule, or the programme's tick grid.
    use super::*;
    use domain::config::{OutputProfile, CAMERA_MAX_FPS};

    const PROFILES: [OutputProfile; 2] = [OutputProfile::P720, OutputProfile::P1080];
    const NS: u64 = 1_000_000_000;
    const MS: u64 = 1_000_000;
    /// The camera's media clock shares nothing with the programme's: its frame 0 is stamped here.
    const MEDIA_BASE_NS: u64 = 7_003_000_000;
    /// A frame reaches the queue this long after its slot on the source's grid: inside a tick.
    const DELIVERY_NS: u64 = 5 * MS;
    /// The test's filler byte: a padded sample reads `u32::MAX`, never a real sample's label.
    const PAD: u8 = 0xFF;
    const TARGET_NS: u64 = PLAYOUT_TARGET_MS * MS;

    /// The programme's tick `i` (gst_programme's `at`).
    fn at(fps: u64, i: u64) -> u64 {
        i * NS / fps
    }

    fn programme_fps(p: OutputProfile) -> u64 {
        u64::from(p.fps)
    }

    #[derive(Debug, Clone, Copy)]
    struct Arrival {
        /// Programme time it reaches the queue.
        at: u64,
        gen: u64,
        k: u64,
        pts: u64,
        src_fps: u64,
    }

    impl Arrival {
        fn id(&self) -> u64 {
            self.gen * 1_000_000 + self.k
        }

        /// The 48 kHz sample its audio starts at (the source's rate divides 48 kHz: exact).
        fn first_sample(&self) -> u64 {
            (self.pts * u64::from(AUDIO_RATE) + NS / 2) / NS
        }

        /// Its audio: the source's samples for the frame, each labelled with its own sample index.
        fn audio(&self) -> Vec<u8> {
            let spf = u64::from(AUDIO_RATE) / self.src_fps;
            (self.first_sample()..self.first_sample() + spf)
                .flat_map(|s| (s as u32).to_le_bytes())
                .collect()
        }
    }

    /// A camera at `src_fps`: frames `ks` stamped `media_base` plus their slot, each reaching the
    /// queue `DELIVERY_NS` after its slot on the programme's clock from `arrive_base`.
    fn source(
        gen: u64,
        src_fps: u64,
        ks: std::ops::Range<u64>,
        media_base: u64,
        arrive_base: u64,
    ) -> Vec<Arrival> {
        assert_eq!(
            u64::from(AUDIO_RATE) % src_fps,
            0,
            "a source rate that divides 48 kHz"
        );
        ks.map(|k| {
            let slot = k * NS / src_fps;
            Arrival {
                at: arrive_base + slot + DELIVERY_NS,
                gen,
                k,
                pts: media_base + slot,
                src_fps,
            }
        })
        .collect()
    }

    /// The fake camera's `POST /burst?ms=N`: what would arrive in `[from, from + ms)` is held and
    /// arrives at its end, in order, with its own stamps.
    fn hold(mut arrivals: Vec<Arrival>, from: u64, ms: u64) -> Vec<Arrival> {
        let until = from + ms * MS;
        for a in &mut arrivals {
            if (from..until).contains(&a.at) {
                a.at = until;
            }
        }
        arrivals.sort_by_key(|a| a.at);
        arrivals
    }

    /// PR #59/#60's bursts: frames leave in bursts of `sizes`, each burst when its newest frame is
    /// due, so a frame is held back 0 to `size - 1` periods.
    fn bursts(src_fps: u64, sizes: &[u64]) -> Vec<Arrival> {
        let total = sizes.iter().sum();
        let mut out = source(1, src_fps, 0..total, MEDIA_BASE_NS, 0);
        let mut k = 0;
        for &size in sizes {
            let last = out[(k + size - 1) as usize].at;
            for a in &mut out[k as usize..(k + size) as usize] {
                a.at = last;
            }
            k += size;
        }
        out
    }

    #[derive(Debug, Default, Clone)]
    struct Rec {
        aired: Option<u64>,
        fresh: bool,
        counts: Counts,
        /// The audio's first real sample minus the frame on air, ns; `None` when the period opens on
        /// the filler or nothing is on air.
        av: Option<i128>,
        /// Gens pushed before this tick.
        pushed_gens: Vec<u64>,
        /// The real audio samples' labels this tick played.
        labels: Vec<u32>,
    }

    struct Run {
        p: Playout<u64>,
        recs: Vec<Rec>,
        pushed: u64,
    }

    impl Run {
        /// The counts while the camera sends: to the tick the last frame airs (the run's ticks after
        /// it repeat because the source has stopped, which no schedule here is about).
        fn total(&self) -> Counts {
            let last = self.recs.iter().rposition(|r| r.fresh).map_or(0, |i| i + 1);
            let mut c = Counts::default();
            for r in &self.recs[..last] {
                c.add(&r.counts);
            }
            c
        }

        fn all(&self) -> Counts {
            let mut c = Counts::default();
            for r in &self.recs {
                c.add(&r.counts);
            }
            c
        }

        /// The tick the last frame airs: the end of the window the camera sends in.
        fn last_air(&self) -> usize {
            self.recs.iter().rposition(|r| r.fresh).expect("a frame aired")
        }

        fn first_air(&self) -> Option<usize> {
            self.recs.iter().position(|r| r.fresh)
        }

        fn fresh_ticks(&self) -> u64 {
            self.recs.iter().filter(|r| r.fresh).count() as u64
        }

        /// Every frame pushed is aired, dropped (counted) or still queued: none is lost uncounted.
        fn assert_conserved(&self, name: &str) {
            let c = self.all();
            assert_eq!(
                self.fresh_ticks() + c.unaired + self.p.queued() as u64,
                self.pushed,
                "{name}: aired + unaired + queued"
            );
        }
    }

    /// The programme's ticks over `arrivals`: each tick pushes what has arrived by its time, ticks,
    /// then takes its audio period.
    fn run(fps: u64, arrivals: &[Arrival], ticks: u64) -> Run {
        let mut p = Playout::default();
        let period_bytes = (u64::from(AUDIO_RATE) / fps) as usize * AUDIO_FRAME_BYTES;
        let (mut next, mut recs) = (0, Vec::new());
        for i in 0..ticks {
            let mut pushed_gens = Vec::new();
            while next < arrivals.len() && arrivals[next].at <= at(fps, i) {
                let a = arrivals[next];
                p.push_video(Some(a.gen), Some(a.pts), a.id());
                p.push_audio(Some(a.gen), Some(a.pts), &a.audio());
                pushed_gens.push(a.gen);
                next += 1;
            }
            let t = p.tick(at(fps, i));
            let mut counts = t.counts;
            let take = p.take_audio_due(at(fps, i), period_bytes, |b| b.fill(PAD));
            assert_eq!(take.bytes.len(), period_bytes);
            counts.add(&take.counts);
            let labels: Vec<u32> = take
                .bytes
                .chunks_exact(4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
                .filter(|&l| l != u32::MAX)
                .collect();
            assert_eq!(
                labels.len() * 4,
                take.real,
                "tick {i}: the real bytes are the labelled ones"
            );
            let first = u32::from_le_bytes([take.bytes[0], take.bytes[1], take.bytes[2], take.bytes[3]]);
            let av = (first != u32::MAX)
                .then(|| sample_ns(i128::from(first)))
                .zip(p.on_air_pts())
                .map(|(a, v)| a - v);
            recs.push(Rec {
                aired: t.frame,
                fresh: t.fresh,
                counts,
                av,
                pushed_gens,
                labels,
            });
        }
        Run {
            p,
            recs,
            pushed: next as u64,
        }
    }

    /// The rule on the schedule alone (one generation, no re-anchor): (repeats, unaired) from the
    /// first frame's air to the last's, and the tick the first frame airs.
    fn expected(fps: u64, arrivals: &[Arrival], ticks: u64) -> (u64, u64, Option<u64>) {
        let first = arrivals.iter().map(|a| a.at).min().expect("a schedule");
        let anchor = (0..ticks)
            .find(|&i| at(fps, i) >= first)
            .expect("anchored inside the run");
        let newest = arrivals
            .iter()
            .filter(|a| a.at <= at(fps, anchor))
            .map(|a| a.pts)
            .max()
            .expect("the anchor tick saw media");
        let air: Vec<(u64, u64)> = arrivals
            .iter()
            .filter_map(|a| {
                let due = i128::from(at(fps, anchor)) + i128::from(a.pts) - i128::from(newest)
                    + i128::from(TARGET_NS);
                let ready = due.max(i128::from(a.at));
                (0..ticks)
                    .find(|&i| i128::from(at(fps, i)) >= ready)
                    .map(|i| (i, a.pts))
            })
            .collect();
        let last_air = air.iter().map(|(t, _)| *t).max().unwrap_or(0);
        let (mut repeats, mut unaired, mut first_air) = (0, 0, None);
        for i in 0..=last_air {
            let n = air.iter().filter(|(t, _)| *t == i).count() as u64;
            if n > 0 {
                unaired += n - 1;
                first_air.get_or_insert(i);
            } else if first_air.is_some() {
                repeats += 1;
            }
        }
        (repeats, unaired, first_air)
    }

    /// The A/V check: every tick whose audio opens on real samples, with a frame on air, has the
    /// audio within one video period of the picture (the audio at the clock's due time, the picture
    /// the newest frame due), allowing the audio's slack either side. Returns the ticks checked.
    fn assert_av_in_step(name: &str, r: &Run, src_fps: u64) -> u64 {
        let slack = i128::from(PLAYOUT_AUDIO_SLACK_MS * MS);
        let period = i128::from(NS / src_fps);
        let mut checked = 0;
        for (i, rec) in r.recs.iter().enumerate() {
            if let Some(av) = rec.av {
                assert!(
                    (-slack..period + slack).contains(&av),
                    "{name}: tick {i}: audio {av} ns from the picture"
                );
                checked += 1;
            }
        }
        checked
    }

    // ---- steady sources ----

    /// A steady 30 fps camera on either profile: after the warm-up (the target, from the first
    /// tick that sees media), every frame airs once, in order, on its own tick: 0 repeats, 0
    /// dropped, no audio overflow, and the audio in step with the picture.
    #[test]
    fn a_steady_camera_airs_every_frame_once_with_no_repeat() {
        let mut checked = 0;
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let src = fps;
            let frames = 10 * src;
            let arrivals = source(1, src, 0..frames, MEDIA_BASE_NS, 0);
            let ticks = 11 * fps;
            let r = run(fps, &arrivals, ticks);
            let (rep, un, first) = expected(fps, &arrivals, ticks);
            assert_eq!((rep, un), (0, 0), "the rule on a steady camera");
            let c = r.total();
            assert_eq!((c.repeated, c.unaired), (0, 0), "{fps} fps");
            assert_eq!(r.first_air().map(|i| i as u64), first, "the warm-up");
            // The warm-up is the target: the first frame is seen at tick 1 and airs at the first tick
            // a target after it.
            let seen = (0..ticks).find(|&i| at(fps, i) >= arrivals[0].at).unwrap();
            assert_eq!(
                first,
                (0..ticks).find(|&i| at(fps, i) >= at(fps, seen) + TARGET_NS)
            );
            let aired: Vec<u64> = r
                .recs
                .iter()
                .filter(|x| x.fresh)
                .filter_map(|x| x.aired)
                .collect();
            let ids: Vec<u64> = arrivals.iter().map(Arrival::id).collect();
            assert_eq!(aired, ids, "{fps} fps: every frame once, in order");
            assert_eq!(c.audio_overflow_bytes + c.audio_late_bytes + c.reanchors, 0);
            assert_eq!(r.p.queued(), 0);
            r.assert_conserved("steady");
            let av = assert_av_in_step("steady", &r, src);
            assert!(av >= frames - 1, "the audio checked on (nearly) every tick: {av}");
            checked += 1;
        }
        eprintln!("steady profiles checked: {checked}");
        assert_eq!(checked, PROFILES.len());
    }

    /// Sources slower and faster than the programme: 25 fps repeats exactly the ticks it has no
    /// frame for (30 − 25 a second) and drops none; 60 fps airs a frame every tick and drops the
    /// other (the programme cannot show it), with no repeat.
    #[test]
    fn sources_at_25_and_60_fps_repeat_or_drop_only_by_rate() {
        let mut checked = 0;
        for profile in PROFILES {
            let fps = programme_fps(profile);
            for src in [25, 60] {
                let secs = 10;
                let arrivals = source(1, src, 0..secs * src, MEDIA_BASE_NS, 0);
                let ticks = (secs + 1) * fps;
                let r = run(fps, &arrivals, ticks);
                let (rep, un, _) = expected(fps, &arrivals, ticks);
                let c = r.total();
                assert_eq!((c.repeated, c.unaired), (rep, un), "{src} fps on {fps}: the rule");
                // The window from the first frame's air to the last's: the schedule's own count.
                let first = r.first_air().unwrap();
                let last = r.recs.iter().rposition(|x| x.fresh).unwrap();
                let window = (last - first + 1) as u64;
                if src < fps {
                    assert_eq!(c.unaired, 0, "{src} fps: nothing to drop");
                    assert_eq!(r.fresh_ticks(), secs * src, "{src} fps: every frame airs");
                    assert_eq!(
                        c.repeated,
                        window - secs * src,
                        "{src} fps: the ticks with no frame"
                    );
                    assert!(
                        c.repeated >= secs * (fps - src) - 1,
                        "{src} fps: {} repeats",
                        c.repeated
                    );
                } else {
                    assert_eq!(c.repeated, 0, "{src} fps: a frame every tick");
                    assert_eq!(r.fresh_ticks(), window, "{src} fps");
                    assert_eq!(
                        c.unaired,
                        secs * src - window,
                        "{src} fps: the frames between ticks"
                    );
                }
                assert_eq!(c.audio_overflow_bytes, 0);
                r.assert_conserved("rate");
                assert!(assert_av_in_step("rate", &r, src) > 0);
                checked += 1;
            }
        }
        eprintln!("source rates checked: {checked}");
        assert_eq!(checked, 2 * PROFILES.len());
    }

    // ---- bursts ----

    /// The hold the harness case runs (`camera-burst.sh`: the fake camera holds 300 ms every 10 s,
    /// 30 fps), three times: each hold is late by its length less the target, so its first frames
    /// are an underrun: the ticks they were due on repeat, and the release tick airs the newest due
    /// and drops the late. At this schedule's phase (a frame 5 ms after its tick, so the anchor sits
    /// 28.3 ms behind it) the rule gives 3 repeats and 3 drops a hold. A hold under the target costs
    /// nothing. No re-anchor (the hold is under the cap), no audio overflow.
    #[test]
    fn a_held_camera_repeats_only_the_ticks_its_late_frames_were_due() {
        let mut checked = 0;
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let src = fps;
            let secs = 32;
            for (hold_ms, per_hold) in [
                (300, Some(3)),
                (PLAYOUT_TARGET_MS - 50, Some(0)),
                (PLAYOUT_TARGET_MS, None),
            ] {
                let mut arrivals = source(1, src, 0..secs * src, MEDIA_BASE_NS, 0);
                let holds: Vec<u64> = (1..=3).map(|n| n * 10 * NS + 10 * MS).collect();
                for &h in &holds {
                    arrivals = hold(arrivals, h, hold_ms);
                }
                let ticks = (secs + 1) * fps;
                let r = run(fps, &arrivals, ticks);
                let (rep, un, _) = expected(fps, &arrivals, ticks);
                let c = r.total();
                assert_eq!(
                    (c.repeated, c.unaired),
                    (rep, un),
                    "{hold_ms} ms on {fps}: the rule"
                );
                assert_eq!(
                    rep, un,
                    "each repeated tick's slot is made up by a drop at the release"
                );
                if let Some(n) = per_hold {
                    // The brief's 150 ms target (pinned in config) gives the hand count.
                    assert_eq!(c.repeated, 3 * n, "{hold_ms} ms: {n} repeats a hold");
                }
                assert_eq!(c.reanchors, 0, "a hold under the cap keeps the clock");
                assert_eq!(c.audio_overflow_bytes, 0, "{hold_ms} ms: the ring holds the hold");
                if hold_ms > PLAYOUT_TARGET_MS {
                    assert!(
                        c.audio_pad_bytes > 0 && c.audio_late_bytes > 0,
                        "the underrun pads, the late audio drops"
                    );
                    // Repeats under 2 % of the on-air ticks: the harness case's verdict.
                    let on_air = (ticks as usize - r.first_air().unwrap()) as u64;
                    assert!(
                        c.repeated * 50 < on_air,
                        "{} repeats in {on_air} ticks",
                        c.repeated
                    );
                }
                r.assert_conserved("hold");
                // A/V: every tick the audio is real it is in step with the picture, the release tick
                // included (N = 1: the release tick re-syncs both, the audio's late part dropped).
                assert!(assert_av_in_step("hold", &r, src) > 0);
                for &h in &holds {
                    let release = (0..ticks).find(|&i| at(fps, i) >= h + hold_ms * MS).unwrap() as usize;
                    assert!(
                        r.recs[release].av.is_some(),
                        "{hold_ms} ms: the release tick plays real audio"
                    );
                }
                checked += 1;
            }
        }
        eprintln!("hold schedules checked: {checked}");
        assert_eq!(checked, 3 * PROFILES.len());
    }

    /// PR #59/#60's bursts (frames held back 0, 1 or 2 periods, leaving in bursts of 1 to 3, the
    /// mixed shape from `stats.rs`): the widest hold-back (two periods) is under the target, so the
    /// queue absorbs every burst: 0 repeats and 0 drops, where the newest-frame slot dropped 210.
    #[test]
    fn bursts_of_one_to_three_frames_cost_nothing() {
        let mixed: Vec<u64> = [1, 2, 3, 1, 1, 2, 1, 3, 2, 1].repeat(30);
        let mut checked = 0;
        for profile in PROFILES {
            let fps = programme_fps(profile);
            assert!(2 * NS / fps < TARGET_NS, "two periods inside the target");
            for (name, sizes) in [
                ("pairs", vec![2; 150]),
                ("threes", vec![3; 100]),
                ("mixed", mixed.clone()),
            ] {
                let arrivals = bursts(fps, &sizes);
                let ticks = arrivals.len() as u64 + fps;
                let r = run(fps, &arrivals, ticks);
                let (rep, un, _) = expected(fps, &arrivals, ticks);
                assert_eq!((rep, un), (0, 0), "{name}: the rule");
                let c = r.total();
                assert_eq!(
                    (c.repeated, c.unaired, c.audio_overflow_bytes),
                    (0, 0, 0),
                    "{name} at {fps}"
                );
                r.assert_conserved(name);
                assert!(assert_av_in_step(name, &r, fps) > 0);
                checked += 1;
            }
        }
        eprintln!("burst schedules checked: {checked}");
        assert_eq!(checked, 3 * PROFILES.len());
    }

    // ---- re-anchoring ----

    /// A gap longer than the cap (the camera sends nothing for 600 ms, then comes back 100 ms later
    /// than before): the first tick after it re-anchors the clock, the target behind the newest
    /// arrival, and the camera then runs steady again. Every repeat is inside the gap; nothing drops.
    #[test]
    fn a_gap_longer_than_the_cap_re_anchors_then_runs_steady() {
        let mut checked = 0;
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let gap_ms = PLAYOUT_REANCHOR_GAP_MS + 100;
            let (before, after) = (2 * fps, 4 * fps);
            let skipped = gap_ms * fps / 1_000;
            let mut arrivals = source(1, fps, 0..before, MEDIA_BASE_NS, 0);
            // The same clock, the frames of the gap never sent, then 100 ms later on the wire.
            arrivals.extend(source(
                1,
                fps,
                before + skipped..before + skipped + after,
                MEDIA_BASE_NS,
                100 * MS,
            ));
            let ticks = (before + skipped + after) + fps;
            let r = run(fps, &arrivals, ticks);
            let c = r.total();
            assert_eq!(c.reanchors, 1, "{fps}: one re-anchor, after the gap");
            assert_eq!(c.unaired, 0);
            // The re-anchor's tick, and the clock it set: the target behind the newest arrival.
            let back = arrivals[before as usize];
            let tick = (0..ticks).find(|&i| at(fps, i) >= back.at).unwrap();
            let rec = &r.recs[tick as usize];
            assert_eq!(rec.counts.reanchors, 1, "the first tick after the gap re-anchors");
            let last_pre = r.recs[..tick as usize].iter().rposition(|x| x.fresh).unwrap();
            let first_post = tick as usize + r.recs[tick as usize..].iter().position(|x| x.fresh).unwrap();
            // The new anchor gives the whole target again: the first frame back airs a target after the
            // tick that saw it (with the gap's lateness kept, it would air 100 ms sooner).
            assert_eq!(
                Some(first_post as u64),
                (0..ticks).find(|&i| at(fps, i) >= at(fps, tick) + TARGET_NS),
                "the target again"
            );
            for (i, x) in r.recs[..=r.last_air()].iter().enumerate() {
                if x.counts.repeated > 0 {
                    assert!(
                        (last_pre..first_post).contains(&i),
                        "{fps}: tick {i} repeats outside the gap"
                    );
                }
            }
            let steady: u64 = r.recs[first_post..=r.last_air()]
                .iter()
                .map(|x| x.counts.repeated)
                .sum();
            assert_eq!(steady, 0, "steady after the gap");
            r.assert_conserved("gap");
            checked += 1;
        }
        eprintln!("gap runs checked: {checked}");
        assert_eq!(checked, PROFILES.len());
    }

    /// A jump in the camera's PTS past the cap, either way, mid-stream with no gap: the clock
    /// re-anchors once, the frames queued before the jump are dropped (counted), and every frame
    /// after it airs, in order, steady.
    #[test]
    fn a_jump_in_the_cameras_pts_re_anchors_either_way() {
        let mut checked = 0;
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let jump = 4 * PLAYOUT_CAP_MS * MS;
            for (name, base_after) in [("forward", MEDIA_BASE_NS + jump), ("back", MEDIA_BASE_NS - jump)] {
                let (before, after) = (2 * fps, 3 * fps);
                let mut arrivals = source(1, fps, 0..before, MEDIA_BASE_NS, 0);
                // The jump: the slots go on, the stamps move by `jump`.
                arrivals.extend(source(1, fps, before..before + after, base_after, 0));
                let ticks = before + after + fps;
                let r = run(fps, &arrivals, ticks);
                let c = r.total();
                assert_eq!(c.reanchors, 1, "{name}: one re-anchor");
                assert!(c.unaired > 0, "{name}: the queue before the jump is dropped");
                let post: Vec<u64> = arrivals[before as usize..].iter().map(Arrival::id).collect();
                let aired: Vec<u64> = r
                    .recs
                    .iter()
                    .filter(|x| x.fresh)
                    .filter_map(|x| x.aired)
                    .collect();
                assert!(
                    aired.ends_with(&post),
                    "{name}: every frame after the jump airs, in order"
                );
                let first_post = r
                    .recs
                    .iter()
                    .position(|x| x.fresh && x.aired == Some(post[0]))
                    .unwrap();
                let steady: u64 = r.recs[first_post..=r.last_air()]
                    .iter()
                    .map(|x| x.counts.repeated)
                    .sum();
                assert_eq!(steady, 0, "{name}: steady after the jump");
                r.assert_conserved(name);
                checked += 1;
            }
        }
        eprintln!("jumps checked: {checked}");
        assert_eq!(checked, 2 * PROFILES.len());
    }

    // ---- generations ----

    /// A reconnect (a new generation) while the old one's frames are queued: none of them airs
    /// after the new one's first push (they are dropped, counted), none of the old audio plays, the
    /// new one starts a fresh clock (not counted as a re-anchor) and runs steady. Its stamps carry
    /// on just above the old one's, so only the generation tells them apart. Also after a hold.
    #[test]
    fn a_new_generation_never_airs_the_old_ones_queue() {
        let mut checked = 0;
        for profile in PROFILES {
            let fps = programme_fps(profile);
            for held in [false, true] {
                let before = 3 * fps;
                let mut arrivals = source(1, fps, 0..before, MEDIA_BASE_NS, 0);
                if held {
                    arrivals = hold(arrivals, NS + 10 * MS, 300);
                }
                let next_pts = arrivals.last().unwrap().pts + 40 * MS;
                let switch = arrivals.last().unwrap().at + 2 * MS;
                let new = source(2, fps, 0..3 * fps, next_pts, switch - DELIVERY_NS);
                arrivals.extend(new.iter().copied());
                let ticks = arrivals.last().unwrap().at * fps / NS + fps;
                let r = run(fps, &arrivals, ticks);
                let c = r.total();
                let first2 = r.recs.iter().position(|x| x.pushed_gens.contains(&2)).unwrap();
                // Frames of gen 1 were still queued when gen 2 arrived (the premise).
                let gen1_aired = r.recs[..first2].iter().filter(|x| x.fresh).count() as u64;
                assert!(
                    gen1_aired < before,
                    "the premise: gen 1 still queued at the switch"
                );
                for (i, x) in r.recs[first2..].iter().enumerate() {
                    assert!(
                        !x.fresh || x.aired.is_some_and(|id| id / 1_000_000 == 2),
                        "tick {}: a gen-1 frame aired after gen 2 arrived",
                        first2 + i
                    );
                    let floor = new[0].first_sample() as u32;
                    assert!(
                        x.labels.iter().all(|&l| l >= floor),
                        "tick {}: gen-1 audio after the switch",
                        first2 + i
                    );
                }
                assert_eq!(
                    c.reanchors, 0,
                    "a new generation's clock is fresh, not a re-anchor"
                );
                let aired2: Vec<u64> = r
                    .recs
                    .iter()
                    .filter(|x| x.fresh)
                    .filter_map(|x| x.aired)
                    .filter(|id| id / 1_000_000 == 2)
                    .collect();
                assert_eq!(
                    aired2,
                    new.iter().map(Arrival::id).collect::<Vec<_>>(),
                    "gen 2 airs every frame"
                );
                let first_new_air = first2 + r.recs[first2..].iter().position(|x| x.fresh).unwrap();
                assert_eq!(
                    Some(first_new_air as u64),
                    (0..ticks).find(|&i| at(fps, i) >= at(fps, first2 as u64) + TARGET_NS),
                    "gen 2's own warm-up: a fresh clock"
                );
                let steady: u64 = r.recs[first_new_air..=r.last_air()]
                    .iter()
                    .map(|x| x.counts.repeated)
                    .sum();
                assert_eq!(steady, 0);
                r.assert_conserved("gen");
                checked += 1;
            }
        }
        eprintln!("generation switches checked: {checked}");
        assert_eq!(checked, 2 * PROFILES.len());
    }

    // ---- bounds ----

    /// The queue's bounds: stamped frames span at most the cap (a 60 fps burst of 40 frames keeps
    /// the newest cap's worth), frames whose stamps do not move are held to `PLAYOUT_MAX_FRAMES`,
    /// and the audio ring to `AUDIO_RING_BYTES`, the cap's worth: what goes is counted.
    #[test]
    fn the_queue_and_the_ring_are_bounded_and_count_what_they_drop() {
        let cap = PLAYOUT_CAP_MS * MS;
        // A 60 fps burst of 40 frames, no tick between.
        let mut p: Playout<u64> = Playout::default();
        let n = 40;
        for k in 0..n {
            p.push_video(Some(1), Some(MEDIA_BASE_NS + k * NS / CAMERA_MAX_FPS), k);
        }
        let kept = (cap * CAMERA_MAX_FPS / NS + 1) as usize;
        assert_eq!(p.queued(), kept, "the cap's span, both ends");
        // The first tick airs one (the anchor puts the oldest due) and counts the rest it drops: every
        // frame is aired, counted, or still queued.
        let t = p.tick(0);
        assert_eq!(
            1 + t.counts.unaired + p.queued() as u64,
            n,
            "the dropped are counted"
        );
        // Stamps that do not move: the frame count bounds it.
        let mut p: Playout<u64> = Playout::default();
        for k in 0..n {
            p.push_video(Some(1), Some(MEDIA_BASE_NS), k);
        }
        assert_eq!(p.queued(), PLAYOUT_MAX_FRAMES);
        // None is due at the first tick (all a target ahead of the clock): the count is the bound's.
        assert_eq!(p.tick(0).counts.unaired, n - PLAYOUT_MAX_FRAMES as u64);
        // The audio: 600 ms of it, no take.
        let ring = (PLAYOUT_CAP_MS as usize) * AUDIO_RATE as usize / 1_000 * AUDIO_FRAME_BYTES;
        assert_eq!(AUDIO_RING_BYTES, ring, "the ring is the cap's audio");
        let mut p: Playout<u64> = Playout::default();
        let chunk = vec![0u8; 1_600 * AUDIO_FRAME_BYTES];
        let mut pushed = 0;
        for k in 0..18 {
            p.push_audio(Some(1), Some(MEDIA_BASE_NS + k * NS / 30), &chunk);
            pushed += chunk.len();
        }
        assert_eq!(p.audio_level(), AUDIO_RING_BYTES);
        assert_eq!(
            p.tick(0).counts.audio_overflow_bytes,
            (pushed - AUDIO_RING_BYTES) as u64
        );
    }

    // ---- the old slot's rule, the empty case, a second programme ----

    /// Unstamped frames (the programme's own tests) keep the newest-frame slot: due at once, the
    /// newest wins (the replaced counted), a tick with nothing new repeats, and an unstamped frame
    /// drops a stamped queue and its clock. Unstamped audio plays first in, first out, filler first.
    #[test]
    fn unstamped_media_keeps_the_newest_frame_slot() {
        let mut p: Playout<u64> = Playout::default();
        for v in 1..=3 {
            p.push_video(None, None, v);
        }
        let t = p.tick(0);
        assert_eq!(
            (t.frame, t.fresh, t.counts.unaired, t.counts.repeated),
            (Some(3), true, 2, 0)
        );
        let t = p.tick(1);
        assert_eq!(
            (t.frame, t.fresh, t.counts.repeated),
            (Some(3), false, 1),
            "a repeat"
        );
        // A stamped queue, then an unstamped frame: the queue goes, the frame airs.
        p.push_video(Some(1), Some(MEDIA_BASE_NS), 4);
        p.push_video(Some(1), Some(MEDIA_BASE_NS + 33 * MS), 5);
        p.push_video(Some(1), None, 6);
        assert!(p.due_ns(2).is_none(), "the clock went with the stamps");
        let t = p.tick(2);
        assert_eq!((t.frame, t.counts.unaired), (Some(6), 2));
        // FIFO audio: 100 real bytes at the end of a 400-byte period.
        p.push_audio(None, None, &[7u8; 100]);
        let a = p.take_audio_due(3, 400, |b| b.fill(PAD));
        assert_eq!((a.real, a.counts.audio_pad_bytes), (100, 300));
        assert!(a.bytes[..300].iter().all(|&b| b == PAD) && a.bytes[300..].iter().all(|&b| b == 7));
        assert_eq!(p.written(), 6);
        assert_eq!(p.newest(), Some(&6));
    }

    /// Nothing pushed: a tick airs nothing and counts nothing (no frame on air is no repeat), and
    /// the audio is all filler, counted.
    #[test]
    fn an_empty_queue_airs_and_counts_nothing() {
        let mut p: Playout<u64> = Playout::default();
        for i in 0..3 {
            let t = p.tick(i * NS / 30);
            assert_eq!((t.frame, t.fresh, t.counts), (None, false, Counts::default()));
        }
        let a = p.take_audio_due(0, 6_400, |b| b.fill(PAD));
        assert_eq!((a.real, a.counts.audio_pad_bytes), (0, 6_400));
        assert_eq!(p.clear_audio(), 0);
    }

    /// A tick earlier than the clock's anchor (a second programme on the same store, its timeline
    /// from 0) starts a fresh clock at the next arrival, the target behind it, not counted.
    #[test]
    fn a_programme_timeline_that_restarts_starts_a_fresh_clock() {
        let fps = 30;
        let mut p: Playout<u64> = Playout::default();
        let a = source(1, fps, 0..60, MEDIA_BASE_NS, 0);
        let mut next = 0;
        for i in 0..60 {
            while next < a.len() && a[next].at <= at(fps, i) {
                p.push_video(Some(1), Some(a[next].pts), a[next].id());
                next += 1;
            }
            p.tick(at(fps, i));
        }
        let more = source(1, fps, 60..61, MEDIA_BASE_NS, 0);
        p.push_video(Some(1), Some(more[0].pts), more[0].id());
        let t = p.tick(0);
        assert_eq!(t.counts.reanchors, 0);
        assert_eq!(p.due_ns(0), Some(i128::from(more[0].pts) - i128::from(TARGET_NS)));
    }

    #[test]
    fn counts_add_field_by_field() {
        let a = Counts {
            unaired: 1,
            repeated: 2,
            reanchors: 3,
            audio_pad_bytes: 4,
            audio_late_bytes: 5,
            audio_overflow_bytes: 6,
        };
        let mut b = a;
        b.add(&a);
        assert_eq!(
            b,
            Counts {
                unaired: 2,
                repeated: 4,
                reanchors: 6,
                audio_pad_bytes: 8,
                audio_late_bytes: 10,
                audio_overflow_bytes: 12
            }
        );
    }
}
