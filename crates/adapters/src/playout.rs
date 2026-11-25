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
//! **Two streams.** The video and the audio are judged apart: their stamps can disagree (PR #113's
//! camera-flap leg showed one stream's stamps carry on across a camera pause while the other's
//! jumped, by the pause). Each keeps its own newest stamp. The audio is placed on the video's stamps:
//! while the video is fresh (it arrived since the last tick, or at it), a chunk more than
//! `PLAYOUT_STREAM_SKEW_MAX_MS` from the newest frame's stamp re-bases the audio onto it (the queued
//! audio and every later chunk move by the difference; counted as a re-anchor). Within that skew the
//! stamps are kept, so the audio stays in step with the picture.
//!
//! **Re-anchoring** (the numbers are `domain::config`'s): the clock anchors the target behind the
//! newest stamp of the later-arriving live stream (the lower of the two), so each keeps the whole
//! target. A new camera generation starts a fresh clock and drops what the old one queued (never
//! carried across). Counted re-anchors: the first arrival after a gap longer than
//! `PLAYOUT_REANCHOR_GAP_MS`; every live stream more than `PLAYOUT_CAP_MS` ahead of the clock (the
//! camera's clock drifting against the tick grid, or a jump forward in its stamps); an arrival more
//! than the cap behind it; a stream whose arrivals have all been past due for
//! `PLAYOUT_LATE_REANCHOR_MS` (a delivery-delay step with no catch-up, a slow camera clock: without
//! it every audio chunk would drop late for as long as the camera stayed behind). A stream the clock
//! had not taken into account that arrives short of the target anchors it again, uncounted (it
//! completes the first anchor). A jump back past the cap within one stream drops that stream's
//! queue. A tick whose time is earlier than the anchor's (a second programme on the same store)
//! starts a fresh clock too.
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
//! keeps them only while the camera is on air (PR #60's slate rule). The filler is counted only once
//! audio has arrived in the generation: a camera with no audio track has none to pad.
use std::collections::VecDeque;
use std::fmt;

use domain::config::{
    PLAYOUT_AUDIO_SLACK_MS, PLAYOUT_CAP_MS, PLAYOUT_EARLY_MARGIN_MS, PLAYOUT_EARLY_REANCHOR_MS,
    PLAYOUT_LATE_REANCHOR_MS, PLAYOUT_MAX_FRAMES, PLAYOUT_REANCHOR_GAP_MS, PLAYOUT_SETTLE_MS,
    PLAYOUT_SETTLE_SKIP_MS, PLAYOUT_SKEW_REVERT_MS, PLAYOUT_STREAM_SKEW_MAX_MS, PLAYOUT_TARGET_MS,
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
    /// It is due on the video's own stamps (`Playout::phase`).
    phased: bool,
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
    /// The sample index of `data[0]`, re-based onto the video's stamps; `None`: unstamped.
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
    /// The clock re-anchored (after a gap, past the cap either way, after a stream ran late for
    /// `PLAYOUT_LATE_REANCHOR_MS`), or the audio was re-based onto the video's stamps. A first anchor,
    /// a stream joining it, and a new generation's are not counted.
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

/// What a tick's arrivals do to the clock (`Playout::judge`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Judged {
    Keep,
    /// A stream the clock had not taken into account arrived short of the target: anchor again,
    /// uncounted (it completes the first anchor).
    Join,
    /// A rule fired: anchor again, counted.
    Reanchor,
}

/// The most notes held between two drains (`Playout::take_notes`); past it they are counted, not kept.
const NOTES_MAX: usize = 512;
/// Ticks traced after a clock event (`Note::trace`), so the stamps around it can be read.
const TRACE_TICKS: u32 = 20;
/// The frames of one clamped run traced each (`clamp_frame`, C4 fix round 3), enough for a pause of
/// 8.5 s at 60 fps; past them a camera stuck on one stamp would note every frame for good.
const CLAMP_TRACE_FRAMES: u32 = 512;

/// One clock event or one traced tick, for the log (C4 fix round 2: the review's re-review read a
/// re-anchor storm on the real chain that no modelled stamp shape gives; these lines read it). The
/// stamps are media times in ns, the audio's as placed on the video's (its raw stamp beside it).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// The rule(s) that fired, `+`-joined, or `tick` for a traced tick.
    pub what: String,
    /// The programme time of the tick (the last tick's, for an event at a push).
    pub at: Option<i128>,
    pub gen: Option<u64>,
    /// The media time the clock had due at `at` before the event, and after it.
    pub due_before: Option<i128>,
    pub due_after: Option<i128>,
    pub video_newest: Option<i128>,
    pub audio_newest: Option<i128>,
    /// The video's and the audio's newest arrival since the last tick, and how many pushes.
    pub video_since: Option<i128>,
    pub audio_since: Option<i128>,
    pub video_pushes: u32,
    pub audio_pushes: u32,
    /// The newest audio push's raw stamp (before the offset).
    pub audio_raw: Option<i128>,
    pub audio_offset_ns: i128,
    pub frames: usize,
    pub audio_bytes: usize,
    /// A traced tick: the frame it aired (its stamp), how many it popped, and whether it repeated.
    pub aired: Option<i128>,
    pub popped: u64,
    pub repeated: bool,
}

impl fmt::Display for Note {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn ms(v: Option<i128>) -> String {
            v.map_or_else(|| "-".into(), |n| format!("{:.1}", n as f64 / 1e6))
        }
        write!(
            f,
            "playout {} gen={} at_ms={} due_ms={}->{} video_ms={} audio_ms={} since_video_ms={} \
             since_audio_ms={} pushes={}/{} audio_raw_ms={} offset_ms={} frames={} audio_bytes={} \
             aired_ms={} popped={} repeated={}",
            self.what,
            self.gen.map_or_else(|| "-".into(), |g| g.to_string()),
            ms(self.at),
            ms(self.due_before),
            ms(self.due_after),
            ms(self.video_newest),
            ms(self.audio_newest),
            ms(self.video_since),
            ms(self.audio_since),
            self.video_pushes,
            self.audio_pushes,
            ms(self.audio_raw),
            ms(Some(self.audio_offset_ns)),
            self.frames,
            self.audio_bytes,
            ms(self.aired),
            self.popped,
            self.repeated,
        )
    }
}

/// One stream's arrivals (the video's or the audio's), for the clock's rules.
#[derive(Debug, Clone, Copy, Default)]
struct Stream {
    /// The newest media time pushed since the stream's last jump back (the audio's re-based).
    newest: Option<i128>,
    /// The newest pushed since the last tick.
    since: Option<i128>,
    /// The programme time of the last tick that saw an arrival of it.
    last_at: Option<i128>,
    /// The tick its current run of past-due arrivals started at.
    late_since: Option<i128>,
    /// The clock has taken it into account: it was live at the anchor, or has arrived since.
    anchored: bool,
    /// Pushes since the last tick (the notes' trace).
    pushes: u32,
}

impl Stream {
    fn arrive(&mut self, t: i128) {
        self.newest = Some(self.newest.map_or(t, |n| n.max(t)));
        self.since = Some(self.since.map_or(t, |n| n.max(t)));
        self.pushes += 1;
    }

    /// It arrived within the gap before `at`.
    fn live(&self, at: i128) -> bool {
        self.last_at
            .is_some_and(|l| at - l <= ms_ns(PLAYOUT_REANCHOR_GAP_MS))
    }

    /// Its stamps start again (a jump back past the cap, or a re-base).
    fn rebased(&mut self) {
        self.newest = None;
        self.since = None;
        self.late_since = None;
    }
}

/// The weight of one sample in the steady interleave's running average (`Playout::interleave`): 1/8,
/// a few frames' chunks, enough to smooth a 30 fps camera's ±20 ms frame-vs-chunk phase.
const INTERLEAVE_WEIGHT: i128 = 8;

/// A re-base settling (`Playout::settle`): the audio's stamps are read against the video's for
/// `PLAYOUT_SETTLE_SKIP_MS` and `PLAYOUT_SETTLE_MS` of audio after it, and the mean of what the skip
/// leaves goes back on the steady interleave. A return's first frames come out of an empty decoder
/// close behind their audio and fall back to the steady interleave over the next ~20 frames (a
/// re-base reads the interleave there, so its error would stand for the whole window), so the skip
/// is the transient's; the window is whole chunk-and-frame beats (`PLAYOUT_SETTLE_MS`). Counted in
/// the audio's own time, not in readings: a chunk is 21.3 ms on the chain's AAC grid, 33.3 on a
/// 30 fps test source.
#[derive(Debug, Clone, Copy)]
struct Settle {
    /// The re-based audio's start (ns on the placed stamps) when it began.
    from: i128,
    /// The sum (ns) and count of the readings past the skip.
    sum: i128,
    n: i128,
}

/// The video's stamps as pushed (raw) and as queued (placed). GstVideoDecoder never lets a stamp go
/// back: a frame whose PTS is below the last it put out leaves with that last PTS instead
/// (gstvideodecoder.c, "decreasing timestamp"). So when one frame's stamp is too far on (camera-flap's
/// returns on PR #113: tsdemux stamped the first new frame a pause past its capture, CI run
/// 45588726031), every frame after it repeats that stamp until their own pass it, a pause later, and
/// a clock slaved to the stamps sees a camera that stopped while frames keep coming (C1 on the real
/// chain: a re-anchor every ~330 ms). A repeated stamp is therefore no stamp: the frame is placed one
/// step (the camera's mean frame period, `VideoStamps::step`) after the last frame placed. When
/// the stamps advance again they are the frames' own, put on from where the placed ones reached
/// (`offset`), so the picture neither jumps nor stalls, and the audio, re-based onto the placed stamps
/// at the return, stays on them.
#[derive(Debug, Clone, Copy, Default)]
struct VideoStamps {
    raw: Option<i128>,
    placed: Option<i128>,
    /// The single-frame increments the raw stamps have moved by: their sum and count (`step`).
    period: (i128, i128),
    /// Added to the raw stamps (the clamped run's end moves it).
    offset: i128,
    /// The frames are arriving with a repeated stamp.
    clamped: bool,
    /// The frames of the clamped run so far (its trace's count).
    run: u32,
    /// What the offset has still to move by, since the last clamped run's end re-locked it (`slewed`).
    slew: i128,
    /// The move that re-lock asked for (its note).
    relocked: i128,
}

/// What `VideoStamps::place` saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Clamp {
    None,
    /// A repeated stamp after advancing ones: the run starts.
    Starts,
    /// The stamps advance past the repeated one: the run ends, the offset moved.
    Ends,
}

impl VideoStamps {
    /// The camera's mean frame period: what a clamped run's frames are placed apart by. A camera
    /// stamps off its own clock, a few ms either side of each slot (camera-flap's fake camera: 30.1 to
    /// 34.0 ms at 30 fps, CI run 45595996718), and a run is placed a pause long (147 frames), so one
    /// increment's error would be carried 147 times over: that run's first return moved the picture
    /// 37.8 ms off its sound by the clamp's end. The mean of the increments telescopes to the span
    /// over the count, so the jitter falls out.
    fn step(&self) -> Option<i128> {
        let (sum, n) = self.period;
        (n > 0).then(|| sum / n)
    }

    /// An increment `d` of the raw stamps, within the cap. Only a single frame's is a period: one
    /// half again past the mean is frames dropped between (a period twice over), and one under half
    /// of it shows the mean was never a period (a stall between the camera's first frames), so the
    /// mean starts over from it.
    fn measure(&mut self, d: i128) {
        match self.step() {
            Some(s) if 2 * d < s => self.period = (d, 1),
            Some(s) if 2 * d > 3 * s => {}
            _ => {
                self.period.0 += d;
                self.period.1 += 1;
            }
        }
    }

    /// The frame stamped `raw`, on the placed stamps. `audio` is the audio's offset (ns), when the
    /// stream has stamped audio: a clamped run's end re-locks the offset onto it (C4 fix round 3, the
    /// round-2 re-review's candidate 1). From there both raw stamp sets are true, so the two streams
    /// are on the same content exactly when their offsets are equal, and any difference is error: a
    /// placement the run counted that the stamps did not advance by (camera-flap's R7 and R12 on
    /// 028e8b3, a frame each), or the audio's re-base reading the interleave wrong. Unconditional:
    /// the move is made a little at a time (`slewed`), so it never reorders the queue.
    fn place(&mut self, raw: i128, audio: Option<i128>) -> (i128, Clamp) {
        let step = self.step().unwrap_or(0);
        let (placed, seen) = match (self.raw, self.placed) {
            (Some(r), Some(p)) if raw == r => {
                let seen = if self.clamped { Clamp::None } else { Clamp::Starts };
                self.run = if self.clamped { self.run + 1 } else { 1 };
                self.clamped = true;
                (p + step, seen)
            }
            (Some(r), Some(p)) if self.clamped && raw > r => {
                self.clamped = false;
                self.offset = p + step - raw;
                // The re-lock removes a frame's error between the two offsets, never a pause: an audio
                // offset further than `PLAYOUT_STREAM_SKEW_MAX_MS` from the picture's is not a content
                // offset (audio that stopped keeps its last re-base's, I-r3-1; one just re-based onto an
                // outlier is a pause off the true one until its stamps come back, CI run 45649360015:
                // the picture slewed back by a pause and the clock re-anchored for seconds). Refused
                // there; the settle puts the audio right.
                self.slew = audio
                    .map(|a| a - self.offset)
                    .filter(|m| m.abs() <= ms_ns(PLAYOUT_STREAM_SKEW_MAX_MS))
                    .unwrap_or(0);
                self.relocked = self.slew;
                (self.slewed(raw, p), Clamp::Ends)
            }
            // A stamp before the repeated one is no clamp's (GstVideoDecoder never lets a stamp go
            // back): an older frame inside the run (a held one let go late), so the run is broken,
            // not ended, and the stamps are not known true: no re-lock (a re-lock there moved a
            // camera's picture by the audio's whole re-base, a pause, half a step a frame).
            (Some(r), Some(p)) if self.clamped && raw < r => {
                self.clamped = false;
                (self.slewed(raw, p), Clamp::None)
            }
            (_, Some(p)) => (self.slewed(raw, p), Clamp::None),
            (_, None) => (raw + self.offset, Clamp::None),
        };
        // The run's end is no increment: its stamp is the frame's own, the one before it the
        // repeated one.
        if let Some(d) = self
            .raw
            .map(|r| raw - r)
            .filter(|d| seen != Clamp::Ends && (1..=ms_ns(PLAYOUT_CAP_MS)).contains(d))
        {
            self.measure(d);
        }
        self.raw = Some(raw);
        self.placed = Some(placed);
        (placed, seen)
    }

    /// `raw` on the offset, moved by as much of the re-lock's slew as one frame takes: never more than
    /// half the way back to the frame placed before it (`prev`), so the queue keeps its order and a
    /// tick gives up at most the one frame between, nor more than half a step on, so no frame is held
    /// over more than one tick for it. With no slew left it is `raw` on the offset.
    fn slewed(&mut self, raw: i128, prev: i128) -> i128 {
        let base = raw + self.offset;
        let back = ((base - prev) / 2).max(0);
        let on = (self.step().unwrap_or(0) / 2).max(0);
        let m = self.slew.clamp(-back, on);
        self.offset += m;
        self.slew -= m;
        base + m
    }
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
    /// The video's arrivals and the audio's, apart: their stamps can disagree (a pause one stream's
    /// stamps carry on across), and each is judged against the clock on its own.
    video_s: Stream,
    audio_s: Stream,
    /// The video's raw and placed stamps (a decoder's clamped run).
    video_t: VideoStamps,
    /// The programme time of the last tick that saw an arrival of either stream.
    last_arrival_at: Option<i128>,
    /// The tick the live streams' current run of early arrivals started at (`PLAYOUT_EARLY_MARGIN_MS`).
    early_since: Option<i128>,
    /// The programme time of the last tick.
    last_tick: Option<i128>,
    /// Samples added to the audio's stamps to put them on the video's (a re-base moves it).
    audio_offset: i128,
    /// The offset a provisional skew re-base replaced (`PLAYOUT_SKEW_REVERT_MS`), until it is undone.
    skew_prior: Option<i128>,
    /// The steady interleave (ns): a stamped audio chunk's start less the newest frame's stamp as it
    /// is pushed, while the video is fresh and they agree (an average, `INTERLEAVE_WEIGHT`): the
    /// re-base's own measure. A re-base puts the audio back on it, not on
    /// the video's newest: the chain's decoders hand the two out apart (the video ~55 ms behind its
    /// audio on camera-flap's log, CI run 45588726031), and a re-base on the newest made that its A/V.
    interleave: Option<i128>,
    /// A re-base settling (`Settle`): after `PLAYOUT_SETTLE_SKIP_MS` and `PLAYOUT_SETTLE_MS` more of
    /// audio, the mean of the readings past the skip is put back on the interleave. The interleave
    /// takes no reading meanwhile (they are on the re-base's offset, not the camera's).
    settle: Option<Settle>,
    /// Stamped audio has arrived in this generation (before it, the filler is not counted).
    audio_seen: bool,
    /// What the pushes since the last tick dropped (handed on with the next tick's counts).
    pending: Counts,
    /// The newest audio push's raw stamp (ns, before the offset), for the notes.
    audio_raw: Option<i128>,
    /// The clock events and traced ticks since the last drain (`take_notes`), and those past
    /// `NOTES_MAX` not kept.
    notes: Vec<Note>,
    notes_dropped: u64,
    /// Ticks still to trace after the last event.
    trace_left: u32,
    /// The last segment each stream's samples carried (video, audio), as `note_segment` saw it.
    segments: [Option<(u64, u64, u64)>; 2],
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
            video_s: Stream::default(),
            audio_s: Stream::default(),
            video_t: VideoStamps::default(),
            last_arrival_at: None,
            early_since: None,
            last_tick: None,
            audio_offset: 0,
            skew_prior: None,
            interleave: None,
            settle: None,
            audio_seen: false,
            pending: Counts::default(),
            audio_raw: None,
            notes: Vec::new(),
            notes_dropped: 0,
            trace_left: 0,
            segments: [None, None],
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
        let Some(raw) = pts.map(i128::from) else {
            // The old slot's rule: due at once, and the newest wins. A stream with no stamps has no
            // clock, so the stamped queue goes with it.
            if self.clock.is_some() || !self.video.is_empty() {
                self.note("unstamped_video", None);
            }
            self.pending.unaired += self.video.len() as u64;
            self.video.clear();
            self.clock = None;
            self.cursor = None;
            self.video.push_back(Frame { pts: None, v });
            return;
        };
        // A camera with no stamped audio has no audio offset: its run's end keeps its own. The
        // offset is judged where it is used (`VideoStamps::place`): one a pause off the picture's is
        // no content offset (C4 fix round 5).
        let audio = self
            .audio_s
            .newest
            .is_some()
            .then(|| sample_ns(self.audio_offset));
        let (t, clamp) = self.video_t.place(raw, audio);
        match clamp {
            Clamp::None => {}
            Clamp::Starts => self.note(&format!("video_clamped(stamp_ms={:.1})", raw as f64 / 1e6), None),
            Clamp::Ends => {
                let relocked = self.video_t.relocked;
                self.note(
                    &format!(
                        "video_unclamped(offset_ms={:.1},audio_offset_ms={},relock_ms={:.1})",
                        (self.video_t.offset + self.video_t.slew - relocked) as f64 / 1e6,
                        audio.map_or_else(|| "-".into(), |a| format!("{:.1}", a as f64 / 1e6)),
                        relocked as f64 / 1e6
                    ),
                    None,
                );
            }
        }
        // The decoder-side trace's playout half (C4 fix round 3): each frame of a clamped run and
        // its end, with the placement given and the audio clock then (the note's own fields).
        let run = self.video_t.run + u32::from(clamp == Clamp::Ends);
        if (self.video_t.clamped || clamp == Clamp::Ends) && run <= CLAMP_TRACE_FRAMES {
            let n = self.snapshot(
                &format!(
                    "clamp_frame(n={run},raw_ms={:.1},placed_ms={:.1}{})",
                    raw as f64 / 1e6,
                    t as f64 / 1e6,
                    if clamp == Clamp::Ends { ",end" } else { "" }
                ),
                None,
            );
            self.keep(n);
        }
        // A jump back past the cap within the video: the queue would hold the new frames behind the
        // old ones (and the span bound would drop the new), so the old go, counted. The clock's own
        // rules re-anchor at the next tick (the frame is more than the cap behind it).
        if self.video_s.newest.is_some_and(|n| t < n - ms_ns(PLAYOUT_CAP_MS)) {
            self.note(&format!("video_jump_back(to_ms={:.1})", t as f64 / 1e6), None);
            self.pending.unaired += self.video.len() as u64;
            self.video.clear();
            self.video_s.rebased();
        }
        // PTS order (a decoder hands frames out in it; an out-of-order one is placed, not appended).
        // Unstamped frames stay in front: they are due at once.
        let at = self
            .video
            .iter()
            .rposition(|f| f.pts.is_none_or(|p| p <= t))
            .map_or(0, |i| i + 1);
        self.video.insert(at, Frame { pts: Some(t), v });
        self.video_s.arrive(t);
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
            self.audio_raw = Some(i128::from(t));
        } else if gen.is_some() && self.audio.back().is_none_or(|c| c.start.is_some()) && self.audio_seen {
            self.note("unstamped_audio", None);
        }
        let start = pts.map(|t| self.place_audio(sample_at(i128::from(t))));
        // Two chunks whose stamps differ by rounding are one stream: the later starts where the
        // earlier ends.
        let start = match (start, self.audio.back().and_then(Chunk::end)) {
            (Some(s), Some(e)) if (s - e).abs() <= sample_at(ms_ns(PLAYOUT_AUDIO_SLACK_MS)) => Some(e),
            (s, _) => s,
        };
        if let Some(s) = start {
            self.audio_s.arrive(sample_ns(s));
        }
        self.audio_seen = true;
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
            self.note("timeline_reset", Some(at));
            self.clock = None;
            self.cursor = None;
            self.last_arrival_at = None;
            for s in [&mut self.video_s, &mut self.audio_s] {
                s.last_at = None;
                s.late_since = None;
                s.anchored = false;
            }
        }
        self.last_tick = Some(at);
        let traced = self.trace_left > 0;
        let mut trace = traced.then(|| self.snapshot("tick", Some(at)));
        let arrived = [self.video_s.since.take(), self.audio_s.since.take()];
        if arrived.iter().any(Option::is_some) {
            let gap = self
                .last_arrival_at
                .is_some_and(|t| at - t > ms_ns(PLAYOUT_REANCHOR_GAP_MS));
            self.last_arrival_at = Some(at);
            for (s, a) in [&mut self.video_s, &mut self.audio_s].into_iter().zip(arrived) {
                if a.is_some() {
                    s.last_at = Some(at);
                }
            }
            match self.clock {
                None => {
                    self.anchor(at);
                    self.note_with("anchor", Some(at), None, arrived);
                }
                Some(c) => {
                    let (judged, why) = self.judge(c.due(at), at, arrived, gap);
                    match judged {
                        Judged::Keep => {}
                        Judged::Join => {
                            self.anchor(at);
                            self.note_with(&format!("join({why})"), Some(at), Some(c.due(at)), arrived);
                        }
                        Judged::Reanchor => {
                            self.anchor(at);
                            counts.reanchors += 1;
                            self.note_with(&format!("reanchor({why})"), Some(at), Some(c.due(at)), arrived);
                        }
                    }
                }
            }
        }
        self.phase();
        let due = self.clock.map(|c| c.due(at));
        // The frame NEAREST due airs (C4 fix round 3, the round-2 re-review): the newest one due puts
        // the picture (due - stamp) behind its sound, anything up to a frame by the anchor's phase
        // (camera-burst's p50 read 25, 4 and 28 ms over three runs with no re-anchor).
        let half = self.video_t.step().unwrap_or(0) / 2;
        let mut aired = None;
        let mut popped = 0u64;
        while let Some(f) = self.video.front() {
            let ready = match f.pts {
                None => true,
                Some(p) => due.is_some_and(|d| p <= d + half),
            };
            if !ready {
                break;
            }
            aired = self.video.pop_front();
            popped += 1;
        }
        let fresh = aired.is_some();
        // Phased or not, a clock that has aired a frame stays where it is: moved back under it, the
        // picture would hold a frame over (`phase`).
        if let Some(c) = self.clock.as_mut().filter(|_| fresh) {
            c.phased = true;
        }
        if let Some(f) = aired {
            counts.unaired += popped - 1;
            self.on_air = Some((f.pts, f.v));
        } else if self.on_air.is_some() {
            counts.repeated = 1;
        }
        if let Some(mut t) = trace.take() {
            t.video_since = arrived[0];
            t.audio_since = arrived[1];
            t.due_after = due;
            t.aired = if fresh { self.on_air_pts() } else { None };
            t.popped = popped;
            t.repeated = counts.repeated == 1;
            t.frames = self.video.len();
            self.keep(t);
            self.trace_left -= 1;
        }
        self.video_s.pushes = 0;
        self.audio_s.pushes = 0;
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
        let mut take = self.take_audio_due_counted(at_ns, n, pad);
        // A camera with no audio yet in this generation has none to pad: its filler is not counted
        // (else a video-only camera reads as all filler, the review's m9).
        if !self.audio_seen {
            take.counts.audio_pad_bytes = 0;
        }
        take
    }

    fn take_audio_due_counted(&mut self, at_ns: u64, n: usize, pad: impl FnMut(&mut [u8])) -> AudioTake {
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

    /// Drops the queued frames, counted unaired (the frame on air stays); how many it dropped.
    pub fn clear_video(&mut self) -> usize {
        let n = self.video.len();
        self.pending.unaired += n as u64;
        self.video.clear();
        n
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
            self.note(&format!("new_generation(to={g})"), None);
            self.restart();
        } else {
            self.unanchor();
        }
        self.gen = Some(g);
    }

    /// A stamped audio chunk's start (sample `raw` as stamped) on the video's stamps. A jump back
    /// past the cap drops the ring (it plays in push order, and the old chunks would sit ahead of
    /// the new). While the video is fresh (it arrived since the last tick, or at it), audio more than
    /// `PLAYOUT_STREAM_SKEW_MAX_MS` from the newest frame's stamp is re-based onto it, counted as a
    /// re-anchor: the audio's stamps, queued and to come, move by the difference.
    fn place_audio(&mut self, raw: i128) -> i128 {
        let mut start = raw + self.audio_offset;
        if self
            .audio_s
            .newest
            .is_some_and(|nw| sample_ns(start) < nw - ms_ns(PLAYOUT_CAP_MS))
        {
            self.note(
                &format!("audio_jump_back(to_ms={:.1})", sample_ns(start) as f64 / 1e6),
                None,
            );
            self.clear_audio();
            self.audio_s.rebased();
        }
        let fresh =
            self.video_s.since.is_some() || self.video_s.last_at.is_some_and(|l| Some(l) == self.last_tick);
        if let Some(v) = self.video_s.newest.filter(|_| fresh) {
            let iota = self.interleave.unwrap_or(0);
            // A provisional re-base whose replaced offset fits the stamps again: undone.
            if let Some(prior) = self
                .skew_prior
                .filter(|p| (sample_ns(raw + p) - v - iota).abs() <= ms_ns(PLAYOUT_SKEW_REVERT_MS))
            {
                self.note(
                    &format!(
                        "unrebase(placed_ms={:.1},video_ms={:.1},offset_ms={:.1})",
                        sample_ns(start) as f64 / 1e6,
                        v as f64 / 1e6,
                        sample_ns(prior) as f64 / 1e6
                    ),
                    None,
                );
                let delta = prior - self.audio_offset;
                self.rebase(delta);
                start += delta;
                self.skew_prior = None;
                self.pending.reanchors += 1;
            }
            let d = sample_ns(start) - v;
            // The interleave reads off the raw stamps (C4 fix round 3, the round-2 re-review): read
            // off the placed ones it carries any error between the two offsets into every later
            // re-base (028e8b3: R7's frame into R9's and R11's).
            if let Some(between) = self.video_t.raw.map(|r| sample_ns(raw) - r).filter(|b| {
                self.settle.is_none() && !self.video_t.clamped && b.abs() <= ms_ns(PLAYOUT_SKEW_REVERT_MS)
            }) {
                let i = self.interleave.unwrap_or(between);
                self.interleave = Some(i + (between - i) / INTERLEAVE_WEIGHT);
            }
            // The video's newest trails its own arrival by up to a step, so a skew up to a step under
            // the threshold can read past it (the re-review's n2, 340 ms): re-based, it plays off the
            // picture by the skew while it lasts (counted) and is undone once it clears. A tolerance
            // the other way would leave a skew up to a step over it unbased, and the video's wait,
            // the target plus the skew, past the cap: the span bound drops the frames (a freeze).
            if (sample_ns(start) - v - iota).abs() > ms_ns(PLAYOUT_STREAM_SKEW_MAX_MS) {
                self.note(
                    &format!(
                        "rebase(placed_ms={:.1},video_ms={:.1},interleave_ms={:.1})",
                        sample_ns(start) as f64 / 1e6,
                        v as f64 / 1e6,
                        iota as f64 / 1e6
                    ),
                    None,
                );
                let delta = sample_at(v + iota) - start;
                self.skew_prior.get_or_insert(self.audio_offset);
                self.rebase(delta);
                start += delta;
                self.settle = Some(Settle {
                    from: sample_ns(start),
                    sum: 0,
                    n: 0,
                });
                self.pending.reanchors += 1;
            } else if let Some(mut st) = self.settle {
                // The audio since the re-base: the first `PLAYOUT_SETTLE_SKIP_MS` of it is the
                // transient's (skipped), the next `PLAYOUT_SETTLE_MS` is averaged. Readings that
                // come long after the last (a stream that stopped, a jump in its stamps) are no
                // part of it: the settle is given up.
                let (skip, window) = (ms_ns(PLAYOUT_SETTLE_SKIP_MS), ms_ns(PLAYOUT_SETTLE_MS));
                let since = sample_ns(start) - st.from;
                if !(0..=skip + window + ms_ns(PLAYOUT_CAP_MS)).contains(&since) {
                    self.settle = None;
                    return start;
                }
                if since >= skip {
                    st.sum += d;
                    st.n += 1;
                }
                self.settle = Some(st);
                if since >= skip + window {
                    self.settle = None;
                    // Within the slack it is one stream already (as two chunks' stamps are).
                    let by = iota - st.sum / st.n;
                    let delta = sample_at(sample_ns(start) + by) - start;
                    if by.abs() > ms_ns(PLAYOUT_AUDIO_SLACK_MS) {
                        self.note(
                            &format!("settle(by_ms={:.1})", sample_ns(delta) as f64 / 1e6),
                            None,
                        );
                        self.rebase(delta);
                        start += delta;
                    }
                }
            }
        }
        start
    }

    /// Moves the audio's stamps, the queued chunks' and every later one's, by `delta` samples.
    fn rebase(&mut self, delta: i128) {
        self.audio_offset += delta;
        self.audio_s.rebased();
        for c in &mut self.audio {
            c.start = c.start.map(|s| s + delta);
        }
    }

    /// The clock's rules at a tick that saw `arrived` (the video's newest since the last tick, the
    /// audio's), with `due` the media time it has due. Re-anchor (counted): after a gap; when every
    /// live stream is more than the cap ahead; when an arrival is more than the cap behind; when a
    /// stream's arrivals have all been past due for `PLAYOUT_LATE_REANCHOR_MS`. Join (not counted:
    /// it completes the anchor): a stream the clock had not taken into account arrives with less
    /// than the target before it is due.
    fn judge(&mut self, due: i128, at: i128, arrived: [Option<i128>; 2], gap: bool) -> (Judged, String) {
        let (cap, slack) = (ms_ns(PLAYOUT_CAP_MS), ms_ns(PLAYOUT_AUDIO_SLACK_MS));
        let mut why: Vec<String> = Vec::new();
        if gap {
            why.push("gap".into());
        }
        let mut join = false;
        for ((s, a), name) in [&mut self.video_s, &mut self.audio_s]
            .into_iter()
            .zip(arrived)
            .zip(["video", "audio"])
        {
            let Some(n) = a else { continue };
            let lead = n - due;
            if lead < -slack {
                let from = *s.late_since.get_or_insert(at);
                if at - from >= ms_ns(PLAYOUT_LATE_REANCHOR_MS) {
                    why.push(format!("{name}_late(lead_ms={:.1})", lead as f64 / 1e6));
                }
            } else {
                s.late_since = None;
            }
            if lead < -cap {
                why.push(format!("{name}_behind(lead_ms={:.1})", lead as f64 / 1e6));
            }
            if !s.anchored {
                s.anchored = true;
                if lead < ms_ns(PLAYOUT_TARGET_MS) - slack {
                    join = true;
                    why.push(format!("{name}_joins(lead_ms={:.1})", lead as f64 / 1e6));
                }
            }
        }
        let ahead = [self.video_s, self.audio_s]
            .iter()
            .filter(|s| s.live(at))
            .filter_map(|s| s.newest)
            .map(|n| n - due)
            .min();
        if let Some(l) = ahead.filter(|l| *l > cap) {
            why.push(format!("ahead(lead_ms={:.1})", l as f64 / 1e6));
        }
        // Every live stream early past the margin, for `PLAYOUT_EARLY_REANCHOR_MS`: the delay
        // ratcheted (a re-anchor on a lagging stamp, then a catch-up), so the clock moves forward to
        // the target again.
        match ahead.filter(|l| *l > ms_ns(PLAYOUT_TARGET_MS + PLAYOUT_EARLY_MARGIN_MS)) {
            Some(l) => {
                let from = *self.early_since.get_or_insert(at);
                if at - from >= ms_ns(PLAYOUT_EARLY_REANCHOR_MS) {
                    why.push(format!("early(lead_ms={:.1})", l as f64 / 1e6));
                }
            }
            None => self.early_since = None,
        }
        let redo = why.iter().any(|w| !w.contains("_joins"));
        let judged = if redo {
            Judged::Reanchor
        } else if join {
            Judged::Join
        } else {
            Judged::Keep
        };
        (judged, why.join("+"))
    }

    /// A sample of stream `video` (else audio) carried segment `seg` (start, base, time, ns): a note
    /// when it differs from the stream's last (`detail` says what), so a new segment mid-run reads in
    /// the log beside the clock's events (C4 fix round 2).
    pub fn note_segment(&mut self, video: bool, seg: (u64, u64, u64), detail: &str) {
        let i = usize::from(!video);
        if self.segments[i] != Some(seg) {
            self.segments[i] = Some(seg);
            let name = if video { "video" } else { "audio" };
            self.note(&format!("segment({name},{detail})"), None);
        }
    }

    /// The notes since the last call (clock events and traced ticks), oldest first, and how many past
    /// `NOTES_MAX` were not kept.
    pub fn take_notes(&mut self) -> (Vec<Note>, u64) {
        (
            std::mem::take(&mut self.notes),
            std::mem::take(&mut self.notes_dropped),
        )
    }

    fn snapshot(&self, what: &str, at: Option<i128>) -> Note {
        let at = at.or(self.last_tick);
        Note {
            what: what.to_owned(),
            at,
            gen: self.gen,
            due_before: at.and_then(|a| self.clock.map(|c| c.due(a))),
            due_after: None,
            video_newest: self.video_s.newest,
            audio_newest: self.audio_s.newest,
            video_since: self.video_s.since,
            audio_since: self.audio_s.since,
            video_pushes: self.video_s.pushes,
            audio_pushes: self.audio_s.pushes,
            audio_raw: self.audio_raw,
            audio_offset_ns: sample_ns(self.audio_offset),
            frames: self.video.len(),
            audio_bytes: self.audio_bytes,
            aired: None,
            popped: 0,
            repeated: false,
        }
    }

    /// Records a clock event (and traces the ticks after it).
    fn note(&mut self, what: &str, at: Option<i128>) {
        let n = self.snapshot(what, at);
        self.keep(n);
        self.trace_left = TRACE_TICKS;
    }

    /// `note` at a tick, after the clock moved: `due_before` as judged, the tick's arrivals.
    fn note_with(
        &mut self,
        what: &str,
        at: Option<i128>,
        due_before: Option<i128>,
        arrived: [Option<i128>; 2],
    ) {
        let mut n = self.snapshot(what, at);
        n.due_before = due_before;
        n.due_after = at.and_then(|a| self.clock.map(|c| c.due(a)));
        n.video_since = arrived[0];
        n.audio_since = arrived[1];
        self.keep(n);
        self.trace_left = TRACE_TICKS;
    }

    fn keep(&mut self, n: Note) {
        if self.notes.len() < NOTES_MAX {
            self.notes.push(n);
        } else {
            self.notes_dropped += 1;
        }
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
        self.video_s = Stream::default();
        self.audio_s = Stream::default();
        self.video_t = VideoStamps::default();
        self.last_arrival_at = None;
        self.early_since = None;
        self.audio_offset = 0;
        self.skew_prior = None;
        self.interleave = None;
        self.settle = None;
        self.audio_seen = false;
    }

    /// The clock at `at`: the target behind the newest stamp of the later-arriving live stream (the
    /// lower of the two newest), so each keeps the whole target. Called at a tick that saw an
    /// arrival, so the stream that arrived is live.
    fn anchor(&mut self, at: i128) {
        let newest = [self.video_s, self.audio_s]
            .iter()
            .filter(|s| s.live(at))
            .filter_map(|s| s.newest)
            .min();
        if let Some(n) = newest {
            self.clock = Some(Clock {
                at,
                media: n - ms_ns(PLAYOUT_TARGET_MS),
                phased: false,
            });
            self.phase();
        }
        for s in [&mut self.video_s, &mut self.audio_s] {
            s.anchored = s.live(at);
            s.late_since = None;
        }
        self.early_since = None;
    }

    /// Puts the clock due on the video's own stamps, once their step is known (C4 fix round 3): the
    /// frame nearest due airs (`tick`), so a clock due between two stamps would sit on the rule's tie
    /// and a jittered stamp would flip which frame airs (at 30 fps the target alone is four frames
    /// and a half). Moved back, never on, so each stream keeps the whole target (to the audio's slack);
    /// made before the first frame airs (the step is known from the second frame, the target is
    /// several). The lead is then `phased_lead`'s.
    fn phase(&mut self) {
        let Some(c) = self.clock.filter(|c| !c.phased) else {
            return;
        };
        if let (Some(v), Some(step)) = (self.video_s.newest, self.video_t.step().filter(|&s| s > 0)) {
            // Within the audio's slack of the target counts as the target: the mean step is floored,
            // so a target of whole frames (150 ms at 60 fps) reads a few ns past the ninth.
            let media = c.media + ms_ns(PLAYOUT_AUDIO_SLACK_MS);
            self.clock = Some(Clock {
                media: media - (media - v).rem_euclid(step),
                phased: true,
                ..c
            });
        }
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
    use domain::config::{
        OutputProfile, CAMERA_MAX_FPS, PLAYOUT_LATE_REANCHOR_MS, PLAYOUT_SETTLE_MS, PLAYOUT_SETTLE_SKIP_MS,
        PLAYOUT_STREAM_SKEW_MAX_MS,
    };

    /// Both profiles are 30 fps (`config.rs`), so a loop over them is one 30 fps grid run twice, not a
    /// sweep of grids: the source rates (25, 30, 60) are what these tests sweep.
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
        /// Programme time its audio reaches the queue (`NO_AUDIO`: never), and the audio's stamp: the
        /// frame's own unless a schedule moves them.
        audio_at: u64,
        audio_pts: u64,
        /// How far the audio's stamp is past its content (an outlier's error, a skewed stamp's): its
        /// samples are labelled by `audio_pts - label_shift`, the time they were captured at.
        label_shift: i64,
    }

    /// An `audio_at` past every run: the frame's audio never arrives (a camera with no audio track).
    const NO_AUDIO: u64 = u64::MAX;

    impl Arrival {
        fn id(&self) -> u64 {
            self.gen * 1_000_000 + self.k
        }

        /// The 48 kHz sample its audio starts at (the source's rate divides 48 kHz: exact).
        fn first_sample(&self) -> u64 {
            (self.audio_pts * u64::from(AUDIO_RATE) + NS / 2) / NS
        }

        /// The 48 kHz sample its audio was captured at (its label).
        fn first_label(&self) -> u64 {
            ((self
                .audio_pts
                .checked_add_signed(-self.label_shift)
                .expect("a label after 0"))
                * u64::from(AUDIO_RATE)
                + NS / 2)
                / NS
        }

        /// Its audio: the source's samples for the frame, each labelled with its own sample index.
        fn audio(&self) -> Vec<u8> {
            let spf = u64::from(AUDIO_RATE) / self.src_fps;
            (self.first_label()..self.first_label() + spf)
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
                audio_at: arrive_base + slot + DELIVERY_NS,
                audio_pts: media_base + slot,
                label_shift: 0,
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
            if (from..until).contains(&a.audio_at) {
                a.audio_at = until;
            }
        }
        arrivals.sort_by_key(|a| a.at);
        arrivals
    }

    /// A hold of the video alone (its audio arrives on time): what would arrive in
    /// `[from, from + ms)` arrives at its end.
    fn hold_video(mut arrivals: Vec<Arrival>, from: u64, ms: u64) -> Vec<Arrival> {
        let until = from + ms * MS;
        for a in &mut arrivals {
            if (from..until).contains(&a.at) {
                a.at = until;
            }
        }
        arrivals
    }

    /// Each frame's audio reaches the queue `after_ms` after the frame (negative: before it), its
    /// stamp unchanged.
    fn audio_after(mut arrivals: Vec<Arrival>, after_ms: i64) -> Vec<Arrival> {
        for a in &mut arrivals {
            a.audio_at =
                a.at.checked_add_signed(after_ms * MS as i64)
                    .expect("a schedule that starts after the skew");
        }
        arrivals
    }

    /// From programme time `from` every arrival (both streams) is `extra` later and stays so: a
    /// delivery-delay step with no catch-up.
    fn delay_from(mut arrivals: Vec<Arrival>, from: u64, extra: u64) -> Vec<Arrival> {
        for a in &mut arrivals {
            if a.at >= from {
                a.at += extra;
                a.audio_at += extra;
            }
        }
        arrivals
    }

    /// A hold of `ms` from `from` whose backlog then drains at `speed` times real time (both
    /// streams): a frame due at `t` arrives at `max(t, from + ms + (t - from) / speed)`.
    fn hold_catch_up(mut arrivals: Vec<Arrival>, from: u64, ms: u64, speed: u64) -> Vec<Arrival> {
        let until = from + ms * MS;
        let late = |t: u64| {
            if t >= from {
                t.max(until + (t - from) / speed)
            } else {
                t
            }
        };
        for a in &mut arrivals {
            a.at = late(a.at);
            a.audio_at = late(a.audio_at);
        }
        arrivals
    }

    /// From `from` every arrival is `extra` later, until `back`, after which it is on time again: a
    /// delivery-delay step that clears.
    fn step_and_back(mut arrivals: Vec<Arrival>, from: u64, back: u64, extra: u64) -> Vec<Arrival> {
        for a in &mut arrivals {
            if (from..back).contains(&a.at) {
                a.at += extra;
                a.audio_at += extra;
            }
        }
        arrivals
    }

    /// Each fresh tick's wait: its time less its frame's arrival (ns), from tick `from` on.
    fn waits(r: &Run, fps: u64, arrivals: &[Arrival], from: usize) -> Vec<i128> {
        r.recs
            .iter()
            .enumerate()
            .skip(from)
            .filter(|(_, x)| x.fresh)
            .map(|(i, x)| {
                let a = arrivals
                    .iter()
                    .find(|a| Some(a.id()) == x.aired)
                    .expect("an aired frame");
                i128::from(at(fps, i as u64)) - i128::from(a.at)
            })
            .collect()
    }

    /// One return of camera-flap on the real chain, as PR #113's diagnostic notes logged it (CI run
    /// 45588726031, commit bddecb7, `compositor.log`'s `playout …` lines): `before_s` of frames, a
    /// pause of `pause_ms` (the camera's counter runs on, so both streams' stamps jump by it), then
    /// `after_s` more. At the return:
    /// - the decoder lets go of the last `held` frames from before the pause, with their old stamps
    ///   (returns 3 to 12: 29 519.7 and 29 553.1 ms arrived with the new audio);
    /// - the new frame at index `video_outlier` is stamped one pause past its capture (tsdemux across
    ///   the PCR jump: 39 383.9 ms where its audio read 34 477.3), and GstVideoDecoder clamps every
    ///   later stamp that would go back to it ("decreasing timestamp"): the frames after it all carry
    ///   39 383.9 until their own stamps pass it, a pause later (4.9 s, 147 frames);
    /// - the new chunk at index `audio_outlier`, when there is one, is stamped a pause on too
    ///   (39 386.6 between 34 498.6 and 34 547.1), and the audio decoder does not clamp.
    ///
    /// Returns the schedule and the return's first new frame.
    fn chain_return(
        fps: u64,
        pause_ms: u64,
        held: u64,
        video_outlier: Option<u64>,
        audio_outlier: Option<u64>,
        before_s: u64,
        after_s: u64,
    ) -> (Vec<Arrival>, u64) {
        chain_return_with(
            fps,
            pause_ms,
            held,
            video_outlier,
            audio_outlier,
            (before_s, after_s),
            |_| {},
        )
    }

    /// `chain_return` with the camera's own stamps shaped first (`camera`: its jitter, a dropped
    /// frame), before the chain's outlier and the decoder's clamp act on them, as on the chain.
    fn chain_return_with(
        fps: u64,
        pause_ms: u64,
        held: u64,
        video_outlier: Option<u64>,
        audio_outlier: Option<u64>,
        (before_s, after_s): (u64, u64),
        camera: impl Fn(&mut Vec<Arrival>),
    ) -> (Vec<Arrival>, u64) {
        let paused = pause_ms * fps / 1_000;
        let (kp, kr) = (before_s * fps, before_s * fps + paused);
        let mut all = source(1, fps, 0..kr + after_s * fps, MEDIA_BASE_NS, 0);
        let back = all[kr as usize].at;
        camera(&mut all);
        let jump = paused * NS / fps;
        let mut out: Vec<Arrival> = all.into_iter().filter(|a| !(kp..kr).contains(&a.k)).collect();
        for a in &mut out {
            if (kp - held..kp).contains(&a.k) {
                a.at = back;
            }
        }
        if let Some(i) = video_outlier {
            let outlier = out.iter().find(|a| a.k == kr + i).expect("the outlier").pts + jump;
            for a in &mut out {
                if a.k == kr + i || (a.k > kr + i && a.pts <= outlier) {
                    a.pts = outlier;
                }
            }
        }
        if let Some(j) = audio_outlier {
            let a = out.iter_mut().find(|a| a.k == kr + j).expect("the audio outlier");
            a.audio_pts += jump;
            a.label_shift = jump as i64;
        }
        (out, kr)
    }

    /// `returns` of camera-flap's returns in one generation, each as `chain_return` models one (the
    /// last two frames before the pause held to the return, the new frame at `video_outlier` stamped
    /// a pause on and clamped to until the stamps pass it, the chunk at `audio_outlier` a pause on),
    /// `run_s` of frames before each pause of `pause_ms` and after the last. Returns the schedule and
    /// each return's first new frame.
    fn chain_returns(
        fps: u64,
        pause_ms: u64,
        returns: u64,
        run_s: u64,
        video_outlier: u64,
        audio_outlier: Option<u64>,
    ) -> (Vec<Arrival>, Vec<u64>) {
        let shape = ReturnShape {
            video_outlier: Some(video_outlier),
            audio_outlier,
            extra: None,
        };
        chain_replay(fps, pause_ms, run_s, &vec![shape; returns as usize])
    }

    /// Which of the chain's stamp errors one return of `chain_replay` brings.
    #[derive(Debug, Clone, Copy)]
    struct ReturnShape {
        /// The new frame at this index is stamped a pause on, and the decoder clamps to it until the
        /// stamps pass it.
        video_outlier: Option<u64>,
        /// The new chunk at this index is stamped a pause on.
        audio_outlier: Option<u64>,
        /// The new frame at this index (inside the clamped run) leaves the decoder twice: one
        /// placement more than the stamps advance.
        extra: Option<u64>,
    }

    /// `chain_returns` with each return shaped on its own (`shapes`, one per return), as camera-flap's
    /// returns differ on the chain.
    fn chain_replay(fps: u64, pause_ms: u64, run_s: u64, shapes: &[ReturnShape]) -> (Vec<Arrival>, Vec<u64>) {
        let returns = shapes.len() as u64;
        let paused = pause_ms * fps / 1_000;
        let jump = paused * NS / fps;
        let mut out = source(
            1,
            fps,
            0..(returns + 1) * (run_s * fps + paused),
            MEDIA_BASE_NS,
            0,
        );
        let mut krs = Vec::new();
        for (i, shape) in (1..=returns).zip(shapes) {
            let kp = i * run_s * fps + (i - 1) * paused;
            let kr = kp + paused;
            let back = out.iter().find(|a| a.k == kr).expect("the return").at;
            out.retain(|a| !(kp..kr).contains(&a.k));
            for a in &mut out {
                if (kp - 2..kp).contains(&a.k) {
                    a.at = back;
                }
            }
            if let Some(v) = shape.video_outlier {
                let outlier = out.iter().find(|a| a.k == kr + v).expect("the outlier").pts + jump;
                for a in &mut out {
                    if a.k == kr + v || (a.k > kr + v && a.pts <= outlier) {
                        a.pts = outlier;
                    }
                }
            }
            if let Some(j) = shape.audio_outlier {
                let a = out.iter_mut().find(|a| a.k == kr + j).expect("the audio outlier");
                a.audio_pts += jump;
                a.label_shift = jump as i64;
            }
            if let Some(e) = shape.extra {
                let at = out.iter().position(|a| a.k == kr + e).expect("the extra frame");
                let twice = Arrival {
                    audio_at: NO_AUDIO,
                    ..out[at]
                };
                out.insert(at + 1, twice);
            }
            krs.push(kr);
        }
        (out, krs)
    }

    /// From `from` to `until` (programme time) one stream (`video`, else the audio) reaches the queue
    /// `ms` after its slot; then it catches up: what would have arrived by `until + ms` arrives then,
    /// in order.
    fn lag_between(mut arrivals: Vec<Arrival>, from: u64, until: u64, ms: u64, video: bool) -> Vec<Arrival> {
        let lag = ms * MS;
        for a in &mut arrivals {
            let t = if video { &mut a.at } else { &mut a.audio_at };
            if (from..until).contains(t) {
                *t += lag;
            } else if (until..until + lag).contains(t) {
                *t = until + lag;
            }
        }
        arrivals
    }

    /// Each tick's A/V on the captured content (labels and the frame's own slot), not on the stamps:
    /// the audio's first real sample minus the frame on air's capture time, ns, from tick `from`.
    fn av_by_content(r: &Run, arrivals: &[Arrival], from: usize) -> Vec<(usize, i128)> {
        r.recs
            .iter()
            .enumerate()
            .skip(from)
            .filter_map(|(i, x)| {
                let label = *x.labels.first()?;
                let first = r.recs[..=i].iter().rev().find_map(|y| y.aired)?;
                let frame = arrivals.iter().find(|a| a.id() == first)?;
                let shown = i128::from(MEDIA_BASE_NS + frame.k * NS / frame.src_fps);
                Some((i, sample_ns(i128::from(label)) - shown))
            })
            .collect()
    }

    /// The ticks after a return's first frame a re-base takes to settle: the transient it skips and
    /// the window it averages (config), and six ticks for what comes before and after them: the
    /// audio outlier is the third chunk (a second re-base, `a.k == kr + 2`), the settle fires on the
    /// chunk that closes its window, and its move shows in that tick's take.
    fn settle_ticks(fps: u64) -> u64 {
        (PLAYOUT_SETTLE_SKIP_MS + PLAYOUT_SETTLE_MS) * fps / 1_000 + 6
    }

    /// The fake camera's `POST /pause` then `POST /resume`, `pauses` times inside one generation,
    /// `run_s` seconds of frames before, between and after them: a paused frame is never sent, the
    /// video's stamps go on with the slots (they jump by the pause), and the audio's carry on from the
    /// last chunk before the pause (the chain's AAC side on PR #113's camera-flap leg, CI run
    /// 45575918709: the two disagreed by the pause after each return). The audio of the frame at
    /// index `j` of the schedule is the `j`th chunk. Returns the schedule and each return's first k.
    fn paused(fps: u64, pauses: u64, pause_ms: u64, run_s: u64) -> (Vec<Arrival>, Vec<u64>) {
        let skipped = pause_ms * fps / 1_000;
        let (mut out, mut returns, mut k) = (Vec::new(), Vec::new(), 0);
        for seg in 0..=pauses {
            if seg > 0 {
                k += skipped;
                returns.push(k);
            }
            out.extend(source(1, fps, k..k + run_s * fps, MEDIA_BASE_NS, 0));
            k += run_s * fps;
        }
        for (j, a) in out.iter_mut().enumerate() {
            a.audio_pts = MEDIA_BASE_NS + j as u64 * NS / fps;
        }
        (out, returns)
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
                a.audio_at = last;
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

    /// The programme's ticks over `arrivals`: each tick pushes what has arrived by its time (a
    /// frame and its audio in the schedule's order, a frame before its audio at the same instant),
    /// ticks, then takes its audio period.
    fn run(fps: u64, arrivals: &[Arrival], ticks: u64) -> Run {
        let mut p = Playout::default();
        let period_bytes = (u64::from(AUDIO_RATE) / fps) as usize * AUDIO_FRAME_BYTES;
        let mut events: Vec<(u64, usize, bool)> = arrivals
            .iter()
            .enumerate()
            .flat_map(|(i, a)| [(a.at, i, false), (a.audio_at, i, true)])
            .filter(|&(t, _, _)| t != NO_AUDIO)
            .collect();
        events.sort_by_key(|&(t, i, audio)| (t, i, audio));
        let (mut next, mut pushed, mut recs) = (0, 0u64, Vec::new());
        for i in 0..ticks {
            let mut pushed_gens = Vec::new();
            while next < events.len() && events[next].0 <= at(fps, i) {
                let (_, j, audio) = events[next];
                let a = arrivals[j];
                if audio {
                    p.push_audio(Some(a.gen), Some(a.audio_pts), &a.audio());
                } else {
                    p.push_video(Some(a.gen), Some(a.pts), a.id());
                    pushed_gens.push(a.gen);
                    pushed += 1;
                }
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
        Run { p, recs, pushed }
    }

    /// The rule on the schedule alone (one generation, no re-anchor): (repeats, unaired) from the
    /// first frame's air to the last's, and the tick the first frame airs.
    /// The clock's lead at an anchor on a camera at `src_fps` (`Playout::phase`): the target, to the
    /// audio's slack, rounded up to whole frames of the camera's (floored) period, so the clock is
    /// due on the video's own stamps.
    fn phased_lead(src_fps: u64) -> i128 {
        let step = i128::from(NS / src_fps);
        let target = i128::from(TARGET_NS) - ms_ns(PLAYOUT_AUDIO_SLACK_MS);
        target + (-target).rem_euclid(step)
    }

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
                // Due the phased lead after the anchor, and aired once it is nearest due: half a
                // frame before.
                let step = i128::from(NS / a.src_fps);
                let due = i128::from(at(fps, anchor)) + i128::from(a.pts) - i128::from(newest)
                    + phased_lead(a.src_fps);
                let ready = (due - step / 2).max(i128::from(a.at));
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
    /// audio within half a video period of the picture either way (the audio at the clock's due
    /// time, the picture the frame nearest due), allowing the audio's slack either side. Returns the
    /// ticks checked.
    fn assert_av_in_step(name: &str, r: &Run, src_fps: u64) -> u64 {
        assert_av_in_step_from(name, r, src_fps, 0)
    }

    /// `assert_av_in_step` from tick `from` on (after a warm-up whose join moved the clock back
    /// under frames already aired).
    fn assert_av_in_step_from(name: &str, r: &Run, src_fps: u64, from: usize) -> u64 {
        let slack = i128::from(PLAYOUT_AUDIO_SLACK_MS * MS);
        let half = i128::from(NS / src_fps / 2);
        let mut checked = 0;
        for (i, rec) in r.recs.iter().enumerate().skip(from) {
            if let Some(av) = rec.av {
                assert!(
                    (-half - slack..=half + slack).contains(&av),
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
    /// C4 fix round 2: each clock event leaves a log note naming its rule and the stamps it saw, and
    /// the ticks after it are traced (`TRACE_TICKS`), so the real chain's stamp sequence can be read
    /// from the compositor's log. A gap past the cap: the first anchor, then one `reanchor(gap)`.
    #[test]
    fn each_clock_event_leaves_a_note_naming_its_rule() {
        let fps = 30; // One grid: the notes are per event, not per rate.
        let gap_ms = PLAYOUT_REANCHOR_GAP_MS + 100;
        let (before, after) = (2 * fps, 2 * fps);
        let skipped = gap_ms * fps / 1_000;
        let mut arrivals = source(1, fps, 0..before, MEDIA_BASE_NS, 0);
        arrivals.extend(source(
            1,
            fps,
            before + skipped..before + skipped + after,
            MEDIA_BASE_NS,
            0,
        ));
        let mut r = run(fps, &arrivals, before + skipped + after + fps);
        let (notes, dropped) = r.p.take_notes();
        let events: Vec<&str> = notes
            .iter()
            .map(|n| n.what.as_str())
            .filter(|w| *w != "tick")
            .collect();
        eprintln!("events: {events:?}; first: {}", notes[0]);
        assert_eq!(events, ["anchor", "reanchor(gap)"]);
        assert_eq!(dropped, 0);
        let traced = notes.iter().filter(|n| n.what == "tick").count();
        assert_eq!(
            traced,
            2 * TRACE_TICKS as usize,
            "each event traces the ticks after it"
        );
        assert!(
            notes[0].to_string().starts_with("playout anchor gen=1 "),
            "{}",
            notes[0]
        );
        assert!(r.p.take_notes().0.is_empty(), "taken once");
    }

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
                // The audio jumps with the picture: from the first frame after the jump, every period
                // to the last frame's tick (not into it: its period runs past the source's last audio)
                // plays its own audio, in step (a jump back drops the old audio queued ahead of it).
                let period_bytes = (u64::from(AUDIO_RATE) / fps) as usize * AUDIO_FRAME_BYTES;
                for (i, x) in r.recs[first_post..r.last_air()].iter().enumerate() {
                    assert_eq!(
                        x.labels.len() * 4,
                        period_bytes,
                        "{name}: tick {} plays its own audio",
                        first_post + i
                    );
                }
                assert!(
                    assert_av_in_step_from(name, &r, fps, first_post) as usize >= r.last_air() - first_post
                );
                r.assert_conserved(name);
                checked += 1;
            }
        }
        eprintln!("jumps checked: {checked}");
        assert_eq!(checked, 2 * PROFILES.len());
    }

    // ---- two streams (fix round 1: the review's C1, C2, I1) ----

    /// The first tick at or after programme time `t`.
    fn tick_at(fps: u64, t: u64) -> u64 {
        (0..).find(|&i| at(fps, i) >= t).unwrap()
    }

    /// The tick the frame `id` airs, fresh.
    fn air_tick(r: &Run, id: u64) -> usize {
        r.recs
            .iter()
            .position(|x| x.fresh && x.aired == Some(id))
            .unwrap_or_else(|| panic!("frame {id} never aired"))
    }

    /// Review C1: a camera paused twice inside one generation whose audio stamps carry on across
    /// each pause while its video's jump (PR #113's camera-flap leg, red on CI with 80 re-anchors a
    /// beat and the pre-pause frame frozen on air). Each return is a gap (a re-anchor) and a re-base
    /// of the audio onto the video's stamps (a re-anchor): the audio's chunk lands on the newest
    /// frame's stamp. With the audio arriving after its frame or with it, the re-base happens before
    /// the return's first tick sees both, so the return costs 2; with it arriving first, that tick
    /// anchors on the audio's old stamps and the next re-anchors past the cap: 3. After that the
    /// return is steady: the first frame back airs a target (and the re-anchor's tick) after the tick
    /// that saw it, every frame back airs once, in order, with no repeat and no re-anchor, and every
    /// period plays its own audio (none padded, none late), within a frame of the picture.
    #[test]
    fn a_pause_whose_audio_stamps_carry_on_returns_on_the_new_frames_with_their_audio() {
        let mut checked = 0;
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let period_bytes = (u64::from(AUDIO_RATE) / fps) as usize * AUDIO_FRAME_BYTES;
            let spf = u64::from(AUDIO_RATE) / fps;
            for (after_ms, per_return) in [(-10i64, 3u64), (0, 2), (10, 2)] {
                let (schedule, returns) = paused(fps, 2, 4_750, 3);
                assert_eq!(returns.len(), 2, "a pause and a second pause");
                // Start the schedule late enough that audio arriving early stays after programme 0.
                let schedule: Vec<Arrival> = schedule
                    .into_iter()
                    .map(|mut a| {
                        a.at += 20 * MS;
                        a.audio_at += 20 * MS;
                        a
                    })
                    .collect();
                let arrivals = audio_after(schedule, after_ms);
                let ticks = arrivals.last().unwrap().at * fps / NS + fps;
                let r = run(fps, &arrivals, ticks);
                let base_sample = arrivals[0].first_sample();
                for (n, &k0) in returns.iter().enumerate() {
                    let tag = format!("{fps} fps, audio {after_ms} ms, return {n}");
                    let first = arrivals.iter().position(|a| a.k == k0).unwrap();
                    let last = returns.get(n + 1).map_or(arrivals.len(), |&k| {
                        arrivals.iter().position(|a| a.k == k).unwrap()
                    }) - 1;
                    let ret = arrivals[first];
                    let seen = tick_at(fps, ret.at.min(ret.audio_at)) as usize;
                    let first_post = air_tick(&r, ret.id());
                    let last_post = air_tick(&r, arrivals[last].id());
                    // The target again (one more tick when the audio came first).
                    assert!(
                        first_post as u64 <= tick_at(fps, at(fps, seen as u64 + 1) + TARGET_NS),
                        "{tag}: the first frame back airs at tick {first_post}, seen at {seen}"
                    );
                    let aired: Vec<u64> = r.recs[first_post..=last_post]
                        .iter()
                        .filter(|x| x.fresh)
                        .filter_map(|x| x.aired)
                        .collect();
                    let ids: Vec<u64> = arrivals[first..=last].iter().map(Arrival::id).collect();
                    assert_eq!(aired, ids, "{tag}: every frame back once, in order");
                    let window = &r.recs[seen..=last_post];
                    let reanchors: u64 = window.iter().map(|x| x.counts.reanchors).sum();
                    assert_eq!(reanchors, per_return, "{tag}: the return's re-anchors");
                    // The re-base moves the audio already queued with the rest: none of the return's
                    // audio is dropped late, from the tick that first saw it.
                    let late: u64 = r.recs[seen..last_post]
                        .iter()
                        .map(|x| x.counts.audio_late_bytes)
                        .sum();
                    assert_eq!(late, 0, "{tag}: no audio of the return dropped");
                    let repeats: u64 = r.recs[first_post..=last_post]
                        .iter()
                        .map(|x| x.counts.repeated)
                        .sum();
                    assert_eq!(repeats, 0, "{tag}: steady back");
                    // The audio to two ticks before the segment's last frame airs: the re-base aligns
                    // by arrival, to within a frame, so the last chunk before the next pause can end a
                    // period before the last frame's, and the period after it runs past the chunk.
                    for (i, x) in r.recs[first_post..last_post - 1].iter().enumerate() {
                        let i = first_post + i;
                        assert_eq!(
                            (
                                x.labels.len() * 4,
                                x.counts.audio_pad_bytes,
                                x.counts.audio_late_bytes
                            ),
                            (period_bytes, 0, 0),
                            "{tag}: tick {i} plays its own audio"
                        );
                        // The chunk its first sample is in, and that chunk's frame, against the frame on air.
                        let j = ((u64::from(x.labels[0]) - base_sample) / spf) as usize;
                        let k_audio = arrivals[j].k as i64;
                        let k_video = (x.aired.unwrap() % 1_000_000) as i64;
                        assert!(
                            (k_audio - k_video).abs() <= 1,
                            "{tag}: tick {i}: audio of frame {k_audio} under frame {k_video}"
                        );
                        checked += 1;
                    }
                }
                r.assert_conserved("pause");
            }
        }
        eprintln!("ticks checked after a return: {checked}");
        assert!(checked > 0);
    }

    /// Review I1: audio reaching the queue before or after its frame (stamps equal), by 100 and
    /// 250 ms, both under `PLAYOUT_STREAM_SKEW_MAX_MS`. The clock anchors on the later stream, so
    /// both keep the whole target: steady, nothing repeats, no audio pads or drops after the
    /// warm-up, and the audio stays in step; with the harness's three 300 ms holds and the audio
    /// first, the video (the later stream) repeats exactly the rule's count for the video alone (3 a
    /// hold; the review read 7 at 100 ms and 10 at 250 with the clock on the earlier stream).
    #[test]
    fn audio_skewed_from_its_video_keeps_the_whole_target_for_both() {
        let mut checked = 0;
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let secs = 32;
            for after_ms in [100i64, 250, -100, -250] {
                let skew_ns = after_ms.unsigned_abs() * MS;
                assert!(skew_ns < PLAYOUT_STREAM_SKEW_MAX_MS * MS, "under the re-base");
                // Steady.
                let arrivals = audio_after(source(1, fps, 0..10 * fps, MEDIA_BASE_NS, 300 * MS), after_ms);
                let ticks = 11 * fps;
                let r = run(fps, &arrivals, ticks);
                let both = tick_at(fps, arrivals[0].at.max(arrivals[0].audio_at)) as usize;
                let warm = tick_at(fps, at(fps, both as u64) + TARGET_NS + skew_ns) as usize + 1;
                let last = r.last_air();
                let mut c = Counts::default();
                // To the last frame's tick, not into it: its period runs past the source's last audio.
                for x in &r.recs[warm..last] {
                    c.add(&x.counts);
                }
                assert_eq!(
                    (
                        c.repeated,
                        c.unaired,
                        c.audio_pad_bytes,
                        c.audio_late_bytes,
                        c.reanchors
                    ),
                    (0, 0, 0, 0, 0),
                    "{fps} fps, audio {after_ms} ms: steady after the warm-up"
                );
                assert!(assert_av_in_step_from("skew", &r, fps, warm) >= (last - warm) as u64);
                // Three 300 ms holds.
                if after_ms < 0 {
                    let mut held =
                        audio_after(source(1, fps, 0..secs * fps, MEDIA_BASE_NS, 300 * MS), after_ms);
                    let holds: Vec<u64> = (1..=3).map(|n| n * 10 * NS + 10 * MS).collect();
                    for &h in &holds {
                        held = hold(held, h, 300);
                    }
                    let ticks = (secs + 1) * fps;
                    let r = run(fps, &held, ticks);
                    let video_only: Vec<Arrival> = held
                        .iter()
                        .map(|&a| Arrival {
                            audio_at: NO_AUDIO,
                            ..a
                        })
                        .collect();
                    let (rep, _, _) = expected(fps, &video_only, ticks);
                    assert_eq!(rep, 9, "the rule for the video alone: 3 a hold");
                    assert_eq!(
                        r.total().repeated,
                        rep,
                        "{fps} fps, audio {after_ms} ms: the later stream's target"
                    );
                    assert_eq!(r.total().reanchors, 0);
                }
                checked += 1;
            }
        }
        eprintln!("skews checked: {checked}");
        assert_eq!(checked, 4 * PROFILES.len());
    }

    /// Review C2: a delivery-delay step with no catch-up puts every arrival past due (the target
    /// plus two ticks, and 300 ms; both under the cap, so no other rule re-anchors). The stream late
    /// for `PLAYOUT_LATE_REANCHOR_MS` re-anchors once, at the first tick that long after the first
    /// late one, and from a target after it the camera is steady with every period's audio real
    /// (the review read no real audio for the rest of the run).
    #[test]
    fn a_delay_step_that_leaves_the_camera_late_re_anchors_once() {
        let mut checked = 0;
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let period = NS / fps;
            for extra in [TARGET_NS + 2 * period, 300 * MS] {
                // The premise: neither a drift past the cap nor a gap re-anchors this step.
                assert!(extra < PLAYOUT_CAP_MS * MS, "the step drifts past the cap");
                assert!(extra < PLAYOUT_REANCHOR_GAP_MS * MS, "the step is a gap");
                let step = 10 * NS;
                let arrivals = delay_from(source(1, fps, 0..20 * fps, MEDIA_BASE_NS, 0), step, extra);
                let ticks = 21 * fps;
                let r = run(fps, &arrivals, ticks);
                let tag = format!("{fps} fps, a {} ms step", extra / MS);
                let first_late = tick_at(fps, arrivals.iter().find(|a| a.at >= step).unwrap().at);
                let expect = tick_at(fps, at(fps, first_late) + PLAYOUT_LATE_REANCHOR_MS * MS) as usize;
                let anchors: Vec<usize> = (0..r.recs.len())
                    .filter(|&i| r.recs[i].counts.reanchors > 0)
                    .collect();
                assert_eq!(anchors, vec![expect], "{tag}: one re-anchor, the lateness's");
                let settled = tick_at(fps, at(fps, expect as u64) + TARGET_NS) as usize + 1;
                let mut c = Counts::default();
                // To the last frame's tick, not into it (its period runs past the source's last audio).
                for (i, x) in r.recs[settled..r.last_air()].iter().enumerate() {
                    c.add(&x.counts);
                    assert_eq!(
                        x.labels.len() * 4,
                        (u64::from(AUDIO_RATE) / fps) as usize * AUDIO_FRAME_BYTES,
                        "{tag}: tick {} all real",
                        settled + i
                    );
                }
                assert_eq!(
                    (c.repeated, c.audio_pad_bytes, c.audio_late_bytes),
                    (0, 0, 0),
                    "{tag}: steady after"
                );
                r.assert_conserved("step");
                checked += 1;
            }
        }
        eprintln!("steps checked: {checked}");
        assert_eq!(checked, 2 * PROFILES.len());
    }

    /// Review C2: a camera clock slower than the programme's (3 000 ppm, scaled up from a real
    /// camera's tens so the run is two minutes and a half): the lead falls 3 ms a second, crosses due
    /// after (lead − delivery + slack) / 3 ms seconds, and the lateness re-anchors it back to the lead,
    /// so the run re-anchors once per crossing and the audio never goes quiet for a second (the
    /// review read the audio silent from the crossing until the lead reached the cap).
    #[test]
    fn a_slow_camera_clock_re_anchors_each_time_it_falls_due() {
        let fps = 30; // One grid: the drift is the source's, and both profiles are 30 fps.
        let ppm = 3_000;
        let secs = 150;
        let mut arrivals = source(1, fps, 0..secs * fps, MEDIA_BASE_NS, 0);
        for a in &mut arrivals {
            let slot = a.k * NS / fps;
            a.at = slot + slot * ppm / 1_000_000 + DELIVERY_NS;
            a.audio_at = a.at;
        }
        let ticks = arrivals.last().unwrap().at * fps / NS + fps;
        let r = run(fps, &arrivals, ticks);
        // Each crossing: the clock's lead (`phased_lead`) less the delivery to the slack past due,
        // at 3 ms a second of the camera's clock, then the lateness's own wait. A frame is read at
        // the tick after it arrives, so the lead read falls a tick at a time and a crossing lands up
        // to a frame's worth of drift later: the run is long enough that both ends count the same.
        let lead = phased_lead(fps) as u64;
        let cross_ns = |extra: u64| {
            (lead - DELIVERY_NS + PLAYOUT_AUDIO_SLACK_MS * MS + extra) * 1_000_000 / ppm
                + PLAYOUT_LATE_REANCHOR_MS * MS
        };
        let (crossings, fewest) = (secs * NS / cross_ns(0), secs * NS / cross_ns(NS / fps));
        assert_eq!((crossings, fewest), (2, 2), "the schedule's own arithmetic");
        let c = r.total();
        assert_eq!(c.reanchors, crossings, "one re-anchor a crossing");
        let first = r.first_air().unwrap();
        let mut windows = 0;
        for w in r.recs[first..r.last_air()].chunks(fps as usize) {
            let real: usize = w.iter().map(|x| x.labels.len()).sum();
            assert!(real > 0, "a second of the camera with no real audio");
            windows += 1;
        }
        eprintln!("seconds with real audio: {windows}");
        assert!(windows as u64 >= secs - 2);
        r.assert_conserved("drift");
    }

    /// The rule past the cap reads every live stream (it fires when all are more than the cap ahead):
    /// a jump forward in the video's stamps at the moment the audio stops leaves the audio live, and
    /// behind, until the gap; the clock re-anchors once, when the audio has gone, and the frames after
    /// the jump air steady. Judged on the stream ahead alone, every tick until the gap would
    /// re-anchor onto the audio's old stamps, which the video is still past.
    /// The coordinator's fix-round-2 ruling on the re-review's n1: after a re-anchor the picture's
    /// delay must come back to the target. A re-anchor taken while the camera is late anchors on a
    /// lagging stamp; once the camera has caught up, every frame waits the target plus that lag.
    /// Two sequences, each with its two re-anchors read off the rules: a 400 ms hold whose backlog
    /// drains at twice real time (late for `PLAYOUT_LATE_REANCHOR_MS`, then early past
    /// `PLAYOUT_EARLY_MARGIN_MS` for `PLAYOUT_EARLY_REANCHOR_MS`), and a 300 ms delay step that
    /// clears 3 s later (late, then early). After the second, every frame waits the steady wait
    /// again: the target, rounded up to the tick grid (the re-review read 400 and 450 ms for the
    /// rest of the run).
    #[test]
    fn a_delay_that_ratchets_up_after_a_late_re_anchor_comes_back_to_the_target() {
        let mut checked = 0;
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let period = i128::from(NS / fps);
            let base = source(1, fps, 0..20 * fps, MEDIA_BASE_NS, 0);
            for (name, arrivals) in [
                (
                    "a 400 ms hold, caught up at 2x",
                    hold_catch_up(base.clone(), 5 * NS, 400, 2),
                ),
                (
                    "a 300 ms step, cleared 3 s on",
                    step_and_back(base.clone(), 5 * NS, 8 * NS, 300 * MS),
                ),
            ] {
                let tag = format!("{fps} fps, {name}");
                let r = run(fps, &arrivals, 21 * fps);
                let c = r.total();
                assert_eq!(c.reanchors, 2, "{tag}: late, then early");
                // Steady before the sequence, and again for its last 5 s: every frame waits the
                // target from its arrival, plus up to a period from its arrival to the tick that saw
                // the anchor's frame, plus up to a period up to the grid: under two periods more.
                let steady =
                    |w: &i128| (i128::from(TARGET_NS)..i128::from(TARGET_NS) + 2 * period).contains(w);
                let before = waits(&r, fps, &arrivals, (2 * fps) as usize);
                let before = &before[..(2 * fps) as usize];
                assert!(before.iter().all(steady), "{tag}: steady before: {before:?}");
                let after = waits(&r, fps, &arrivals, (15 * fps) as usize);
                assert!(
                    after.len() as u64 >= 4 * fps,
                    "{tag}: {} fresh ticks",
                    after.len()
                );
                assert!(after.iter().all(steady), "{tag}: back to the target: {after:?}");
                r.assert_conserved(&tag);
                checked += 1;
            }
        }
        eprintln!("ratchet sequences checked: {checked}");
        assert_eq!(checked, 2 * PROFILES.len());
    }

    /// C1 on the real chain (fix round 2): camera-flap's returns, replayed from the stamps PR #113's
    /// diagnostic notes logged (`chain_return`). On bddecb7 every return with the clamped outlier
    /// stormed: the video's newest stamp sat at the outlier, the lateness rule re-anchored every
    /// ~330 ms, and each re-anchor froze 5 ticks then skipped 6 (22–23 counted re-anchors, ~62 %
    /// repeated ticks, 1.6–3.6 s of late audio per 4.6 s window). Three shapes, each read off the
    /// log: the clean return (returns 1–2), the outlier on the first new frame with an audio outlier
    /// (returns 3–7, 9, 11–12), and on the second with none (returns 8, 10); and the outlier with each
    /// frame reaching the queue 60 ms after its audio, as the chain's decoders hand them out (the
    /// re-base keeps that interleave, `Playout::interleave`). From the switch to live
    /// (`resume_after_ms` after the return, 1 s) to the run's end the window is steady again: no
    /// counted re-anchor, no repeated or unaired frame, no late or filler audio, and the sound in step
    /// with the picture by their captured content (the stamps no longer say).
    #[test]
    fn a_return_whose_decoder_clamps_an_outlier_stamp_runs_steady_and_in_step() {
        let mut checked = 0;
        for profile in PROFILES {
            let fps = programme_fps(profile);
            for (name, outlier, audio_outlier, lag_ms) in [
                ("clean", None, None, 0),
                (
                    "outlier on the first new frame, audio outlier",
                    Some(0),
                    Some(2),
                    0,
                ),
                ("outlier on the second new frame", Some(1), None, 0),
                (
                    "the outlier, each frame 60 ms after its audio",
                    Some(0),
                    Some(2),
                    60,
                ),
            ] {
                let tag = format!("{fps} fps, {name}");
                let (arrivals, kr) = chain_return(fps, 4_900, 2, outlier, audio_outlier, 5, 8);
                // The chain's decoders hand each frame out after its audio (~55 ms on the log): the
                // re-base onto the outlier's stamps must keep that, not take it for A/V.
                let arrivals: Vec<Arrival> = arrivals
                    .into_iter()
                    .map(|a| Arrival {
                        at: a.at + lag_ms * MS,
                        ..a
                    })
                    .collect();
                let r = run(fps, &arrivals, 5 * fps + 147 + 8 * fps);
                let live = (kr + fps) as usize;
                let mut c = Counts::default();
                for x in &r.recs[live..] {
                    c.add(&x.counts);
                }
                assert_eq!(
                    (c.reanchors, c.repeated, c.unaired),
                    (0, 0, 0),
                    "{tag}: the live window's re-anchors, repeats, unaired"
                );
                assert_eq!(
                    (c.audio_late_bytes, c.audio_pad_bytes),
                    (0, 0),
                    "{tag}: the live window's late and filler audio"
                );
                let slack = i128::from(PLAYOUT_AUDIO_SLACK_MS * MS);
                let av = av_by_content(&r, &arrivals, live);
                assert!(av.len() as u64 >= 6 * fps, "{tag}: {} ticks with sound", av.len());
                // The frame nearest due airs: within half a frame either way.
                let half = i128::from(NS / fps / 2);
                let off: Vec<_> = av
                    .iter()
                    .filter(|(_, d)| !(-half - slack..=half + slack).contains(d))
                    .collect();
                assert!(
                    off.is_empty(),
                    "{tag}: out of step: {:?}",
                    &off[..off.len().min(8)]
                );
                r.assert_conserved(&tag);
                checked += 1;
            }
        }
        eprintln!("chain returns checked: {checked}");
        assert_eq!(checked, 4 * PROFILES.len());
    }

    /// The live window of a chain return (`chain_return`'s `kr`, from the switch to live, 1 s on, to
    /// the run's end): no counted re-anchor, no repeated or unaired frame, no late or filler audio, and
    /// the sound within a frame of the picture by captured content (`av_by_content`: in step is
    /// `[0, period)`, so within a frame is `(-period, 2 period)`, plus the slack). Returns the ticks
    /// with sound it judged.
    fn assert_live_in_step(tag: &str, fps: u64, arrivals: &[Arrival], r: &Run, kr: u64) -> usize {
        assert_window_in_step(tag, fps, arrivals, r, kr, r.recs.len())
    }

    /// `assert_live_in_step` on the live window up to tick `until` (the next pause's).
    fn assert_window_in_step(
        tag: &str,
        fps: u64,
        arrivals: &[Arrival],
        r: &Run,
        kr: u64,
        until: usize,
    ) -> usize {
        assert_window_in_step_from(
            tag,
            fps,
            arrivals,
            r,
            (kr + fps) as usize,
            (kr + settle_ticks(fps)) as usize,
            until,
        )
    }

    /// `assert_window_in_step` with the window's own start (`live`: its re-anchors, repeats and
    /// dropped frames are counted from there) and the tick its re-base has settled by (`settled`:
    /// `settle_ticks`). The settle's move is the audio shifting onto the steady interleave, counted
    /// as late or filler audio once, so the audio's late and filler bytes and its A/V are judged from
    /// the settled tick.
    fn assert_window_in_step_from(
        tag: &str,
        fps: u64,
        arrivals: &[Arrival],
        r: &Run,
        live: usize,
        settled: usize,
        until: usize,
    ) -> usize {
        let mut c = Counts::default();
        for x in &r.recs[live..until] {
            c.add(&x.counts);
        }
        assert_eq!(
            (c.reanchors, c.repeated, c.unaired),
            (0, 0, 0),
            "{tag}: the live window's re-anchors, repeats, unaired"
        );
        let mut c = Counts::default();
        for x in &r.recs[settled..until] {
            c.add(&x.counts);
        }
        assert_eq!(
            (c.audio_late_bytes, c.audio_pad_bytes),
            (0, 0),
            "{tag}: the settled window's late and filler audio"
        );
        let (slack, period) = (i128::from(PLAYOUT_AUDIO_SLACK_MS * MS), i128::from(NS / fps));
        let av: Vec<_> = av_by_content(r, arrivals, settled)
            .into_iter()
            .filter(|(i, _)| *i < until)
            .collect();
        assert!(
            av.len() as u64 >= (until - settled) as u64 * 3 / 4,
            "{tag}: {} of {} ticks with sound",
            av.len(),
            until - settled
        );
        let off: Vec<_> = av
            .iter()
            .filter(|(_, d)| !(-period - slack..2 * period + slack).contains(d))
            .collect();
        assert!(
            off.is_empty(),
            "{tag}: out of step: {:?}",
            &off[..off.len().min(8)]
        );
        r.assert_conserved(tag);
        av.len()
    }

    /// Two clamped runs in one stream, as every camera-flap return brings one: the first run's end
    /// is a partial increment (the frame's own stamp just past the repeated one, 5 ms here), not a
    /// period, so the second run's frames are still placed a period apart. Each run is checked frame
    /// by frame, and the stream between them keeps its own stamps on the moved offset.
    #[test]
    fn a_clamped_runs_end_is_no_period_for_the_next_run() {
        let period = i128::from(NS / 30);
        let mut t = VideoStamps::default();
        let mut raw = Vec::new();
        raw.extend((0..30).map(|k| k * period));
        let outlier = 29 * period + 4_900 * NS_PER_MS;
        raw.extend(std::iter::repeat_n(outlier, 20));
        let end = outlier + 5 * NS_PER_MS;
        raw.extend((0..30).map(|k| end + k * period));
        raw.extend(std::iter::repeat_n(end + 29 * period, 20));
        let (mut placed, mut runs, mut checked) = (Vec::new(), 0, 0);
        for &r in &raw {
            let (p, c) = t.place(r, None);
            runs += u32::from(c == Clamp::Starts);
            placed.push(p);
        }
        assert_eq!(runs, 2, "two clamped runs");
        for (i, w) in placed.windows(2).enumerate() {
            // The outlier's own jump (frame 30) is the stamps', not a placement.
            if i + 1 == 30 {
                continue;
            }
            assert_eq!(w[1] - w[0], period, "frame {}: placed a period on", i + 1);
            checked += 1;
        }
        eprintln!("placements checked: {checked}");
        assert_eq!(checked, raw.len() - 2);
    }

    /// The fake camera stamps each frame off its own clock, a few ms either side of its slot
    /// (camera-flap's ticks on PR #113, CI run 45595996718: increments of 30.1 to 34.0 ms at 30 fps).
    /// A clamped run places its frames one step apart for a pause (147 frames at 30 fps), so a step
    /// read off one increment carries that increment's error 147 times over, and the picture runs off
    /// its sound by the sum (that run's first return: the clamp's end put the video 37.8 ms past
    /// where its start had it). The step is the camera's mean frame period: single-frame increments
    /// only (a dropped frame's double one is not a period), and a first reading that a later
    /// increment shows was no period (a stall half again too long) is started over. Here each frame
    /// `k` is stamped `((3k mod 7) - 3)` ms off its slot (a single increment 3 ms long or 4 short,
    /// zero on average), alone, with a frame dropped every second before the pause, and with the
    /// camera's first two frames a stall (300 ms) apart.
    #[test]
    fn a_clamped_run_is_placed_at_the_cameras_mean_frame_period() {
        let (mut checked, mut ticks) = (0, 0);
        let jitter = |k: u64| (((k * 3) % 7) as i64 - 3) * MS as i64;
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let before_s = 5;
            for (shape, drops, stall) in [
                ("jittered", false, false),
                ("jittered, a frame dropped every second", true, false),
                ("jittered, a stall between the first two frames", false, true),
            ] {
                for (name, outlier, audio_outlier) in [
                    ("outlier on the first new frame, audio outlier", 0, Some(2)),
                    ("outlier on the second new frame", 1, None),
                ] {
                    let tag = format!("{fps} fps, {shape}, {name}");
                    let (arrivals, kr) = chain_return_with(
                        fps,
                        4_900,
                        2,
                        Some(outlier),
                        audio_outlier,
                        (before_s, 8),
                        |all| {
                            for a in all.iter_mut() {
                                a.pts = a.pts.checked_add_signed(jitter(a.k)).expect("a stamp after 0");
                                if drops && a.k < (before_s - 1) * fps && a.k % fps == fps / 2 {
                                    a.at = NO_AUDIO;
                                }
                                if stall && a.k == 0 {
                                    a.pts -= 300 * MS;
                                }
                            }
                        },
                    );
                    let r = run(fps, &arrivals, before_s * fps + 147 + 8 * fps);
                    ticks += assert_live_in_step(&tag, fps, &arrivals, &r, kr);
                    checked += 1;
                }
            }
        }
        eprintln!("jittered chain returns checked: {checked}, ticks with sound: {ticks}");
        assert_eq!(checked, 6 * PROFILES.len());
    }

    /// After the pause the decoder starts with an empty pipeline, so the return's first frames reach
    /// the queue close behind their audio and only then fall back to the steady interleave (PR #113's
    /// camera-flap, CI run 45595996718, return 1: the first new frame 13 ms behind its audio, the
    /// steady ~55 ms). A re-base reads the interleave at the return, inside that transient, and its
    /// error stood for the whole window: on CI run 45603961130 the returns' audio sat -18 to +28 ms
    /// off its video's offset, most of them +18 to +28 (A/V p50 23, max 60 against 40). So a re-base
    /// settles: past the transient (`PLAYOUT_SETTLE_SKIP_MS` of audio), the mean of the next `PLAYOUT_SETTLE_MS` is
    /// put back on the steady interleave (a move, not a re-anchor; the interleave takes no reading
    /// meanwhile). Here the video reaches the queue `lag` ms after its audio, except the return's
    /// first `fast` frames, which come at once; the clean return's audio, stamped late from it, is a
    /// re-base with no clamp (the interleave would read there). Settled, the window is in step.
    #[test]
    fn a_re_base_at_a_return_settles_onto_the_steady_interleave() {
        let (mut checked, mut ticks) = (0, 0);
        for profile in PROFILES {
            let fps = programme_fps(profile);
            for (name, outlier, audio_outlier, late_ms) in [
                (
                    "outlier on the first new frame, audio outlier",
                    Some(0),
                    Some(2),
                    0,
                ),
                ("outlier on the second new frame", Some(1), None, 0),
                // No clamp: the re-base's readings are the unclamped kind the interleave averages.
                (
                    "clean, the audio stamped 600 ms late from the return",
                    None,
                    None,
                    600,
                ),
            ] {
                // The return's first frames come at once for as long as the settle skips (less the
                // two chunks before the re-base): the transient the settle must not read.
                let transient = PLAYOUT_SETTLE_SKIP_MS * fps / 1_000 - 2;
                for (lag, fast) in [(60, 3), (90, 4), (60, transient), (90, transient)] {
                    let tag = format!("{fps} fps, {name}, video {lag} ms behind, {fast} fast");
                    let (arrivals, kr) = chain_return(fps, 4_900, 2, outlier, audio_outlier, 5, 8);
                    let late = |a: &Arrival| if a.k >= kr { late_ms * MS } else { 0 };
                    let arrivals: Vec<Arrival> = arrivals
                        .into_iter()
                        .map(|a| Arrival {
                            at: if (kr..kr + fast).contains(&a.k) {
                                a.at
                            } else {
                                a.at + lag * MS
                            },
                            audio_pts: a.audio_pts + late(&a),
                            label_shift: a.label_shift + late(&a) as i64,
                            ..a
                        })
                        .collect();
                    let r = run(fps, &arrivals, 5 * fps + 147 + 8 * fps);
                    let settled = (kr + settle_ticks(fps)) as usize;
                    ticks += assert_window_in_step_from(
                        &tag,
                        fps,
                        &arrivals,
                        &r,
                        (kr + fps) as usize,
                        settled,
                        r.recs.len(),
                    );
                    // Settled, the sound is in step, not just within a frame: the readings' mean is
                    // the steady interleave's own measure, so nothing is left over.
                    // The frame nearest due airs: within half a frame either way.
                    let (slack, half) = (i128::from(PLAYOUT_AUDIO_SLACK_MS * MS), i128::from(NS / fps / 2));
                    let off: Vec<_> = av_by_content(&r, &arrivals, settled)
                        .into_iter()
                        .filter(|(_, d)| !(-half - slack..=half + slack).contains(d))
                        .collect();
                    assert!(
                        off.is_empty(),
                        "{tag}: settled off step: {:?}",
                        &off[..off.len().min(8)]
                    );
                    checked += 1;
                }
            }
        }
        eprintln!("settling returns checked: {checked}, ticks with sound: {ticks}");
        assert_eq!(checked, 12 * PROFILES.len());
    }

    /// Camera-flap returns again and again in one generation (twelve on PR #113's leg), so each
    /// return starts from what the last left: the video's offset a clamp's end moved, a provisional
    /// re-base standing, the interleave as averaged since. Four returns here on bursty video (a burst
    /// of four frames, as `a_return_on_bursty_video_runs_steady_and_in_step`), at each place in the
    /// burst: every live window, from its switch to live to the next pause, runs steady and in step.
    #[test]
    fn every_return_of_a_flapping_camera_runs_steady_and_in_step() {
        const BURST: u64 = 4;
        let (mut checked, mut ticks) = (0, 0);
        for profile in PROFILES {
            let fps = programme_fps(profile);
            for phase in 0..BURST {
                let pause_ms = ((147 + phase) * 1_000).div_ceil(fps);
                let (arrivals, krs) = chain_returns(fps, pause_ms, 4, 8, 0, Some(2));
                let grid = BURST * NS / fps;
                let arrivals: Vec<Arrival> = arrivals
                    .into_iter()
                    .map(|a| Arrival {
                        at: a.at.div_ceil(grid) * grid,
                        ..a
                    })
                    .collect();
                let end = krs.last().expect("returns") + 8 * fps;
                let r = run(fps, &arrivals, end + BURST);
                for (n, &kr) in krs.iter().enumerate() {
                    let until = krs
                        .get(n + 1)
                        .map_or(end as usize, |&next| (next - (147 + phase) - 1) as usize);
                    let tag = format!("{fps} fps, place {phase} in the burst, return {}", n + 1);
                    ticks += assert_window_in_step(&tag, fps, &arrivals, &r, kr, until);
                    checked += 1;
                }
            }
        }
        eprintln!("returns checked: {checked}, ticks with sound: {ticks}");
        assert_eq!(checked, 4 * BURST as usize * PROFILES.len());
    }

    /// The chain's decoders hand the frames out in bursts (camera-flap's ticks on PR #113, CI run
    /// 45595996718: three frames in one tick, then none for two), so the newest frame's stamp at an
    /// audio push trails by anything up to a burst, and a re-base reads the interleave off one push.
    /// Each frame here reaches the queue at the next boundary of a burst of four frames (in order,
    /// the held frames still before the return's first), with its audio on time, and the return falls
    /// at each place in the burst: the live window runs steady and the sound stays within a frame.
    #[test]
    fn a_return_on_bursty_video_runs_steady_and_in_step() {
        const BURST: u64 = 4;
        let (mut checked, mut ticks) = (0, 0);
        let mut places = std::collections::BTreeSet::new();
        for (pi, profile) in PROFILES.into_iter().enumerate() {
            let fps = programme_fps(profile);
            for (name, outlier, audio_outlier) in [
                ("outlier on the first new frame, audio outlier", 0, Some(2)),
                ("outlier on the second new frame", 1, None),
            ] {
                for phase in 0..BURST {
                    // `paused` is `147 + phase` frames, so the return lands at each place in a burst.
                    let pause_ms = ((147 + phase) * 1_000).div_ceil(fps);
                    let tag = format!("{fps} fps, {name}, return at {phase} in the burst");
                    let (arrivals, kr) = chain_return(fps, pause_ms, 2, Some(outlier), audio_outlier, 5, 8);
                    let back = arrivals.iter().find(|a| a.k == kr).expect("the return").at;
                    places.insert((pi, outlier, back % (BURST * NS / fps)));
                    // Held to the next burst boundary on the programme's clock: in order, as the
                    // decoder hands them out (the held frames still before the return's first).
                    let grid = BURST * NS / fps;
                    let arrivals: Vec<Arrival> = arrivals
                        .into_iter()
                        .map(|a| Arrival {
                            at: a.at.div_ceil(grid) * grid,
                            ..a
                        })
                        .collect();
                    let r = run(fps, &arrivals, 5 * fps + 147 + BURST + 8 * fps);
                    ticks += assert_live_in_step(&tag, fps, &arrivals, &r, kr);
                    checked += 1;
                }
            }
        }
        eprintln!("bursty chain returns checked: {checked}, ticks with sound: {ticks}");
        assert_eq!(checked, 2 * BURST as usize * PROFILES.len());
        // Every return fell at its own place in its burst (the sweep is not one place four times).
        assert_eq!(places.len(), checked, "the returns' places in the burst");
    }

    /// The coordinator's fix-round-2 ruling on the re-review's n2: audio skewed past the re-base
    /// threshold played off the picture by the skew for good. An arrival skew and a stamp skew read
    /// the same at the moment (the newest of each stream apart by the skew), so neither can be told
    /// at the re-base; a re-base is therefore provisional, and undone (counted) once the audio's stamps
    /// agree with the video's again on the offset it replaced (`PLAYOUT_SKEW_REVERT_MS`). By captured
    /// content (`av_by_content`), from 1 s after the skew clears to the run's end:
    /// - audio 400 ms behind its video for 3 s, then on time: one re-base, one undo, in step after,
    ///   and while skewed off by at most the skew and a frame (bounded; a frame more at the switch);
    /// - audio 400 ms ahead (the video late) for 3 s: the same, mirrored, and the late video is a
    ///   delivery step for the picture the clock follows (the re-base puts the audio on it): one
    ///   lateness re-anchor in, one past the cap out, 4 in all;
    /// - audio 340 ms behind for 3 s (the re-review's case: the newest frame trails its arrival by up
    ///   to a frame, so it reads past the threshold): the same, where it used to stay off for good;
    /// - audio stamped 600 ms before its capture, arriving with its video, for the whole run: one
    ///   re-base, never undone, in step after it.
    #[test]
    fn a_skew_re_base_is_undone_once_the_stamps_agree_again() {
        let mut checked = 0;
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let (period, half) = (i128::from(NS / fps), i128::from(NS / fps / 2));
            let slack = i128::from(PLAYOUT_AUDIO_SLACK_MS * MS);
            let in_step = |d: &i128| (-half - slack..=half + slack).contains(d);
            let base = source(1, fps, 0..16 * fps, MEDIA_BASE_NS, 0);
            let stamped_early: Vec<Arrival> = base
                .iter()
                .map(|&a| Arrival {
                    audio_pts: a.audio_pts - 600 * MS,
                    label_shift: -600 * MS as i64,
                    ..a
                })
                .collect();
            for (name, arrivals, skew_ms, until, reanchors) in [
                (
                    "audio 400 ms behind",
                    lag_between(base.clone(), 4 * NS, 7 * NS, 400, false),
                    400,
                    7 * NS,
                    2,
                ),
                (
                    "audio 400 ms ahead",
                    lag_between(base.clone(), 4 * NS, 7 * NS, 400, true),
                    400,
                    7 * NS,
                    4,
                ),
                (
                    "audio 340 ms behind",
                    lag_between(base.clone(), 4 * NS, 7 * NS, 340, false),
                    340,
                    7 * NS,
                    2,
                ),
                ("audio stamped 600 ms early", stamped_early, 600, 0, 1),
            ] {
                let tag = format!("{fps} fps, {name}");
                let r = run(fps, &arrivals, 17 * fps);
                assert_eq!(r.total().reanchors, reanchors, "{tag}: counted re-anchors");
                // After the early re-anchor the undo leaves (its rule's wait), and a tick more: the
                // phased clock's lead can put it a tick later (`Playout::phase`).
                let settled =
                    ((until + skew_ms * MS + PLAYOUT_EARLY_REANCHOR_MS * MS + NS / fps) * fps / NS) as usize;
                let av = av_by_content(&r, &arrivals, settled);
                assert!(av.len() as u64 >= 7 * fps, "{tag}: {} ticks with sound", av.len());
                let off: Vec<_> = av.iter().filter(|(_, d)| !in_step(d)).collect();
                assert!(
                    off.is_empty(),
                    "{tag}: out of step after: {:?}",
                    &off[..off.len().min(6)]
                );
                // While skewed: off by at most the skew and a frame, and a frame more at the switch
                // back (the audio's undo and the clock's re-anchor can land a tick apart).
                let bound = i128::from(skew_ms * MS) + 2 * period + slack;
                let during = av_by_content(&r, &arrivals, (2 * fps) as usize);
                let past: Vec<_> = during.iter().filter(|(_, d)| d.abs() > bound).collect();
                assert!(
                    past.is_empty(),
                    "{tag}: past the skew: {:?}",
                    &past[..past.len().min(6)]
                );
                r.assert_conserved(&tag);
                checked += 1;
            }
        }
        eprintln!("skew sequences checked: {checked}");
        assert_eq!(checked, 4 * PROFILES.len());
    }

    #[test]
    fn a_video_jump_while_the_audio_stops_re_anchors_once() {
        let mut checked = 0;
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let jump = 4 * PLAYOUT_CAP_MS * MS;
            let (before, after) = (2 * fps, 3 * fps);
            let mut arrivals = source(1, fps, 0..before, MEDIA_BASE_NS, 0);
            arrivals.extend(
                source(1, fps, before..before + after, MEDIA_BASE_NS + jump, 0)
                    .into_iter()
                    .map(|a| Arrival {
                        audio_at: NO_AUDIO,
                        ..a
                    }),
            );
            let ticks = before + after + fps;
            let r = run(fps, &arrivals, ticks);
            assert_eq!(r.total().reanchors, 1, "{fps}: one re-anchor");
            // The frames the video sends while the audio is still live wait past the clock (the
            // span bound keeps the newest cap's worth); from the first one that airs it is steady.
            let first_post = r
                .recs
                .iter()
                .position(|x| x.fresh && x.aired.is_some_and(|id| id >= arrivals[before as usize].id()))
                .expect("a frame after the jump airs");
            let steady: u64 = r.recs[first_post..=r.last_air()]
                .iter()
                .map(|x| x.counts.repeated)
                .sum();
            assert_eq!(steady, 0, "{fps}: steady after");
            r.assert_conserved("jump, audio stops");
            checked += 1;
        }
        eprintln!("jumps with the audio stopping checked: {checked}");
        assert_eq!(checked, PROFILES.len());
    }

    /// The re-base reads the video only while it is fresh (it arrived since the last tick or at
    /// it): a hold of the video alone, longer than `PLAYOUT_STREAM_SKEW_MAX_MS` and under the gap,
    /// with its audio on time, is not a skew. No re-anchor, the audio never pads or drops, and it
    /// stays in step with the picture after the hold.
    #[test]
    fn a_hold_of_the_video_alone_is_not_a_skew() {
        let mut checked = 0;
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let hold_ms = PLAYOUT_STREAM_SKEW_MAX_MS + 50;
            assert!(hold_ms < PLAYOUT_REANCHOR_GAP_MS);
            let arrivals = hold_video(source(1, fps, 0..10 * fps, MEDIA_BASE_NS, 0), 5 * NS, hold_ms);
            let ticks = 11 * fps;
            let r = run(fps, &arrivals, ticks);
            let c = r.total();
            assert_eq!(c.reanchors, 0, "{fps}: no re-base");
            let first = r.first_air().unwrap();
            let mut audio = Counts::default();
            // To the last frame's tick, not into it (its period runs past the source's last audio).
            for x in &r.recs[first..r.last_air()] {
                audio.add(&x.counts);
            }
            assert_eq!(
                (audio.audio_pad_bytes, audio.audio_late_bytes),
                (0, 0),
                "{fps}: the audio's own clock"
            );
            // In step at every tick that airs a new frame (while the video is held the picture
            // repeats and the audio runs on, as it should).
            let (slack, half) = (i128::from(PLAYOUT_AUDIO_SLACK_MS * MS), i128::from(NS / fps / 2));
            let mut fresh_av = 0;
            for (i, x) in r.recs.iter().enumerate().filter(|(_, x)| x.fresh) {
                if let Some(av) = x.av {
                    assert!(
                        (-half - slack..=half + slack).contains(&av),
                        "{fps}: tick {i}: audio {av} ns from the picture"
                    );
                    fresh_av += 1;
                }
            }
            assert!(
                fresh_av as u64 >= 9 * fps,
                "{fps}: {fresh_av} fresh ticks checked"
            );
            r.assert_conserved("video hold");
            checked += 1;
        }
        eprintln!("video holds checked: {checked}");
        assert_eq!(checked, PROFILES.len());
    }

    /// Review m9: a camera with no audio track has no audio to pad, so its periods count no filler
    /// (else every camera tick reads 192 KB/s of `audio_pad_bytes`); once its audio arrives, a period
    /// the ring cannot fill counts again.
    #[test]
    fn a_camera_with_no_audio_counts_no_filler_until_its_audio_arrives() {
        let fps = 30; // One grid: the rule is per period, not per rate.
        let silent: Vec<Arrival> = source(1, fps, 0..3 * fps, MEDIA_BASE_NS, 0)
            .into_iter()
            .map(|a| Arrival {
                audio_at: NO_AUDIO,
                ..a
            })
            .collect();
        let r = run(fps, &silent, 4 * fps);
        assert!(r.fresh_ticks() > 0, "the camera aired");
        assert_eq!(r.all().audio_pad_bytes, 0, "no audio track, no filler counted");
        // Its audio starts at the second second; its last second stops again: the stop pads.
        let late: Vec<Arrival> = source(1, fps, 0..3 * fps, MEDIA_BASE_NS, 0)
            .into_iter()
            .map(|a| {
                if (fps..2 * fps).contains(&a.k) {
                    a
                } else {
                    Arrival {
                        audio_at: NO_AUDIO,
                        ..a
                    }
                }
            })
            .collect();
        let r = run(fps, &late, 4 * fps);
        let before: u64 = r.recs[..tick_at(fps, late[fps as usize].audio_at) as usize]
            .iter()
            .map(|x| x.counts.audio_pad_bytes)
            .sum();
        assert_eq!(before, 0, "nothing counted before the audio");
        assert!(r.all().audio_pad_bytes > 0, "the audio's stop pads, counted");
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
    /// the audio is all filler, not counted (no audio has arrived to pad: the review's m9).
    #[test]
    fn an_empty_queue_airs_and_counts_nothing() {
        let mut p: Playout<u64> = Playout::default();
        for i in 0..3 {
            let t = p.tick(i * NS / 30);
            assert_eq!((t.frame, t.fresh, t.counts), (None, false, Counts::default()));
        }
        let a = p.take_audio_due(0, 6_400, |b| b.fill(PAD));
        assert_eq!((a.real, a.counts.audio_pad_bytes), (0, 0));
        assert!(a.bytes.iter().all(|&b| b == PAD), "all filler");
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
        assert_eq!(p.due_ns(0), Some(i128::from(more[0].pts) - phased_lead(fps)));
    }

    /// The review's m5: a build's loss drops its queued frames (counted unaired, so conserved) and
    /// its audio; the frame on air stays (the domain freezes on it), and the next tick airs nothing
    /// new.
    #[test]
    fn a_lost_builds_queue_goes_counted_and_the_frame_on_air_stays() {
        let fps = 30; // One grid: the flush is per queue, not per rate.
        let arrivals = source(1, fps, 0..fps, MEDIA_BASE_NS, 0);
        let mut r = run(fps, &arrivals, fps / 2);
        let on_air = r.recs.last().unwrap().aired.expect("a frame on air");
        let queued = r.p.queued();
        assert!(queued > 0, "the premise: frames queued");
        assert_eq!(r.p.clear_video(), queued);
        assert!(r.p.clear_audio() > 0, "its audio too");
        let t = r.p.tick(at(fps, fps / 2));
        assert_eq!(
            (t.frame, t.fresh, t.counts.unaired),
            (Some(on_air), false, queued as u64)
        );
        r.recs.push(Rec {
            counts: t.counts,
            ..Rec::default()
        });
        r.assert_conserved("loss");
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

    /// A camera whose sound comes in AAC's 1 024-sample chunks on a grid of their own, `phase`
    /// samples past the picture's, each chunk arriving whole (its last sample captured, then the
    /// delivery and `lag` more), `secs` of it at `fps`: each tick's A/V on the content (the period's
    /// first label minus the capture of the frame on air), from a second after the first frame airs.
    fn aac_grid_av(fps: u64, phase: u64, lag: u64, secs: u64) -> Vec<i128> {
        const CHUNK: u64 = 1_024;
        let rate = u64::from(AUDIO_RATE);
        let s0 = MEDIA_BASE_NS * rate / NS + phase;
        // (arrival, video frame, audio chunk), a frame before a chunk at the same instant.
        let mut events: Vec<(u64, Option<u64>, Option<u64>)> = (0..secs * fps)
            .map(|k| (k * NS / fps + DELIVERY_NS, Some(k), None))
            .chain((0..secs * rate / CHUNK).map(|j| {
                (
                    (phase + (j + 1) * CHUNK) * NS / rate + DELIVERY_NS + lag,
                    None,
                    Some(j),
                )
            }))
            .collect();
        events.sort_by_key(|e| (e.0, e.2.is_some()));
        let mut p = Playout::<u64>::default();
        let period_bytes = (rate / fps) as usize * AUDIO_FRAME_BYTES;
        let (mut next, mut av, mut first_air) = (0, Vec::new(), None);
        for i in 0..(secs + 1) * fps {
            let now = at(fps, i);
            while let Some(&(_, video, audio)) = events.get(next).filter(|e| e.0 <= now) {
                if let Some(k) = video {
                    p.push_video(Some(1), Some(MEDIA_BASE_NS + k * NS / fps), k);
                }
                if let Some(j) = audio {
                    let s = s0 + j * CHUNK;
                    let bytes: Vec<u8> = (s..s + CHUNK).flat_map(|l| (l as u32).to_le_bytes()).collect();
                    p.push_audio(Some(1), Some((s * NS).div_ceil(rate)), &bytes);
                }
                next += 1;
            }
            let t = p.tick(now);
            let take = p.take_audio_due(now, period_bytes, |b| b.fill(PAD));
            if t.fresh && first_air.is_none() {
                first_air = Some(i);
            }
            let label = u32::from_le_bytes([take.bytes[0], take.bytes[1], take.bytes[2], take.bytes[3]]);
            // From a second after the first frame airs to the last second's start (the source stops,
            // and its last frames' sound outlasts them).
            let steady = first_air.is_some_and(|f| i >= f + fps) && i < secs * fps;
            if let (true, Some(k), true) = (steady, t.frame, label != u32::MAX) {
                av.push(sample_ns(i128::from(label)) - i128::from(MEDIA_BASE_NS + k * NS / fps));
            }
        }
        av
    }

    /// The round-2 re-review's probe `aac_grid_phase_av`: a tick that airs the newest frame due puts
    /// the picture (due - stamp) behind its sound, anything up to a frame, and the clock anchors on
    /// the later stream's newest stamp, which on the chain is the audio's, on AAC's 21.3 ms grid: so
    /// each anchor drew a bias from 0 to 33 ms (3 to 31 ms over the grid's phase in the probe;
    /// camera-burst's p50 read 25, 4 and 28 ms over three runs with no re-anchor, and camera-flap
    /// re-anchors at every return). The frame NEAREST due airs instead: at every phase of the grid the
    /// sound stays within half a frame of the picture.
    #[test]
    fn a_tick_airs_the_frame_nearest_its_due_on_any_audio_grid() {
        let (mut phases, mut ticks, mut widest) = (0, 0, 0);
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let (half, slack) = (i128::from(NS / fps / 2), ms_ns(PLAYOUT_AUDIO_SLACK_MS));
            for (phase, lag) in (0..1_024).step_by(64).flat_map(|p| [(p, 0), (p, 40 * MS)]) {
                let tag = format!("{fps} fps, the audio grid {phase} samples on, {lag} ns later");
                let av = aac_grid_av(fps, phase, lag, 6);
                assert!(av.len() as u64 >= 4 * fps, "{tag}: {} ticks with sound", av.len());
                let off: Vec<_> = av.iter().filter(|d| d.abs() > half + slack).collect();
                assert!(
                    off.is_empty(),
                    "{tag}: off the nearest frame: {:?}",
                    &off[..off.len().min(4)]
                );
                widest = widest.max(av.iter().map(|d| d.abs()).max().unwrap_or(0));
                phases += 1;
                ticks += av.len();
            }
        }
        eprintln!("grid phases and lags checked: {phases}, ticks: {ticks}, widest A/V: {widest} ns");
        assert_eq!(phases, 2 * 16 * PROFILES.len());
    }

    /// Camera-flap's returns 7 to 12 on PR #113's head 028e8b3 (chaos run 45611982713, its
    /// `compositor.log`). R7 and R12 each left the picture a whole frame behind its sound at their
    /// clamp's end (+31.3 and +31.9 ms on the offsets: one placement more than the stamps advanced),
    /// R8 re-based the audio with no clamp, R10 returned with neither stream's stamps off, and the
    /// interleave, read off the placed timelines, carried R7's frame into R9 and R11 (+35.3, +37.1)
    /// and R12's on top of it (+69.0: the gate's 63). Replayed with the extra frame late in R7's and
    /// R12's runs, and a live stretch after each clamp's end: from every clamp's end on, and through
    /// every later return, the sound stays within half a frame of the picture, and the extra frame
    /// costs one frame given up. Only the bad runs' own frames between the extra one and the run's
    /// end stay off: no rule at the run's end reaches back into it.
    #[test]
    fn a_clamped_runs_extra_frame_is_not_carried_past_its_end() {
        const EXTRA: u64 = 110;
        let shape = |video: bool, audio: bool, extra: bool| ReturnShape {
            video_outlier: video.then_some(0),
            audio_outlier: audio.then_some(2),
            extra: extra.then_some(EXTRA),
        };
        let shapes = [
            shape(true, true, true),
            shape(false, true, false),
            shape(true, true, false),
            shape(false, false, false),
            shape(true, false, false),
            shape(true, true, true),
        ];
        let (mut windows, mut ticks) = (0, 0);
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let pause_ms = (147 * 1_000_u64).div_ceil(fps);
            let paused = pause_ms * fps / 1_000;
            let (arrivals, krs) = chain_replay(fps, pause_ms, 7, &shapes);
            let end = krs.last().expect("returns") + 7 * fps;
            let r = run(fps, &arrivals, end + 8);
            let (half, slack) = (i128::from(NS / fps / 2), ms_ns(PLAYOUT_AUDIO_SLACK_MS));
            let k_of = |i: usize| {
                r.recs[..=i]
                    .iter()
                    .rev()
                    .find_map(|y| y.aired)
                    .map(|id| id % 1_000_000)
            };
            for (n, (&kr, s)) in krs.iter().zip(&shapes).enumerate() {
                let tag = format!("{fps} fps, R{}", n + 7);
                let (from, until) = (
                    (kr + fps) as usize,
                    krs.get(n + 1).map_or(end, |&x| x - paused) as usize,
                );
                // The bad run's frames from its extra one to the run's end, and the frames the move
                // after it takes (a move of half a step a frame, `VideoStamps::slewed`).
                let bad = s
                    .extra
                    .zip(s.video_outlier)
                    .map_or(0..0, |(e, v)| kr + e..kr + v + paused + 1 + 4);
                let mut c = Counts::default();
                for x in &r.recs[from..until] {
                    c.add(&x.counts);
                }
                let given_up = u64::from(s.extra.is_some());
                assert_eq!((c.reanchors, c.repeated), (0, 0), "{tag}: re-anchors, repeats");
                assert!(
                    c.unaired <= given_up,
                    "{tag}: {} unaired for {given_up} extra",
                    c.unaired
                );
                assert_eq!(
                    (c.audio_late_bytes, c.audio_pad_bytes),
                    (0, 0),
                    "{tag}: late and filler audio"
                );
                let av: Vec<_> = av_by_content(&r, &arrivals, from)
                    .into_iter()
                    .filter(|&(i, _)| i < until && k_of(i).is_some_and(|k| !bad.contains(&k)))
                    .collect();
                assert!(
                    av.len() as u64 >= (until - from) as u64 / 2,
                    "{tag}: {} ticks with sound",
                    av.len()
                );
                let off: Vec<_> = av.iter().filter(|(_, d)| d.abs() > half + slack).collect();
                assert!(
                    off.is_empty(),
                    "{tag}: out of step: {:?}",
                    &off[..off.len().min(6)]
                );
                windows += 1;
                ticks += av.len();
            }
            r.assert_conserved(&format!("{fps} fps"));
        }
        eprintln!("windows checked: {windows}, ticks with sound: {ticks}");
        assert_eq!(windows, shapes.len() * PROFILES.len());
    }

    /// A camera with no audio has no audio offset to re-lock a clamped run's end onto: the run's end
    /// keeps the seamless offset (`VideoStamps::place`), and the picture runs on frame by frame.
    #[test]
    fn a_video_only_cameras_clamp_end_moves_nothing() {
        let (mut cameras, mut frames) = (0, 0);
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let pause_ms = (147 * 1_000_u64).div_ceil(fps);
            let (mut arrivals, kr) = chain_return(fps, pause_ms, 2, Some(0), None, 2, 8);
            for a in &mut arrivals {
                a.audio_at = NO_AUDIO;
            }
            let r = run(fps, &arrivals, kr + 8 * fps + 8);
            let (live, last) = ((kr + fps) as usize, r.last_air());
            let mut c = Counts::default();
            for x in &r.recs[live..=last] {
                c.add(&x.counts);
            }
            assert_eq!(
                (c.reanchors, c.repeated, c.unaired),
                (0, 0, 0),
                "{fps} fps: re-anchors, repeats, unaired after the return"
            );
            let aired: Vec<u64> = r.recs[live..=last]
                .iter()
                .filter(|x| x.fresh)
                .filter_map(|x| x.aired.map(|id| id % 1_000_000))
                .collect();
            for w in aired.windows(2) {
                assert_eq!(w[1], w[0] + 1, "{fps} fps: frame by frame");
            }
            assert!(
                aired.len() as u64 >= 6 * fps,
                "{fps} fps: {} frames aired",
                aired.len()
            );
            cameras += 1;
            frames += aired.len();
        }
        eprintln!("video-only cameras checked: {cameras}, frames aired after the return: {frames}");
        assert_eq!(cameras, PROFILES.len());
    }

    /// A camera slower than the target (5 fps: a frame every 200 ms against 150) airs its first
    /// frame before its second shows the step, so the clock it aired on stays (`Playout::phase`):
    /// moved back under the frame on air, it would hold that frame over. The first frame airs on the
    /// newest-due rule (no step yet, so no half of one), the second half a period early (nearest due,
    /// the step known), and each after it the source's period after the last: six ticks of 30 fps.
    #[test]
    fn a_clock_that_has_aired_a_frame_is_not_phased_under_it() {
        let src = 5;
        let mut checked = 0;
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let arrivals = source(1, src, 0..10 * src, MEDIA_BASE_NS, 0);
            let r = run(fps, &arrivals, 12 * fps);
            let airs: Vec<usize> = r
                .recs
                .iter()
                .enumerate()
                .filter(|(_, x)| x.fresh)
                .map(|(i, _)| i)
                .collect();
            assert_eq!(airs.len() as u64, 10 * src, "{fps} fps: every frame airs");
            let ticks = (fps / src) as usize;
            for (i, w) in airs.windows(2).enumerate() {
                let gap = if i == 0 { ticks - ticks / 2 } else { ticks };
                assert_eq!(w[1] - w[0], gap, "{fps} fps: ticks {w:?}");
                checked += 1;
            }
        }
        eprintln!("frame-to-frame gaps checked: {checked}");
        assert_eq!(checked, (10 * src as usize - 1) * PROFILES.len());
    }

    /// A clamped run's end re-locks the offset onto the audio's (`VideoStamps::place`) by however
    /// much they differ, three frames either way here: the move is made half a step a frame at most
    /// (`VideoStamps::slewed`), so the placements never go back (the queue keeps its order), never
    /// close by more than half the way to the frame before, nor open by more than half a step, and
    /// the whole move is made.
    #[test]
    fn a_re_lock_moves_the_picture_without_reordering_it() {
        let period = i128::from(NS / 30);
        let mut checked = 0;
        for frames in [-3, 3] {
            let delta = frames * period;
            let mut t = VideoStamps::default();
            let mut raw: Vec<i128> = (0..30).map(|k| k * period).collect();
            let outlier = 29 * period + 4_900 * NS_PER_MS;
            raw.extend(std::iter::repeat_n(outlier, 20));
            let end = outlier + 5 * NS_PER_MS;
            raw.extend((0..30).map(|k| end + k * period));
            let mut placed = Vec::new();
            let mut target = None;
            for &r in &raw {
                // The seamless offset the run's end would take, to re-lock `delta` from it.
                let mut dry = t;
                if dry.place(r, None).1 == Clamp::Ends {
                    target = Some(dry.offset + delta);
                }
                placed.push(t.place(r, target).0);
            }
            let target = target.expect("the run ended");
            let step = t.step().expect("a step");
            let after = 50;
            for w in placed[after - 1..].windows(2) {
                let d = w[1] - w[0];
                assert!(d > 0, "{frames} frames: the order kept: {w:?}");
                assert!(
                    d >= (period - step / 2).min(step / 2),
                    "{frames} frames: {d} ns apart"
                );
                assert!(d <= period + step / 2, "{frames} frames: {d} ns apart");
                checked += 1;
            }
            assert_eq!(t.offset, target, "{frames} frames: the whole move made");
        }
        eprintln!("placements checked after a re-lock: {checked}");
        assert_eq!(checked, 2 * 30);
    }

    /// I-r3-1 (the round-3 re-review): a clamped run's end re-locks the picture onto the audio's offset,
    /// which is only the audio's while the audio is arriving. Audio that stopped mid-generation (a
    /// stream pausing is a designed case) keeps the offset of its last re-base for good, and the next
    /// video return's clamp end slewed the picture onto it: a move the size of a pause, half a step a
    /// frame, which the clock saw as a camera that stopped and started (the reviewer's probe on
    /// 1ec7098: 18 counted re-anchors, 90 repeated ticks and 90 skipped frames over 10 s, growing by
    /// a pause with every later flap). Here the audio stops at 3 s and the video flaps once, with the
    /// decoder's clamp on the first or second new frame; from a second after the return to the run's
    /// last frame, nothing is re-anchored, repeated or dropped.
    #[test]
    fn audio_that_has_stopped_is_no_clock_for_a_clamps_end() {
        let (mut windows, mut ticks) = (0, 0);
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let pause_ms = (147 * 1_000_u64).div_ceil(fps);
            for (name, outlier) in [("first new frame", 0), ("second new frame", 1)] {
                let tag = format!("{fps} fps, outlier on the {name}");
                let (mut arrivals, kr) = chain_return(fps, pause_ms, 2, Some(outlier), None, 5, 10);
                let stop = 3 * NS;
                for a in &mut arrivals {
                    if a.k * NS / a.src_fps >= stop {
                        a.audio_at = NO_AUDIO;
                    }
                }
                let mut r = run(fps, &arrivals, kr + 10 * fps + 8);
                // The premise: the audio really stopped (no real sample in the last 5 s), and the
                // clamp's end happened inside the run, exactly once.
                let last = r.recs.len() - 1;
                assert!(
                    r.recs[last - 5 * fps as usize..]
                        .iter()
                        .all(|x| x.labels.is_empty()),
                    "{tag}: audio still playing at the end"
                );
                let notes = r.p.take_notes().0;
                let ends = notes
                    .iter()
                    .filter(|n| n.what.starts_with("video_unclamped"))
                    .count();
                let (live, until) = ((kr + fps) as usize, r.last_air());
                let mut c = Counts::default();
                for x in &r.recs[live..=until] {
                    c.add(&x.counts);
                }
                assert_eq!(
                    (c.reanchors, c.repeated, c.unaired),
                    (0, 0, 0),
                    "{tag}: re-anchors, repeats, unaired after the return"
                );
                assert_eq!(ends, 1, "{tag}: the clamp's end");
                let aired: Vec<u64> = r.recs[live..=until]
                    .iter()
                    .filter(|x| x.fresh)
                    .filter_map(|x| x.aired.map(|id| id % 1_000_000))
                    .collect();
                for w in aired.windows(2) {
                    assert_eq!(w[1], w[0] + 1, "{tag}: frame by frame");
                }
                assert!(
                    aired.len() as u64 >= 8 * fps,
                    "{tag}: {} frames aired",
                    aired.len()
                );
                windows += 1;
                ticks += aired.len();
            }
        }
        eprintln!("returns with stopped audio checked: {windows}, frames aired: {ticks}");
        assert_eq!(windows, 2 * PROFILES.len());
    }

    /// M6's witness (fix round 3's Owed 119): the steady interleave is read off the RAW stamps, never
    /// the placed ones. A return with no clamp (camera-flap's R2 and R5) leaves its settle's residue in
    /// the audio's offset; read off the placed stamps the interleave would take that residue for the
    /// camera's own and put it into the next return's re-base, which then settled onto it. Here the
    /// first return's settle reads the video two periods late (a lag that ends with its readings), so
    /// the audio is left that much off its picture: the premise, checked on the first window. Then the
    /// interleave runs on unclamped stamps for 5 s, and the second return (a clamped one) re-bases and
    /// settles: from its settle's end to its clamp's end, the sound is in step.
    #[test]
    fn a_settle_residue_is_not_carried_into_the_next_return() {
        let (mut windows, mut ticks) = (0, 0);
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let pause_ms = (147 * 1_000_u64).div_ceil(fps);
            let paused = pause_ms * fps / 1_000;
            let shapes = [
                ReturnShape {
                    video_outlier: None,
                    audio_outlier: Some(2),
                    extra: None,
                },
                ReturnShape {
                    video_outlier: Some(0),
                    audio_outlier: Some(2),
                    extra: None,
                },
            ];
            let (arrivals, krs) = chain_replay(fps, pause_ms, 7, &shapes);
            let back = arrivals
                .iter()
                .find(|a| a.k == krs[0])
                .expect("the first return")
                .at;
            let until = back + (PLAYOUT_SETTLE_SKIP_MS + PLAYOUT_SETTLE_MS) * MS;
            let late_ms = 2 * 1_000 / fps;
            let arrivals = lag_between(arrivals, back, until, late_ms, true);
            let end = krs[1] + 7 * fps;
            let r = run(fps, &arrivals, end + 8);
            let (half, slack) = (i128::from(NS / fps / 2), ms_ns(PLAYOUT_AUDIO_SLACK_MS));
            let tag = format!("{fps} fps");
            // The premise: the first window's sound is off its picture by more than a frame's half.
            let (a_from, a_until) = ((krs[0] + settle_ticks(fps)) as usize, (krs[1] - paused) as usize);
            let first = av_by_content(&r, &arrivals, a_from);
            let first: Vec<_> = first.into_iter().filter(|&(i, _)| i < a_until).collect();
            assert!(
                first.len() as u64 >= 3 * fps,
                "{tag}: {} ticks with sound in the first window",
                first.len()
            );
            let off = first.iter().filter(|(_, d)| d.abs() > half + slack).count();
            assert!(
                off * 2 > first.len(),
                "{tag}: the first return left no residue ({off} of {} ticks off)",
                first.len()
            );
            // The second window, from its settle's end to its clamp's end.
            let (b_from, b_until) = ((krs[1] + settle_ticks(fps)) as usize, (krs[1] + 147 - 4) as usize);
            let second: Vec<_> = av_by_content(&r, &arrivals, b_from)
                .into_iter()
                .filter(|&(i, _)| i < b_until)
                .collect();
            assert!(
                second.len() >= (b_until - b_from) * 3 / 4,
                "{tag}: {} of {} ticks with sound in the second window",
                second.len(),
                b_until - b_from
            );
            let off: Vec<_> = second.iter().filter(|(_, d)| d.abs() > half + slack).collect();
            assert!(
                off.is_empty(),
                "{tag}: the second return carries the residue: {:?}",
                &off[..off.len().min(6)]
            );
            windows += 1;
            ticks += second.len();
        }
        eprintln!("residue windows checked: {windows}, ticks with sound: {ticks}");
        assert_eq!(windows, PROFILES.len());
    }

    /// A settle the camera's pause cuts short is given up, not finished on the first reading after
    /// the return: that reading is the return's transient (the decoder starts empty), and as the
    /// whole mean it would put the audio off by the transient's size for the rest of the window. Here
    /// the audio's stamps are 600 ms before its capture from the start (a re-base at the first
    /// chunk, so a settle opens), the camera pauses inside its skip, and the return's first chunks
    /// reach the queue three frames after their video (a first reading of -3 frames against the
    /// steady 0). From a second after the return, no settle move was made and the sound is in step.
    #[test]
    fn a_settle_cut_short_by_a_pause_is_given_up() {
        let (mut windows, mut ticks) = (0, 0);
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let kp = PLAYOUT_SETTLE_SKIP_MS * fps / 1_000;
            let pause_ms = (147 * 1_000_u64).div_ceil(fps);
            let kr = kp + pause_ms * fps / 1_000;
            let mut arrivals = source(1, fps, 0..kr + 8 * fps, MEDIA_BASE_NS, 0);
            arrivals.retain(|a| !(kp..kr).contains(&a.k));
            for a in &mut arrivals {
                a.audio_pts -= 600 * MS;
                a.label_shift = -600 * MS as i64;
                if (kr..kr + 3).contains(&a.k) {
                    a.audio_at += 3 * NS / fps;
                }
            }
            let tag = format!("{fps} fps");
            let mut r = run(fps, &arrivals, kr + 8 * fps + 8);
            let notes = r.p.take_notes().0;
            let rebases = notes.iter().filter(|n| n.what.starts_with("rebase(")).count();
            let settles = notes.iter().filter(|n| n.what.starts_with("settle(")).count();
            assert_eq!(rebases, 1, "{tag}: the stamps' re-base at the start only");
            assert_eq!(settles, 0, "{tag}: no settle move at the return");
            let (half, slack) = (i128::from(NS / fps / 2), ms_ns(PLAYOUT_AUDIO_SLACK_MS));
            let av = av_by_content(&r, &arrivals, (kr + fps) as usize);
            assert!(av.len() as u64 >= 6 * fps, "{tag}: {} ticks with sound", av.len());
            let off: Vec<_> = av.iter().filter(|(_, d)| d.abs() > half + slack).collect();
            assert!(
                off.is_empty(),
                "{tag}: out of step: {:?}",
                &off[..off.len().min(6)]
            );
            windows += 1;
            ticks += av.len();
        }
        eprintln!("cut-short settles checked: {windows}, ticks with sound: {ticks}");
        assert_eq!(windows, PROFILES.len());
    }

    /// The settle averages its whole `PLAYOUT_SETTLE_MS` window: readings late in it count. The same
    /// clean return twice, the second with the video `extra` ms further behind for the window's
    /// middle 40% (past its first 30%, ending 30% before its end); the move the settle makes differs
    /// by the mean of that, `extra` times the span's share of the window, to within a chunk's
    /// reading. A window cut short would not see the span (no difference) and a skip too short would
    /// shift both runs alike.
    #[test]
    fn the_settle_reads_its_whole_window() {
        let by = |notes: Vec<Note>| -> Vec<f64> {
            notes
                .iter()
                .filter_map(|n| {
                    n.what
                        .strip_prefix("settle(by_ms=")?
                        .strip_suffix(')')?
                        .parse()
                        .ok()
                })
                .collect()
        };
        let extra = 90_u64;
        let (mut checked, mut moved) = (0, 0);
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let make = |span: Option<(u64, u64)>| {
                let (arrivals, kr) = chain_return(fps, 4_900, 2, None, None, 5, 8);
                let mut arrivals: Vec<Arrival> = arrivals
                    .into_iter()
                    .map(|a| Arrival {
                        at: if (kr..kr + 3).contains(&a.k) {
                            a.at
                        } else {
                            a.at + 60 * MS
                        },
                        audio_pts: if a.k >= kr {
                            a.audio_pts + 600 * MS
                        } else {
                            a.audio_pts
                        },
                        label_shift: if a.k >= kr {
                            a.label_shift + 600 * MS as i64
                        } else {
                            a.label_shift
                        },
                        ..a
                    })
                    .collect();
                let back = arrivals.iter().find(|a| a.k == kr).expect("the return").at;
                if let Some((from, until)) = span {
                    arrivals = lag_between(arrivals, back + from, back + until, extra, true);
                }
                let mut r = run(fps, &arrivals, kr + 12 * fps);
                by(r.p.take_notes().0)
            };
            let (skip, window) = (PLAYOUT_SETTLE_SKIP_MS, PLAYOUT_SETTLE_MS);
            let (from, until) = (skip + window * 3 / 10, skip + window * 7 / 10);
            let plain = make(None);
            let lagged = make(Some((from * MS, until * MS)));
            assert_eq!(plain.len(), 1, "{fps} fps: one settle move, plain: {plain:?}");
            assert_eq!(lagged.len(), 1, "{fps} fps: one settle move, lagged: {lagged:?}");
            let want = (extra * (until - from) / window) as f64;
            let got = (lagged[0] - plain[0]).abs();
            assert!(
                (got - want).abs() <= 12.0,
                "{fps} fps: the span moved the settle by {got:.1} ms, want {want:.1}"
            );
            checked += 1;
            moved += 1;
        }
        eprintln!("whole-window settles checked: {checked}, moved: {moved}");
        assert_eq!(checked, PROFILES.len());
    }

    /// A stamp before the repeated one inside a clamped run is an older frame let go late, not the
    /// run's end (GstVideoDecoder never lets a stamp go back): the run is broken, the stamps are not
    /// known true, and the stamps that advance after it re-lock nothing, whatever the audio's offset.
    /// (The re-lock there moved a camera's picture by the audio's whole re-base.) The audio's offset
    /// is two frames off here, within what a re-lock may move (`PLAYOUT_STREAM_SKEW_MAX_MS`). A clean run's end
    /// beside it does re-lock, to the same audio offset: the guard is the difference.
    #[test]
    fn an_older_frame_inside_a_clamped_run_breaks_it_without_a_re_lock() {
        let period = i128::from(NS / 30);
        let two_frames = 2 * period;
        let (mut broken, mut ended) = (0, 0);
        for (name, older) in [("an older frame", true), ("no older frame", false)] {
            let mut t = VideoStamps::default();
            let mut raw: Vec<i128> = (0..10).map(|k| k * period).collect();
            raw.extend([9 * period + period / 2; 4]);
            if older {
                raw.push(9 * period + period / 4);
            }
            raw.extend((0..5).map(|k| 10 * period + k * period));
            let offset_before = t.offset;
            let mut ends = 0;
            for &r in &raw {
                let (_, c) = t.place(r, Some(two_frames));
                ends += usize::from(c == Clamp::Ends);
            }
            if older {
                assert_eq!(ends, 0, "{name}: no run ended");
                assert_eq!(t.relocked, 0, "{name}: nothing re-locked");
                assert_eq!(t.offset, offset_before, "{name}: the offset did not move");
                broken += 1;
            } else {
                assert_eq!(ends, 1, "{name}: the run ended");
                assert_ne!(t.relocked, 0, "{name}: the end re-locked");
                ended += 1;
            }
        }
        eprintln!("broken runs checked: {broken}, clean ends: {ended}");
        assert_eq!((broken, ended), (1, 1));
    }

    /// The camera-flap storm of fad76a2 (CI run 45649360015, windows 8 and 10: 12 and 13 re-anchors,
    /// 60 repeated ticks, 7 freezes), in the log's own order. The decoder's clamp runs across the
    /// pause (the last frames held at one stamp) and the camera's stamps are seamless over it, while
    /// the audio's jump by the pause. At the return the audio's first chunk arrives true and its second
    /// is the chain's outlier (a pause on again): the outlier is a re-base onto the picture's timeline,
    /// and its offset a pause off the true one; the picture's clamp ends on the very next frame and
    /// its re-lock took that provisional offset for the audio's, slewing the picture back by a pause
    /// (`relock_ms=-9905`), the clock re-anchoring on it for seconds. The re-lock moves the picture
    /// by no more than the error it exists for, a frame: here none, to within one frame, and nothing
    /// is re-anchored, repeated or dropped from a second after the return.
    #[test]
    fn a_clamp_ending_beside_the_audios_outlier_re_locks_nothing() {
        let (mut windows, mut ticks) = (0, 0);
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let pause_ms = (147 * 1_000_u64).div_ceil(fps);
            let paused = pause_ms * fps / 1_000;
            let tag = format!("{fps} fps");
            let (mut arrivals, kr) = chain_return(fps, pause_ms, 2, None, Some(1), 5, 10);
            let (kp, jump) = (kr - paused, paused * NS / fps);
            for a in &mut arrivals {
                if a.k >= kr {
                    a.pts -= jump;
                }
            }
            let repeated = arrivals.iter().find(|a| a.k == kr).expect("the return").pts + NS / fps / 2;
            for a in &mut arrivals {
                if (kp - 6..kp).contains(&a.k) || a.k == kr {
                    a.pts = repeated;
                }
            }
            let mut r = run(fps, &arrivals, kr + 10 * fps + 8);
            let notes = r.p.take_notes().0;
            let ends: Vec<&Note> = notes
                .iter()
                .filter(|n| n.what.starts_with("video_unclamped"))
                .collect();
            assert_eq!(
                ends.len(),
                1,
                "{tag}: the clamp's end: {:?}",
                ends.iter().map(|n| &n.what).collect::<Vec<_>>()
            );
            let relock: f64 = ends[0]
                .what
                .split("relock_ms=")
                .nth(1)
                .and_then(|x| x.trim_end_matches(')').parse().ok())
                .expect("the re-lock");
            let frame_ms = 1_000.0 / fps as f64;
            assert!(
                relock.abs() <= frame_ms,
                "{tag}: the clamp's end moved the picture {relock} ms: {}",
                ends[0].what
            );
            let (live, until) = ((kr + fps) as usize, r.last_air());
            let mut c = Counts::default();
            for x in &r.recs[live..=until] {
                c.add(&x.counts);
            }
            assert_eq!(
                (c.reanchors, c.repeated, c.unaired),
                (0, 0, 0),
                "{tag}: after the return"
            );
            windows += 1;
            ticks += until - live;
        }
        eprintln!("clamp ends beside an audio outlier checked: {windows}, ticks: {ticks}");
        assert_eq!(windows, PROFILES.len());
    }

    /// The re-lock's bound is `PLAYOUT_STREAM_SKEW_MAX_MS`, the re-base's own: an audio offset that far
    /// from the picture's is taken (to the millisecond under it), one past it is refused whole (not
    /// clipped), either sign. A clean clamp's end, as in the broken-run test.
    #[test]
    fn the_re_lock_takes_an_audio_offset_up_to_the_skew_bound_and_no_further() {
        let period = i128::from(NS / 30);
        let bound = ms_ns(PLAYOUT_STREAM_SKEW_MAX_MS);
        let (mut taken, mut refused) = (0, 0);
        // The re-lock an audio offset of 0 asks of the same stamps: minus the picture's own offset.
        let zero = {
            let mut t = VideoStamps::default();
            let mut raw: Vec<i128> = (0..10).map(|k| k * period).collect();
            raw.extend([9 * period + period / 2; 4]);
            raw.extend((0..5).map(|k| 10 * period + k * period));
            for &r in &raw {
                t.place(r, Some(0));
            }
            t.relocked
        };
        assert_ne!(zero, 0, "the picture's offset is not 0 at the end: the premise");
        for (name, moves, want) in [
            ("just under, ahead", bound - NS_PER_MS, true),
            ("just under, behind", -(bound - NS_PER_MS), true),
            ("just over, ahead", bound + NS_PER_MS, false),
            ("just over, behind", -(bound + NS_PER_MS), false),
        ] {
            // The audio offset that asks the picture for `moves`.
            let by = moves - zero;
            let mut t = VideoStamps::default();
            let mut raw: Vec<i128> = (0..10).map(|k| k * period).collect();
            raw.extend([9 * period + period / 2; 4]);
            raw.extend((0..5).map(|k| 10 * period + k * period));
            let mut ends = 0;
            for &r in &raw {
                let (_, c) = t.place(r, Some(by));
                ends += usize::from(c == Clamp::Ends);
            }
            assert_eq!(ends, 1, "{name}: the run ended");
            if want {
                assert_eq!(t.relocked, moves, "{name}: taken whole");
                taken += 1;
            } else {
                assert_eq!(t.relocked, 0, "{name}: refused");
                refused += 1;
            }
        }
        eprintln!("bounds checked: taken {taken}, refused {refused}");
        assert_eq!((taken, refused), (2, 2));
    }

    /// The same, inside the re-lock's bound (a short pause: the picture's offset is under
    /// `PLAYOUT_STREAM_SKEW_MAX_MS`, so only the missing audio keeps the end from moving it by the
    /// pause, taking an audio offset of 0 for the audio's): the end's note says no audio offset and
    /// no re-lock.
    #[test]
    fn a_video_only_cameras_short_pause_is_not_re_locked_onto_nothing() {
        let (mut ends, mut cameras) = (0, 0);
        for profile in PROFILES {
            let fps = programme_fps(profile);
            let pause_ms = PLAYOUT_STREAM_SKEW_MAX_MS - 100;
            let (mut arrivals, kr) = chain_return(fps, pause_ms, 2, Some(0), None, 2, 4);
            for a in &mut arrivals {
                a.audio_at = NO_AUDIO;
            }
            let mut r = run(fps, &arrivals, kr + 4 * fps + 8);
            let notes = r.p.take_notes().0;
            let end: Vec<&Note> = notes
                .iter()
                .filter(|n| n.what.starts_with("video_unclamped"))
                .collect();
            assert_eq!(end.len(), 1, "{fps} fps: the clamp's end");
            assert!(
                end[0].what.contains("audio_offset_ms=-,relock_ms=0.0"),
                "{fps} fps: {}",
                end[0].what
            );
            ends += end.len();
            cameras += 1;
        }
        eprintln!("video-only short pauses checked: {cameras}, clamp ends: {ends}");
        assert_eq!((cameras, ends), (PROFILES.len(), PROFILES.len()));
    }
}
