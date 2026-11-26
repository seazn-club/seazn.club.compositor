//! The output pipeline (C09; spec §3.3's refinement): `appsrc(h264) + appsrc(aac) → flvmux
//! streamable=true → rtmp2sink`, or `filesink` for a `file://` target. Rebuilt on every
//! `swap_sink(gen, target)`; the programme pipeline that feeds it is never rebuilt.
//!
//! The two pipelines are joined only by the **encoded ring**: the programme's encoder appsinks push
//! every encoded sample into it (2 s by PTS, dropping the oldest; plan, Global Constraints), and one
//! feeder thread per output generation takes from it. A new output starts at a video keyframe, and
//! its PTS starts at 0.
//!
//! **A sink sends nothing that was queued before it connected** (Owed 84; owner, 2026-10-24,
//! option A: when the destination recovers after an outage, the queued stale frames are dropped and
//! the stream restarts at the next keyframe, so viewers never get a burst of old frames; F-023).
//! The feeder takes nothing from the ring until the sink can send (`Gate`): a file at once, and
//! rtmp2sink once its publish is accepted. Until then the ring stays the only queue, and it drops
//! its oldest. At that moment (the reconnect point) the feeder asks the encoder for a keyframe (made
//! at once, or, within `FORCED_KEYFRAME_MIN_INTERVAL_MS` of the encoder's last keyframe, at that
//! floor: F-036), and no video frame the ring holds then may start the stream (`Picker`): the first
//! frame sent is a keyframe encoded after the reconnect point, and no audio older than it is sent. A request the
//! encoder does not take is asked again at each entry. A drain that reaches an output that has sent
//! nothing, at the gate or after it, ends with the encoders (`release_unsent`).
//!
//! **One exception: the encoder's first frames** (Owed 112). A sink that can send only after the
//! encoder's first frames, while the ring still holds all of them, at most
//! `FIRST_GOP_CATCH_UP_MAX_MS` of them and under `OUTPUT_QUEUE_BYTES` a track, starts at the
//! encoder's first keyframe and asks nothing (a bounded exception to Owed 84, pending the owner):
//! dropping them would hold the stream to F-036's floor after frame 0, a head outage of up to 2.2 s.
//!
//! **Progress** (Owed 13). A poll every `OUTPUT_PROGRESS_POLL_MS` reads the sink's bytes-SENT total,
//! rtmp2sink `stats` field `out-bytes-total` (F-010), or the file's size for `file://`, and sends
//! `Event::OutputProgress { gen, total_bytes, encoded }`. Never acknowledged bytes: CF Input B sends
//! none (F-005). `encoded` is the encoders' totals at the same reading, the samples each pushed into
//! the ring (Owed 94: the domain's egress backstop counts only while both grow). A bus error sends `Event::OutputError { gen }`, and its text goes to the log only after
//! the scrubber. The sink's `Eos` after `drain()` takes one final total into `Stats` and sends
//! `Event::OutputDrained`.
//!
//! **A destination stall is the sink's own error** (Owed 87; the owner, 2026-10-24, "A"). Every
//! rtmp2sink gets `timeout` = `OUTPUT_SINK_TIMEOUT_MS`, in its unit, whole seconds: a socket wait
//! that long fails the sink, and that bus error is the `OutputError` the domain swaps on. Set, not
//! inherited: it equals GStreamer 1.24.2's default today, and the domain's 15 s budget reads it.
//!
//! **Teardown never blocks the caller.** A retired output is set to NULL on a reaper thread of its
//! own (F-001: `set_state(NULL)` did not return for 10 s on a hung renderer).
use std::collections::VecDeque;
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use app::ports::{Inbound, TargetSpec};
use crossbeam_channel::Sender;
use domain::config::{FIRST_GOP_CATCH_UP_MAX_MS, OUTPUT_SINK_TIMEOUT_MS, X264_DELAY_FRAMES};
use domain::output_link::Encoded;
use domain::supervisor::Event;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

use crate::redact::Scrubber;
use crate::stats::Stats;

/// How often the output's bytes-sent total is read: the domain's `OUTPUT_PROGRESS_POLL_MS` (Owed 15,
/// 250 ms), which moved there because the egress backstop's budget reads it (Owed 94). A destination
/// stall's detection is rtmp2sink's `timeout` (`OUTPUT_SINK_TIMEOUT_S`, Owed 87), not this poll.
pub use domain::config::OUTPUT_PROGRESS_POLL_MS;

/// rtmp2sink's `timeout` (a uint, "RTMP timeout in seconds") on every RTMP sink: the domain's
/// `OUTPUT_SINK_TIMEOUT_MS` (Owed 87) in the property's unit.
pub const OUTPUT_SINK_TIMEOUT_S: u32 = (OUTPUT_SINK_TIMEOUT_MS / 1_000) as u32;
// The property counts whole seconds and 0 means no timeout: a constant it cannot carry exactly is a
// compile error, never a rounded or disabled detector.
const _: () = assert!(
    OUTPUT_SINK_TIMEOUT_MS.is_multiple_of(1_000)
        && OUTPUT_SINK_TIMEOUT_MS >= 1_000
        && OUTPUT_SINK_TIMEOUT_MS / 1_000 <= u32::MAX as u64
);

/// The encoded ring's span (plan, Global Constraints: "Encoded ring buffer 2 s, dropping the oldest"):
/// how far behind its own track's newest entry an entry may be (`prune`). Per track since F-040, so
/// the audio running ahead of a late video track never drops the video.
pub const ENCODED_RING_SPAN: Duration = Duration::from_secs(2);
// Owed 112: the first-GOP backlog a sink starts from is one the ring can still hold. The span is per
// track (`prune`), so the video track alone bounds that backlog; the audio's lead over a delayed video
// encoder (`X264_DELAY_MS`) drops nothing and is not a term here (the review's m2).
const _: () = assert!((FIRST_GOP_CATCH_UP_MAX_MS as u128) < ENCODED_RING_SPAN.as_millis());
/// A hard cap on the ring's entries beside its span, so that samples with no PTS cannot grow it:
/// 2 s holds 60 video frames at 30 fps and about 94 AAC frames at 48 kHz (1 024 samples each), 154 in
/// all; twice that is the cap.
pub const ENCODED_RING_MAX_ENTRIES: usize = 308;
/// The feeder stops taking from the ring when the output's two appsrcs both hold data and either holds
/// at least this much: the sink is not consuming, so the backlog stays in the (bounded) ring rather
/// than in the output. 512 KiB is about 1 s at 4 Mbps.
pub const OUTPUT_QUEUE_BYTES: u64 = 512 * 1024;
/// How long the feeder and the bus thread wait before looking again (a retire is seen within it).
const WAKE: Duration = Duration::from_millis(20);
/// The bus thread's longest wait for one message.
const BUS_WAIT: Duration = Duration::from_millis(50);

pub(crate) type LogFn = Arc<dyn Fn(&str) + Send + Sync>;
/// Asks the programme's encoder for a keyframe (a force-key-unit event); false if it was not taken.
/// The argument is the running time from which a keyframe serves (the reconnect point's: just after
/// the newest video the ring has taken, `RingState::newest_video`): GstVideoEncoder makes none when
/// it has already made one at or after it (F-036, the C1 review's I1), so a GOP's own keyframe
/// encoded after the reconnect point starts the stream with no forced one a floor behind it. Never
/// asked before the ring has taken any video (`Feeder::run`): the encoder's first frame is a keyframe
/// anyway, and a request it matched there would hold every later one off for the encoder's life.
pub(crate) type KeyRequest = Arc<dyn Fn(gst::ClockTime) -> bool + Send + Sync>;

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// One encoded sample, its PTS already in running time (x264enc's output segment starts at 1 000 h,
/// so a raw PTS would put the video 1 000 h ahead of the audio: spike trap, F-001's chain). `S` is
/// the sample, a `gst::Sample` everywhere but in the `Picker`'s own tests.
#[derive(Clone)]
pub(crate) struct Entry<S = gst::Sample> {
    seq: u64,
    pub(crate) sample: S,
    pub(crate) video: bool,
    pub(crate) key: bool,
    pub(crate) pts: gst::ClockTime,
}

impl<S> Entry<S> {
    pub(crate) fn new(sample: S, video: bool, key: bool, pts: gst::ClockTime) -> Self {
        Self {
            seq: 0,
            sample,
            video,
            key,
            pts,
        }
    }
}

#[derive(Default)]
struct RingState {
    entries: VecDeque<Entry>,
    next_seq: u64,
    video_eos: bool,
    audio_eos: bool,
    /// The output generation allowed to take entries; 0 = no output.
    owner: u64,
    /// `OutputDrained` was sent (it carries no generation, so it is sent once in all).
    drained_sent: bool,
    /// The samples each encoder has pushed, dropped ones included: what the encoders delivered.
    encoded: Encoded,
    /// The running time of the newest video entry ever pushed, dropped ones included (F-036): a
    /// keyframe after it was encoded after any reconnect point taken now.
    newest_video: Option<gst::ClockTime>,
    /// The video entries the encoder has delivered since its newest delivered KEYFRAME (F-040; 0 at the
    /// keyframe itself), `None` before any keyframe. Counted in frames, the unit x264 places its next
    /// keyframe in: a frame dropped before the encoder never reaches the ring, so it cannot move this
    /// (`own_key_in_flight`; a time rule drifted by the frames lost inside the GOP).
    video_since_key: Option<u64>,
    /// The programme's GOP in frames (`key-int-max`), 0 until the programme sets it.
    gop_frames: u32,
    /// The forced keyframe's floor in frames (`FORCED_KEYFRAME_MIN_INTERVAL_MS`), 0 until the
    /// programme sets it (C7: `forced_key_can_beat_the_gop` reads it with the GOP).
    floor_frames: u32,
    /// The entries the ring dropped for its span or its count, per track (the test hooks read it).
    #[cfg(feature = "test-hooks")]
    pruned: Encoded,
}

/// The programme → output hand-off: bounded by span (per track) and by count, dropping the oldest.
#[derive(Default)]
pub(crate) struct EncodedRing {
    state: Mutex<RingState>,
    wake: Condvar,
}

impl EncodedRing {
    pub(crate) fn push(&self, mut e: Entry) {
        let mut r = locked(&self.state);
        e.seq = r.next_seq;
        r.next_seq += 1;
        if e.video {
            r.encoded.video += 1;
            r.newest_video = Some(r.newest_video.map_or(e.pts, |n| n.max(e.pts)));
            r.video_since_key = since_key_after(r.video_since_key, e.key);
        } else {
            r.encoded.audio += 1;
        }
        r.entries.push_back(e);
        let dropped = prune(&mut r.entries);
        #[cfg(feature = "test-hooks")]
        {
            r.pruned.video += dropped.0;
            r.pruned.audio += dropped.1;
        }
        #[cfg(not(feature = "test-hooks"))]
        let _ = dropped;
        drop(r);
        self.wake.notify_all();
    }

    /// What the encoders have pushed so far, per track (Owed 94; every progress reading carries it).
    pub(crate) fn encoded(&self) -> Encoded {
        locked(&self.state).encoded
    }

    /// The entries held now (the test hooks read it).
    #[cfg(feature = "test-hooks")]
    pub(crate) fn len(&self) -> usize {
        locked(&self.state).entries.len()
    }

    /// The programme's GOP in frames, set when it builds its encoder (`own_key_in_flight`), and the
    /// forced keyframe's floor in frames (C7, `forced_key_can_beat_the_gop`).
    pub(crate) fn set_gop_frames(&self, gop_frames: u32, floor_frames: u32) {
        let mut r = locked(&self.state);
        r.gop_frames = gop_frames;
        r.floor_frames = floor_frames;
    }

    /// The entries dropped for the ring's span or count so far, (video, audio) (the test hooks read it).
    #[cfg(feature = "test-hooks")]
    pub(crate) fn pruned(&self) -> (u64, u64) {
        let r = locked(&self.state);
        (r.pruned.video, r.pruned.audio)
    }

    /// Drops the oldest entries as an overflowing ring does, up to the first video entry that is
    /// not a keyframe (at least one is dropped), so the next entry a feeder takes follows a gap and
    /// cannot start a picture (the test hooks plant it). Returns how many were dropped.
    #[cfg(feature = "test-hooks")]
    pub(crate) fn drop_to_delta(&self) -> usize {
        let mut r = locked(&self.state);
        let mut dropped = 0;
        while let Some(front) = r.entries.front() {
            if dropped > 0 && front.video && !front.key {
                break;
            }
            r.entries.pop_front();
            dropped += 1;
        }
        dropped
    }

    pub(crate) fn end_of_stream(&self, video: bool) {
        let mut r = locked(&self.state);
        if video {
            r.video_eos = true;
        } else {
            r.audio_eos = true;
        }
        drop(r);
        self.wake.notify_all();
    }

    /// Both encoders have reached EOS and no output exists: there is nothing to drain (what the
    /// ring still holds has no sink to go to), and `OutputDrained` is due now (once).
    pub(crate) fn take_drained_without_output(&self) -> bool {
        let mut r = locked(&self.state);
        if r.video_eos && r.audio_eos && r.owner == 0 && !r.drained_sent {
            r.drained_sent = true;
            return true;
        }
        false
    }

    pub(crate) fn take_drained(&self) -> bool {
        let mut r = locked(&self.state);
        !std::mem::replace(&mut r.drained_sent, true)
    }

    /// An output reached EOS after the programme's drain reached the ring (both encoders at EOS):
    /// the EOS is that drain's, and `OutputDrained` is due now (once). An output's EOS before then
    /// is a failed sink's aftermath (each appsrc whose push fails sends one down; F-011), not a
    /// drain, and must not spend the once-only event.
    pub(crate) fn take_drained_at_eos(&self) -> bool {
        let mut r = locked(&self.state);
        if r.video_eos && r.audio_eos && !r.drained_sent {
            r.drained_sent = true;
            return true;
        }
        false
    }

    fn set_owner(&self, gen: u64) {
        locked(&self.state).owner = gen;
        self.wake.notify_all();
    }

    fn release(&self, gen: u64) {
        let mut r = locked(&self.state);
        if r.owner == gen {
            r.owner = 0;
        }
        drop(r);
        self.wake.notify_all();
    }
}

/// Counts `gone` against its track in `dropped`, (video, audio).
fn count_dropped<S>(gone: Option<Entry<S>>, dropped: &mut (u64, u64)) {
    match gone {
        Some(e) if e.video => dropped.0 += 1,
        Some(_) => dropped.1 += 1,
        None => {}
    }
}

/// What the ring's two bounds drop once an entry has been pushed on `entries`' back; returns how many
/// of each track, (video, audio). The count bound takes the oldest entries. The span bound is PER
/// TRACK (F-040): an entry goes when it is more than `ENCODED_RING_SPAN` behind the newest entry of
/// its own track, the one just pushed. Measured across tracks, a video encoder that is late (its
/// frame delay, or a stall) had its pictures dropped by the audio, which is not late: the first
/// output after a 2 s stall saw the stalled encoder's own in-flight frames more than a span behind
/// the audio, and one dropped picture made the output wait for the encoder's next natural keyframe,
/// up to a GOP (`PROGRAMME_GOP_MS`) of picture later, for every picture that followed it.
pub(crate) fn prune<S>(entries: &mut VecDeque<Entry<S>>) -> (u64, u64) {
    let mut dropped = (0u64, 0u64);
    while entries.len() > ENCODED_RING_MAX_ENTRIES {
        count_dropped(entries.pop_front(), &mut dropped);
    }
    let Some((video, newest)) = entries.back().map(|e| (e.video, e.pts)) else {
        return dropped;
    };
    while let Some(i) = entries.iter().position(|e| e.video == video) {
        if newest.saturating_sub(entries[i].pts) <= span() {
            break;
        }
        count_dropped(entries.remove(i), &mut dropped);
    }
    dropped
}

fn span() -> gst::ClockTime {
    gst::ClockTime::from_nseconds(ENCODED_RING_SPAN.as_nanos() as u64)
}

/// What the output threads share with the programme.
#[derive(Clone)]
pub(crate) struct Shared {
    pub(crate) ring: Arc<EncodedRing>,
    pub(crate) tx: Sender<Inbound>,
    pub(crate) log: LogFn,
    pub(crate) stats: Arc<Stats>,
    pub(crate) scrubber: Arc<Mutex<Scrubber>>,
    /// Teardowns (`set_state(NULL)` on a reaper thread) not yet returned.
    pub(crate) reapers: Arc<AtomicUsize>,
    /// Progress events not sent because the inbound channel was full (the next poll carries a newer
    /// total, so none is owed).
    pub(crate) progress_dropped: Arc<AtomicU64>,
    /// The `timeout` each new rtmp2sink gets: `OUTPUT_SINK_TIMEOUT_S`, which only a test hook
    /// changes (`GstProgramme::set_output_sink_timeout_s`), so a read-back can tell the set from the
    /// element's default (Owed 87).
    pub(crate) sink_timeout_s: u32,
}

impl Shared {
    pub(crate) fn log(&self, line: &str) {
        let clean = locked(&self.scrubber).scrub(line);
        (self.log)(&clean);
    }

    /// An event the domain must see (an error, the drain): this thread waits for room.
    fn send(&self, e: Event) {
        let _ = self.tx.send(Inbound::Event(e));
    }
}

/// Where an output goes.
#[derive(Clone, PartialEq, Eq)]
enum Destination {
    Rtmp(String),
    File(PathBuf),
}

impl Destination {
    /// `rtmp://` or `rtmps://` plus the stream key as the last path segment, or a `file://` path.
    /// Anything else, or an RTMP target with no key, is refused; the text never holds the key.
    fn parse(target: &TargetSpec) -> Result<Destination, String> {
        let url = target.url.trim();
        if let Some(path) = url.strip_prefix("file://") {
            if path.is_empty() {
                return Err("output target file:// has no path".to_owned());
            }
            return Ok(Destination::File(PathBuf::from(path)));
        }
        let lower = url.to_ascii_lowercase();
        if lower.starts_with("rtmp://") || lower.starts_with("rtmps://") {
            if target.stream_key.trim().is_empty() {
                return Err("output target has no stream key".to_owned());
            }
            let base = url.trim_end_matches('/');
            return Ok(Destination::Rtmp(format!("{base}/{}", target.stream_key)));
        }
        Err(format!(
            "output target scheme is not rtmp, rtmps or file: {}",
            app::ports::redacted_origin(url)
        ))
    }
}

/// One output generation: its pipeline and the flag its threads stop on.
pub(crate) struct OutputGen {
    gen: u64,
    pipeline: gst::Pipeline,
    #[cfg(feature = "test-hooks")]
    sink: gst::Element,
    #[cfg(feature = "test-hooks")]
    srcs: (gst_app::AppSrc, gst_app::AppSrc),
    #[cfg(feature = "test-hooks")]
    first_pts: FirstPts,
    retired: Arc<AtomicBool>,
}

/// The first PTS each output appsrc sent downstream, (video, audio) (the test hooks read it).
#[cfg(feature = "test-hooks")]
type FirstPts = Arc<Mutex<(Option<gst::ClockTime>, Option<gst::ClockTime>)>>;

impl fmt::Debug for OutputGen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OutputGen").field("gen", &self.gen).finish()
    }
}

impl OutputGen {
    pub(crate) fn gen(&self) -> u64 {
        self.gen
    }

    /// The sink element (the test hooks hold its writes).
    #[cfg(feature = "test-hooks")]
    pub(crate) fn sink(&self) -> &gst::Element {
        &self.sink
    }

    /// Posts an error on this output's bus, as a failing sink would (the test hooks plant it).
    #[cfg(feature = "test-hooks")]
    pub(crate) fn plant_error(&self) -> bool {
        let msg = gst::message::Error::builder(gst::ResourceError::Write, "planted output error")
            .src(&self.sink)
            .build();
        self.pipeline.post_message(msg).is_ok()
    }

    /// The first PTS each appsrc sent downstream (the test hooks read it).
    #[cfg(feature = "test-hooks")]
    pub(crate) fn first_pts(&self) -> (Option<gst::ClockTime>, Option<gst::ClockTime>) {
        *locked(&self.first_pts)
    }

    /// The bytes queued in the video and audio appsrcs (the test hooks read them).
    #[cfg(feature = "test-hooks")]
    pub(crate) fn queued(&self) -> (u64, u64) {
        (
            self.srcs.0.current_level_bytes(),
            self.srcs.1.current_level_bytes(),
        )
    }

    /// Stops this generation's threads and hands its pipeline to a reaper thread, which sets it to
    /// NULL. Returns at once.
    pub(crate) fn retire(self, shared: &Shared) {
        self.retired.store(true, Ordering::SeqCst);
        shared.ring.release(self.gen);
        reap(shared, self.pipeline.upcast(), format!("output gen={}", self.gen));
    }
}

/// Sets `element` to NULL on a thread of its own, counted in `shared.reapers` until it returns.
pub(crate) fn reap(shared: &Shared, element: gst::Element, what: String) {
    let reapers = Arc::clone(&shared.reapers);
    reapers.fetch_add(1, Ordering::SeqCst);
    let log = shared.clone();
    let what_bg = what.clone();
    let spawned = thread::Builder::new().name("gst-reaper".to_owned()).spawn({
        let reapers = Arc::clone(&reapers);
        move || {
            if element.set_state(gst::State::Null).is_err() {
                log.log(&format!("{what_bg}: teardown to NULL failed"));
            }
            drop(element);
            reapers.fetch_sub(1, Ordering::SeqCst);
        }
    });
    if spawned.is_err() {
        // No thread: the pipeline is leaked rather than torn down on the caller's thread, which
        // must never block (F-001).
        reapers.fetch_sub(1, Ordering::SeqCst);
        shared.log(&format!(
            "{what}: no reaper thread; the pipeline is left as it is"
        ));
    }
}

fn make(name: &str) -> Result<gst::Element, String> {
    gst::ElementFactory::make(name)
        .build()
        .map_err(|e| format!("output: no {name} element: {e}"))
}

/// Builds and starts generation `gen` for `target`, and starts its feeder and poll threads. The
/// feeder asks for the encoder's next keyframe through `request_key` when the sink can send (Owed 84).
pub(crate) fn build(
    shared: &Shared,
    gen: u64,
    target: &TargetSpec,
    request_key: KeyRequest,
) -> Result<OutputGen, String> {
    let dest = Destination::parse(target)?;
    let pipeline = gst::Pipeline::new();
    let vsrc = make("appsrc")?;
    let asrc = make("appsrc")?;
    let mux = make("flvmux")?;
    mux.set_property("streamable", true);
    let sink = match &dest {
        Destination::Rtmp(location) => {
            let s = make("rtmp2sink")?;
            s.set_property("location", location.as_str());
            s.set_property("sync", false);
            // The gate (Owed 84) waits for the publish before it feeds a byte, so the connect must
            // start without data: rtmp2sink's default, set here because the gate depends on it (with
            // `false` it connects on its first buffer, which the gate never sends).
            s.set_property("async-connect", true);
            // The only detector of a destination stall (Owed 87): see the module doc.
            s.set_property("timeout", shared.sink_timeout_s);
            s
        }
        Destination::File(path) => {
            let s = make("filesink")?;
            s.set_property("location", path.to_string_lossy().as_ref());
            s.set_property("sync", false);
            s
        }
    };
    pipeline
        .add_many([&vsrc, &asrc, &mux, &sink])
        .map_err(|e| format!("output: {e}"))?;
    mux.link(&sink).map_err(|e| format!("output: mux to sink: {e}"))?;
    link_to_mux(&vsrc, &mux, "video")?;
    link_to_mux(&asrc, &mux, "audio")?;
    let vsrc = into_appsrc(vsrc)?;
    let asrc = into_appsrc(asrc)?;
    for src in [&vsrc, &asrc] {
        src.set_format(gst::Format::Time);
        src.set_is_live(false);
        src.set_property("block", false);
        // The feeder bounds what it queues here (`OUTPUT_QUEUE_BYTES`); appsrc's own cap only
        // signals, so it is set out of the way.
        src.set_max_bytes(0);
    }
    // The operator's one line naming where this generation sends: through `Shared::log`, which
    // scrubs the key out of it.
    let shown = match &dest {
        Destination::Rtmp(location) => location.clone(),
        Destination::File(path) => format!("file://{}", path.display()),
    };
    shared.log(&format!("output gen={gen}: sink {shown}"));
    if let Err(e) = pipeline.set_state(gst::State::Playing) {
        // A sink that cannot open (a file in a missing directory) leaves elements past NULL.
        reap(shared, pipeline.upcast(), format!("output gen={gen}"));
        return Err(format!("output gen={gen}: did not start: {e}"));
    }

    let stats_sink = shared.stats.on_new_sink();
    shared.ring.set_owner(gen);
    let retired = Arc::new(AtomicBool::new(false));

    #[cfg(feature = "test-hooks")]
    let srcs = (vsrc.clone(), asrc.clone());
    #[cfg(feature = "test-hooks")]
    let first_pts = record_first_pts(&vsrc, &asrc);
    let feeder = Feeder {
        gen,
        vsrc,
        asrc,
        ring: Arc::clone(&shared.ring),
        retired: Arc::clone(&retired),
        gate: match &dest {
            Destination::Rtmp(_) => Gate::Publish(sink.clone()),
            Destination::File(_) => Gate::Open,
        },
        request_key,
        shared: shared.clone(),
    };
    thread::Builder::new()
        .name(format!("output-feed-{gen}"))
        .spawn(move || feeder.run())
        .map_err(|e| format!("output gen={gen}: no feeder thread: {e}"))?;

    let watch = Watch {
        gen,
        stats_sink,
        dropped_streak: 0,
        bus: pipeline.bus().ok_or("output: pipeline has no bus")?,
        sink: sink.clone(),
        file: match &dest {
            Destination::File(p) => Some(p.clone()),
            Destination::Rtmp(_) => None,
        },
        shared: shared.clone(),
        retired: Arc::clone(&retired),
    };
    thread::Builder::new()
        .name(format!("output-watch-{gen}"))
        .spawn(move || watch.run())
        .map_err(|e| format!("output gen={gen}: no watch thread: {e}"))?;

    Ok(OutputGen {
        gen,
        pipeline,
        #[cfg(feature = "test-hooks")]
        sink,
        #[cfg(feature = "test-hooks")]
        srcs,
        #[cfg(feature = "test-hooks")]
        first_pts,
        retired,
    })
}

/// A probe on each appsrc's src pad that records the first buffer's PTS, then removes itself.
#[cfg(feature = "test-hooks")]
fn record_first_pts(vsrc: &gst_app::AppSrc, asrc: &gst_app::AppSrc) -> FirstPts {
    let first: FirstPts = Arc::default();
    for (video, src) in [(true, vsrc), (false, asrc)] {
        let Some(pad) = src.static_pad("src") else {
            continue;
        };
        let first = Arc::clone(&first);
        pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            let pts = info.buffer().and_then(|b| b.pts());
            let mut f = locked(&first);
            if video {
                f.0 = pts;
            } else {
                f.1 = pts;
            }
            gst::PadProbeReturn::Remove
        });
    }
    first
}

fn link_to_mux(src: &gst::Element, mux: &gst::Element, template: &str) -> Result<(), String> {
    let pad = mux
        .request_pad_simple(template)
        .ok_or_else(|| format!("output: flvmux gave no {template} pad"))?;
    src.static_pad("src")
        .ok_or("output: appsrc has no src pad")?
        .link(&pad)
        .map(|_| ())
        .map_err(|e| format!("output: {template} link: {e:?}"))
}

fn into_appsrc(e: gst::Element) -> Result<gst_app::AppSrc, String> {
    e.downcast::<gst_app::AppSrc>()
        .map_err(|_| "output: appsrc is not an AppSrc".to_owned())
}

/// When an output generation may start taking the ring (Owed 84).
enum Gate {
    /// A file is written as soon as the pipeline plays: open at the build.
    Open,
    /// rtmp2sink, open once its publish is accepted. Measured in GStreamer 1.24.2's
    /// `gstrtmp2sink.c`: `render` waits while `self->connection` is unset, and `connect_task_done`
    /// sets it only after the publish's `NetStream.Publish.Start` (`start_publish_done`). `stats`
    /// reads that connection's counters; without one, the counters saved when a connection ended,
    /// and the null stats (every field 0) before any. Each build makes a new rtmp2sink, whose saved
    /// counters are cleared when its connect task starts, so this sink's `out-bytes-total` leaves 0
    /// exactly when its publish is accepted: the connection has sent its `connect` command by then
    /// (476 B, F-011). Anything fed before that queues in front of a sink that cannot send, and goes
    /// out in one burst at the connect (F-023). The gate is read at each ring push and each `WAKE`
    /// while the sink connects, about 100 reads a second, each a lock and a copy of the stats
    /// structure; it stops at the open (F-027).
    Publish(gst::Element),
}

impl Gate {
    fn is_open(&self) -> bool {
        match self {
            Gate::Open => true,
            Gate::Publish(sink) => rtmp_total(sink) > 0,
        }
    }
}

/// rtmp2sink `stats`'s `out-bytes-total` (F-010): the bytes its connection has sent, 0 before it has
/// one.
fn rtmp_total(sink: &gst::Element) -> u64 {
    // GStreamer 1.24's `stats` contract (F-027); `the_gate_reads_gstreamer_1_24` pins the version.
    sink.property::<gst::Structure>("stats")
        .get::<u64>("out-bytes-total")
        .unwrap_or(0)
}

/// Takes the ring into one output generation.
struct Feeder {
    gen: u64,
    vsrc: gst_app::AppSrc,
    asrc: gst_app::AppSrc,
    ring: Arc<EncodedRing>,
    retired: Arc<AtomicBool>,
    gate: Gate,
    request_key: KeyRequest,
    shared: Shared,
}

enum Next {
    Entry(Entry),
    Eos,
    Stop,
}

/// Where an output generation's stream may start, read when its sink can send (`reconnect_point`).
struct Reconnect {
    /// The seq of the first entry that may start the stream (`Picker::new`): the first encoded after
    /// the gate opened, or, with `first_gop`, the oldest of those entries.
    seq: u64,
    /// The running time a keyframe serves from: 1 ns after the newest video the ring has taken;
    /// `None` before any, when nothing is asked (`run`).
    serves_from: Option<gst::ClockTime>,
    /// Owed 112: the encoder's first frames, taken out of the ring at the gate, from which the
    /// stream starts at the encoder's first keyframe; empty on every other path.
    first_gop: VecDeque<Entry>,
}

/// The keyframe asked for at the reconnect point (Owed 84): asked again at each entry until the
/// encoder takes it or the stream has started, so a refused request does not leave the stream
/// waiting for the encoder's own keyframe, up to a GOP away (3 s, F-036; 2 s since C7; PR #57 review m-6). With the floor over the GOP (C7) none is asked (`forced_key_can_beat_the_gop`), so the retry is reached only by a GOP over the floor. The
/// request carries the running time a keyframe serves from (`KeyRequest`).
#[derive(Default)]
struct KeyAsk {
    tries: u32,
    taken: bool,
    /// `ask_key` has said that nothing is asked (no video yet): said once, at its first call.
    said_none: bool,
    /// `ask_key` has said that nothing is asked because the GOP's own keyframe is in the encoder
    /// (F-040, `own_key_in_flight`): said once.
    said_in_flight: bool,
    /// `ask_key` has said that nothing is asked because the forced keyframe's floor is not under the
    /// GOP (C7, `forced_key_can_beat_the_gop`): said once.
    said_floor_over_gop: bool,
}

impl Feeder {
    fn run(self) {
        let Some(Reconnect {
            seq,
            serves_from,
            first_gop,
        }) = self.reconnect_point()
        else {
            return;
        };
        let mut pick = Picker::new(seq);
        let mut ask = KeyAsk::default();
        // Owed 112: the encoder's first frames, taken out of the ring at the reconnect point, start
        // the stream at its first keyframe; nothing is asked then (`pick.started()` below). Empty
        // on every other path.
        for e in first_gop {
            pick.take(e, |e, base| self.push(e, base));
        }
        // F-036 (CI on PR #109): before the ring has taken any video, nothing is asked. x264's first
        // frame is a keyframe, and every video entry comes after the reconnect point, so that frame
        // starts the stream. A request there would also be matched on that frame at a running time
        // 1 000 h too late: GstVideoEncoder 1.24.2 reads a frame's running time against its OUTPUT
        // segment (`gst_video_encoder_chain`, gstvideoencoder.c 1596-1598), which is set only when
        // the segment goes out with the first frame (`gst_video_encoder_push_event`, 1045), while
        // x264enc's min-pts has moved that frame's PTS on by 1 000 h (gstx264enc.c 1418; 1574-1583).
        // The 1 000 h becomes `last_force_key_unit_request` (1696), and the floor then throttles
        // every later forced keyframe (1600-1607) until the encoder stops: each later sink waits for
        // the GOP's own keyframe, up to a GOP. `ask_key`'s `None` branch is that guard, and it writes
        // the line the tests wait on (the C1 review's m1: the line and the guard are one branch).
        if !pick.started() {
            self.ask_key(&mut ask, serves_from);
        }
        loop {
            match self.next() {
                Next::Stop => return,
                Next::Eos => {
                    if !pick.started() {
                        self.release_unsent();
                    }
                    let _ = self.vsrc.end_of_stream();
                    let _ = self.asrc.end_of_stream();
                    return;
                }
                Next::Entry(e) => {
                    if !pick.started() {
                        self.ask_key(&mut ask, serves_from);
                    }
                    pick.take(e, |e, base| self.push(e, base));
                }
            }
        }
    }

    /// Asks the encoder for a keyframe, one from `serves_from` on, unless it has taken a request
    /// already or there is nothing to ask (`None`: no video yet, `run`). The take is logged (the C1
    /// review's m4: the line follows the ask, so a reader of it knows the request is with the
    /// encoder), and so is the `None` refusal, at the first call only (the C1 review's m1: the line
    /// `swap_before_the_first_frame` waits on is written by the branch that refuses, so a request
    /// made with no video cannot leave that line standing).
    fn ask_key(&self, ask: &mut KeyAsk, serves_from: Option<gst::ClockTime>) {
        let Some(serves_from) = serves_from else {
            if !std::mem::replace(&mut ask.said_none, true) {
                self.shared.log(&format!(
                    "output gen={}: no keyframe is asked: the ring has taken no video yet, so the \
                     encoder's first frame, a keyframe, starts the stream",
                    self.gen
                ));
            }
            return;
        };
        if ask.taken {
            return;
        }
        // C7: with the floor over the GOP, a keyframe asked for can only be made after the GOP's own
        // (held to the floor), and, the GOP's own being in x264's delay, right behind it: a GOP under
        // Cloudflare's floor. The GOP's own keyframe serves every swap, at most a GOP away.
        let (can_beat, gop, floor) = {
            let r = locked(&self.ring.state);
            (
                forced_key_can_beat_the_gop(r.gop_frames, r.floor_frames),
                r.gop_frames,
                r.floor_frames,
            )
        };
        if !can_beat {
            if !std::mem::replace(&mut ask.said_floor_over_gop, true) {
                self.shared.log(&format!(
                    "output gen={}: {FLOOR_OVER_GOP_LINE} (floor {floor} frames, GOP {gop}): the GOP's own \
                     keyframe, at most a GOP on, serves this sink",
                    self.gen
                ));
            }
            return;
        }
        // F-040: the GOP's own keyframe is in the encoder's delay, due within `X264_DELAY_MS`, and
        // serves this swap (it is after `serves_from`): a request would be matched on a frame this
        // side of it, a GOP of the frames between under Cloudflare's floor. Read live, at every
        // entry: a keyframe that does not come (the model wrong) lets the request through.
        let due = {
            let r = locked(&self.ring.state);
            own_key_in_flight(r.video_since_key, r.gop_frames)
        };
        if let Some(due) = due {
            if !std::mem::replace(&mut ask.said_in_flight, true) {
                self.shared.log(&format!(
                    "output gen={}: the keyframe is not asked: the GOP's own keyframe, due {due} frames \
                     on, is in the encoder's delay and serves this sink",
                    self.gen
                ));
            }
            return;
        }
        ask.tries += 1;
        let taken = (self.request_key)(serves_from);
        ask.taken = taken;
        if taken {
            self.shared.log(&format!(
                "output gen={}: the keyframe request was taken at try {}; a keyframe from running \
                 time {serves_from} serves it",
                self.gen, ask.tries
            ));
        } else if ask.tries == 1 {
            self.shared.log(&format!(
                "output gen={}: the keyframe request was not taken; it is asked again at each frame",
                self.gen
            ));
        }
    }

    /// The drain reached an output that has sent nothing, at the gate or after it (PR #57 review
    /// I-1). Nothing is owed to it, and it could never drain: flvmux 1.24.2 forwards no EOS before
    /// its first buffer (`gst_flv_mux_aggregate` asks for data in its header state, gstflvmux.c
    /// 2057-2059), so its sink never posts the EOS a drain waits for. The ring is released, and the
    /// drain ends with the encoders, as after a sink error (`Watch`).
    fn release_unsent(&self) {
        self.ring.release(self.gen);
        if self.ring.take_drained_without_output() {
            self.shared.send(Event::OutputDrained);
        }
    }

    /// Waits, taking nothing, until the sink can send (`Gate`), and returns the reconnect point
    /// (`Reconnect`). `None`: retired, the ring is another generation's, or both encoders ended
    /// first. That last is the programme's drain reaching a sink that never connected: nothing is
    /// owed to it (whatever the ring holds predates any connect), so `release_unsent`.
    ///
    /// Owed 112: when the ring holds only the encoder's first frames, at most
    /// `FIRST_GOP_CATCH_UP_MAX_MS` of them (`first_gop_backlog`) and under `OUTPUT_QUEUE_BYTES` a
    /// track (`first_gop_over_queue`), they are taken out of it under the same lock, so the ring
    /// cannot drop the first keyframe before the feeder sends it.
    fn reconnect_point(&self) -> Option<Reconnect> {
        let since = Instant::now();
        loop {
            if self.retired.load(Ordering::SeqCst) {
                return None;
            }
            if self.gate.is_open() {
                let mut r = locked(&self.ring.state);
                if r.owner != self.gen {
                    return None;
                }
                let serves_from = r.newest_video.map(|t| t + gst::ClockTime::from_nseconds(1));
                let held = r.entries.iter().filter(|e| e.video).count();
                let waited = since.elapsed().as_millis();
                let backlog = first_gop_backlog(&r.entries, r.encoded.video);
                let over = backlog.and(first_gop_over_queue(&r.entries, sample_bytes));
                if let (Some(_), Some(bytes)) = (backlog, over) {
                    self.shared.log(&format!(
                        "output gen={}: the encoder's first frames hold {bytes} B on one track, over \
                         the output queue's {OUTPUT_QUEUE_BYTES} B (Owed 112), so they are dropped",
                        self.gen
                    ));
                }
                if let (Some(backlog), None) = (backlog, over) {
                    let first_gop = std::mem::take(&mut r.entries);
                    let seq = first_gop.front().map_or(r.next_seq, |e| e.seq);
                    drop(r);
                    self.shared.log(&format!(
                        "output gen={}: the sink can send after {waited} ms; the {held} video frames \
                         queued before it are the encoder's first, {} ms from its first keyframe \
                         (Owed 112), so it starts at the encoder's first keyframe and no keyframe is \
                         asked",
                        self.gen,
                        backlog.mseconds()
                    ));
                    return Some(Reconnect {
                        seq,
                        serves_from,
                        first_gop,
                    });
                }
                let seq = r.next_seq;
                drop(r);
                self.shared.log(&format!(
                    "output gen={}: the sink can send after {waited} ms; the {held} video frames queued \
                     before it are dropped, and it starts at the next keyframe",
                    self.gen
                ));
                return Some(Reconnect {
                    seq,
                    serves_from,
                    first_gop: VecDeque::new(),
                });
            }
            let r = locked(&self.ring.state);
            if r.owner != self.gen {
                return None;
            }
            if r.video_eos && r.audio_eos {
                drop(r);
                self.release_unsent();
                return None;
            }
            drop(
                self.ring
                    .wake
                    .wait_timeout(r, WAKE)
                    .unwrap_or_else(PoisonError::into_inner),
            );
        }
    }

    /// Pushes `e` rebased to `base`.
    fn push(&self, e: &Entry, base: gst::ClockTime) {
        let src = if e.video { &self.vsrc } else { &self.asrc };
        if let Some(sample) = rebased(&e.sample, base) {
            let _ = src.push_sample(&sample);
        }
    }

    fn next(&self) -> Next {
        let mut r = locked(&self.ring.state);
        loop {
            if self.retired.load(Ordering::SeqCst) || r.owner != self.gen {
                return Next::Stop;
            }
            if r.entries.is_empty() {
                if r.video_eos && r.audio_eos {
                    return Next::Eos;
                }
            } else if !self.backed_up() {
                let e = r.entries.pop_front().expect("checked non-empty");
                return Next::Entry(e);
            }
            r = self
                .ring
                .wake
                .wait_timeout(r, WAKE)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// The sink is not consuming: both appsrcs hold data, so the muxer has what it needs, and one of
    /// them holds `OUTPUT_QUEUE_BYTES`. A pad that is empty is never waited on (the muxer may be
    /// waiting for exactly that pad).
    fn backed_up(&self) -> bool {
        let v = self.vsrc.current_level_bytes();
        let a = self.asrc.current_level_bytes();
        v > 0 && a > 0 && (v >= OUTPUT_QUEUE_BYTES || a >= OUTPUT_QUEUE_BYTES)
    }
}

/// Which ring entries one output generation sends, and the base each is rebased by. No GStreamer
/// call: the feeder pushes what `take` hands it.
///
/// - The first entry sent is a video keyframe, its PTS the base (0 on the wire). A keyframe whose
///   seq is below the reconnect point was queued before the sink could send, so it never starts the
///   stream, and neither does any frame behind it (Owed 84).
/// - Audio that reaches the ring before that keyframe is held, bounded like the ring: the audio
///   encoder runs about 60 ms ahead of the video one, so a keyframe's first audio arrives before it
///   (dropping it lost 2 or 3 AAC frames per sink). Once the keyframe sets the base, the held audio
///   at or after it is sent, and no audio before it, so the audio never starts ahead of the video.
/// - A seq gap (the ring dropped entries this output never took) makes the video resume at a
///   keyframe.
pub(crate) struct Picker<S = gst::Sample> {
    reconnect: u64,
    base: Option<gst::ClockTime>,
    need_key: bool,
    expected_seq: Option<u64>,
    early_audio: VecDeque<Entry<S>>,
}

impl<S> Picker<S> {
    /// `reconnect`: the seq of the first entry encoded after the sink could send.
    pub(crate) fn new(reconnect: u64) -> Self {
        Self {
            reconnect,
            base: None,
            need_key: true,
            expected_seq: None,
            early_audio: VecDeque::new(),
        }
    }

    /// Whether anything has been sent: the first keyframe set the base.
    pub(crate) fn started(&self) -> bool {
        self.base.is_some()
    }

    /// Hands `send` each entry to send now, in order, with its base.
    pub(crate) fn take(&mut self, e: Entry<S>, mut send: impl FnMut(&Entry<S>, gst::ClockTime)) {
        if self.expected_seq.is_some_and(|s| s != e.seq) {
            self.need_key = true;
        }
        self.expected_seq = Some(e.seq + 1);
        if e.video && self.need_key {
            if !e.key || e.seq < self.reconnect {
                return;
            }
            self.need_key = false;
            if self.base.is_none() {
                let base = e.pts;
                self.base = Some(base);
                send(&e, base);
                for a in self.early_audio.drain(..) {
                    if a.pts >= base {
                        send(&a, base);
                    }
                }
                return;
            }
        }
        let Some(base) = self.base else {
            if !e.video {
                if self.early_audio.len() >= ENCODED_RING_MAX_ENTRIES {
                    self.early_audio.pop_front();
                }
                self.early_audio.push_back(e);
            }
            return;
        };
        if e.pts >= base {
            send(&e, base);
        }
    }
}

/// `RingState::video_since_key` after one more video entry: a keyframe is frame 0 of its GOP, any
/// other entry one more, and before any keyframe there is nothing to count from.
fn since_key_after(since: Option<u64>, key: bool) -> Option<u64> {
    if key {
        Some(0)
    } else {
        since.map(|n| n + 1)
    }
}

/// F-040: how many frames on the GOP's own keyframe is due when the encoder has made it but not yet
/// handed it out, which then serves any swap's keyframe request and makes asking for one unsafe;
/// `None` when none is in the encoder's delay. A frame delay (`X264_DELAY_FRAMES`) separates the two
/// ends of GstVideoEncoder's floor: it notes its last keyframe when x264 hands it out and checks a
/// request when a frame goes in, so a keyframe still inside x264 is invisible to the throttle, and a
/// request matched in that window cuts a GOP of the frames between. The keyframe is predicted, not
/// seen, and in FRAMES: x264 (`key-int-max` = `gop_frames`, scene cut off) makes the next one that
/// many frames after the last, counting the frames it receives, so a frame dropped before it (the
/// leaky queue, an appsrc) moves the keyframe later in time and must not move this answer. A forced
/// keyframe resets x264's count and arrives as a keyframe in the ring, which resets `video_since_key`
/// the same way (a forced one in the delay is throttled against by the encoder itself, input to
/// input). In the delay means due after the newest frame the ring has taken and no more than the
/// delay's frames on: earlier it would have been taken, later it is a frame the encoder has not
/// received, which a request made now precedes, and resets. The window is the most the delay can be,
/// so a wrong guess errs towards waiting for a keyframe at most `X264_DELAY_MS` away. `gop_frames`
/// 0 (not set) or no keyframe taken: nothing to predict from.
pub(crate) fn own_key_in_flight(video_since_key: Option<u64>, gop_frames: u32) -> Option<u64> {
    let due_in = u64::from(gop_frames).checked_sub(video_since_key?)?;
    (due_in > 0 && due_in <= u64::from(X264_DELAY_FRAMES)).then_some(due_in)
}

/// C7: the line `ask_key` writes, once, when it asks for no keyframe because the floor is not under
/// the GOP (the programme tests wait on it).
pub(crate) const FLOOR_OVER_GOP_LINE: &str =
    "the keyframe is not asked: a forced keyframe cannot come before the GOP's own";

/// C7: whether a keyframe asked for can come before the GOP's own: only when the floor it is held to
/// is shorter than the GOP, both in frames (x264 counts frames; `OutputProfile::
/// forced_key_can_beat_the_gop` is the same comparison on the declared intervals). Held to the floor
/// after the last keyframe, a request made when the floor is the GOP or longer is made at or after the
/// GOP's own keyframe's frame, which x264 has already coded and not yet handed out
/// (`own_key_in_flight`), so GstVideoEncoder's throttle cannot see it and the request cuts a GOP of
/// the few frames between: under Cloudflare's 2 s. A GOP or a floor not yet set (0) asks as before.
pub(crate) fn forced_key_can_beat_the_gop(gop_frames: u32, floor_frames: u32) -> bool {
    gop_frames == 0 || floor_frames == 0 || floor_frames < gop_frames
}

/// Owed 112: whether a sink that can send only now starts from the encoder's first keyframe rather
/// than dropping what the ring holds (Owed 84). It does when the ring still holds every video frame
/// the encoders have pushed (`encoded_video`: none was dropped, none taken by an output), the oldest
/// is a keyframe (x264's first frame is one), and the newest is at most `FIRST_GOP_CATCH_UP_MAX_MS`
/// after it. Returns that backlog, the newest's running time less the keyframe's; `None` to drop.
/// Never with no video: that is F-036 fact 8's path, where the first frame starts the stream anyway.
pub(crate) fn first_gop_backlog<S>(
    entries: &VecDeque<Entry<S>>,
    encoded_video: u64,
) -> Option<gst::ClockTime> {
    let mut video = entries.iter().filter(|e| e.video);
    let first = video.next()?;
    let (held, newest) = video.fold((1u64, first.pts), |(n, newest), e| (n + 1, newest.max(e.pts)));
    let backlog = newest.saturating_sub(first.pts);
    let bound = gst::ClockTime::from_mseconds(FIRST_GOP_CATCH_UP_MAX_MS);
    (held == encoded_video && first.key && backlog <= bound).then_some(backlog)
}

/// Owed 112 (the C3b review's m2): the first-GOP entries go to the output at once, past
/// `backed_up`'s check, so they are taken only when they fit what that check lets an output hold.
/// Returns the larger track's bytes (`size` of each sample, summed per track) when it reaches
/// `OUTPUT_QUEUE_BYTES`, as `backed_up` counts it; `None` when they fit.
pub(crate) fn first_gop_over_queue<S>(entries: &VecDeque<Entry<S>>, size: impl Fn(&S) -> u64) -> Option<u64> {
    let (mut video, mut audio) = (0u64, 0u64);
    for e in entries {
        let track = if e.video { &mut video } else { &mut audio };
        *track += size(&e.sample);
    }
    let most = video.max(audio);
    (most >= OUTPUT_QUEUE_BYTES).then_some(most)
}

/// A sample's bytes (`first_gop_over_queue`).
fn sample_bytes(sample: &gst::Sample) -> u64 {
    sample.buffer().map_or(0, |b| b.size() as u64)
}

/// `sample` with its PTS and DTS moved back by `base` (a shallow copy: the memory is shared).
fn rebased(sample: &gst::Sample, base: gst::ClockTime) -> Option<gst::Sample> {
    let mut buffer = sample.buffer_owned()?;
    {
        let b = buffer.make_mut();
        let pts = b.pts().map(|p| p.saturating_sub(base));
        let dts = b.dts().map(|d| d.saturating_sub(base));
        b.set_pts(pts);
        b.set_dts(dts);
    }
    let mut builder = gst::Sample::builder().buffer(&buffer);
    let caps = sample.caps_owned();
    if let Some(caps) = &caps {
        builder = builder.caps(caps);
    }
    Some(builder.build())
}

/// The output's bus and its progress poll.
struct Watch {
    gen: u64,
    /// This sink's serial in `Stats` (`Stats::on_new_sink`).
    stats_sink: u64,
    /// Progress events not delivered since the last one that was.
    dropped_streak: u64,
    bus: gst::Bus,
    sink: gst::Element,
    file: Option<PathBuf>,
    shared: Shared,
    retired: Arc<AtomicBool>,
}

impl Watch {
    fn run(mut self) {
        let poll = Duration::from_millis(OUTPUT_PROGRESS_POLL_MS);
        let mut next_poll = Instant::now() + poll;
        let mut failed = false;
        loop {
            if self.retired.load(Ordering::SeqCst) {
                return;
            }
            let wait = next_poll.saturating_duration_since(Instant::now()).min(BUS_WAIT);
            let msg = self.bus.timed_pop_filtered(
                gst::ClockTime::from_nseconds(wait.as_nanos() as u64),
                &[
                    gst::MessageType::Error,
                    gst::MessageType::Warning,
                    gst::MessageType::Eos,
                ],
            );
            if let Some(msg) = msg {
                if self.retired.load(Ordering::SeqCst) {
                    return;
                }
                match msg.view() {
                    gst::MessageView::Error(e) => {
                        self.shared.log(&format!(
                            "output gen={} error: {} ({})",
                            self.gen,
                            e.error(),
                            e.debug().as_deref().unwrap_or("")
                        ));
                        if !failed {
                            failed = true;
                            // Nothing more reaches a failed sink, so the ring is no longer its: a
                            // drain now ends when the encoders do, whether it started before the
                            // failure or after it, with the dead sink still in place (PR #28 review
                            // item 3; the domain swaps it only after its back-off, and never in
                            // the hold).
                            self.shared.ring.release(self.gen);
                            self.shared.send(Event::OutputError { gen: self.gen });
                            if self.shared.ring.take_drained_without_output() {
                                self.shared.send(Event::OutputDrained);
                            }
                        }
                    }
                    gst::MessageView::Warning(w) => self.shared.log(&format!(
                        "output gen={} warning: {} ({})",
                        self.gen,
                        w.error(),
                        w.debug().as_deref().unwrap_or("")
                    )),
                    gst::MessageView::Eos(_) => {
                        // One final sample, so the last snapshot equals what the sink wrote; a sink
                        // that failed is not sampled again (its EOS follows the error).
                        if !failed {
                            self.progress();
                        }
                        if self.shared.ring.take_drained_at_eos() {
                            self.shared.send(Event::OutputDrained);
                        }
                    }
                    _ => {}
                }
            }
            if Instant::now() >= next_poll {
                next_poll += poll;
                if next_poll < Instant::now() {
                    next_poll = Instant::now() + poll;
                }
                if !failed {
                    self.progress();
                }
            }
        }
    }

    /// One reading into `Stats` and one `OutputProgress` (the sink's total and the encoders' at the
    /// same moment), never waited for: a full channel drops it (the next poll carries newer totals),
    /// counts it, and says so once when the channel fills and once, with the count, when an event is
    /// delivered again.
    fn progress(&mut self) {
        let total = self.total();
        self.shared.stats.on_sink_total(self.stats_sink, total);
        let e = Inbound::Event(Event::OutputProgress {
            gen: self.gen,
            total_bytes: total,
            encoded: self.shared.ring.encoded(),
        });
        if self.shared.tx.try_send(e).is_err() {
            self.shared.progress_dropped.fetch_add(1, Ordering::Relaxed);
            if self.dropped_streak == 0 {
                self.shared.log(&format!(
                    "output gen={}: the inbound channel is full; progress events are dropped until it drains",
                    self.gen
                ));
            }
            self.dropped_streak += 1;
        } else if self.dropped_streak > 0 {
            self.shared.log(&format!(
                "output gen={}: {} progress events were dropped while the inbound channel was full",
                self.gen, self.dropped_streak
            ));
            self.dropped_streak = 0;
        }
    }

    /// The sink's bytes-sent total: rtmp2sink `stats`'s `out-bytes-total` (F-010), or the file's
    /// size on disk.
    fn total(&self) -> u64 {
        match &self.file {
            Some(path) => std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
            None => rtmp_total(&self.sink),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Owed 84's rule, sample-free: an entry is its seq (the sample) with its kind, key flag and PTS.
    fn entry(seq: u64, video: bool, key: bool, pts_ms: u64) -> Entry<u64> {
        let mut e = Entry::new(seq, video, key, gst::ClockTime::from_mseconds(pts_ms));
        e.seq = seq;
        e
    }

    /// Every entry `take` sends for `entries`, as (seq, base ms), in order.
    fn sent(reconnect: u64, entries: Vec<Entry<u64>>) -> Vec<(u64, u64)> {
        let mut pick = Picker::new(reconnect);
        let mut out = Vec::new();
        for e in entries {
            pick.take(e, |e, base| out.push((e.sample, base.mseconds())));
        }
        out
    }

    /// A ring as the encoders fill it (30 fps video, 48 kHz AAC whose encoder runs about 60 ms ahead),
    /// holding a natural keyframe (seq 0) when a sink can send at seq 6. The control run (a reconnect
    /// point of 0) starts at that keyframe, so it is the reconnect point alone that moves the start.
    fn ring_with_an_old_keyframe() -> Vec<Entry<u64>> {
        vec![
            entry(0, true, true, 0),
            entry(1, false, false, 0),
            entry(2, false, false, 21),
            entry(3, true, false, 33),
            entry(4, false, false, 43),
            entry(5, true, false, 67),
            // The reconnect point: seq 6 is the first entry encoded after the sink could send.
            entry(6, false, false, 64),
            entry(7, true, false, 100),
            entry(8, false, false, 85),
            entry(9, false, false, 107),
            entry(10, false, false, 128),
            entry(11, false, false, 149),
            // The keyframe the reconnect asked for.
            entry(12, true, true, 133),
            entry(13, false, false, 171),
            entry(14, true, false, 167),
        ]
    }

    #[test]
    fn a_keyframe_queued_before_the_reconnect_point_never_starts_the_stream() {
        // Control: with the reconnect point at the ring's start, its old keyframe starts the stream.
        let all = sent(0, ring_with_an_old_keyframe());
        assert_eq!(all.first(), Some(&(0, 0)), "{all:?}");
        // From seq 6: the first entry sent is the keyframe asked for (seq 12, PTS 133 ms the base),
        // then the held audio at or after it (149 ms; 64 to 128 ms are older), then the rest.
        assert_eq!(
            sent(6, ring_with_an_old_keyframe()),
            vec![(12, 133), (11, 133), (13, 133), (14, 133)]
        );
    }

    #[test]
    fn with_no_keyframe_after_the_reconnect_point_nothing_is_sent() {
        let ring: Vec<_> = ring_with_an_old_keyframe()
            .into_iter()
            .filter(|e| e.seq != 12)
            .collect();
        assert_eq!(sent(6, ring), vec![]);
        // An empty ring sends nothing, whatever the point.
        assert_eq!(sent(0, vec![]), vec![]);
    }

    // `started` is what the feeder's drain reads (PR #57 review I-1): false while every entry was
    // refused or held, true from the first one sent, each read checked against what `take` sent.
    #[test]
    fn started_reads_whether_anything_was_sent() {
        let mut checked = 0;
        for reconnect in [0, 6, 13] {
            let mut pick = Picker::new(reconnect);
            assert!(!pick.started(), "{reconnect}: before any entry");
            let mut sent_any = false;
            for e in ring_with_an_old_keyframe() {
                pick.take(e, |_, _| sent_any = true);
                assert_eq!(pick.started(), sent_any, "{reconnect}");
                checked += 1;
            }
            // 13 is past the ring's last keyframe (12): nothing is ever sent.
            assert_eq!(sent_any, reconnect <= 12, "{reconnect}");
        }
        assert_eq!(checked, 3 * ring_with_an_old_keyframe().len());
    }

    /// The encoder's first `n` frames as the ring holds them: video at 30 fps (ADR 0005), stamped
    /// `i × 10⁹ / fps` ns as the programme stamps its ticks (F-036 fact 4), the first a keyframe
    /// (x264's first frame), each after an AAC frame of its own.
    fn first_frames(n: u64) -> VecDeque<Entry<u64>> {
        let fps = u64::from(domain::config::OutputProfile::P720.fps);
        let mut out = VecDeque::new();
        for i in 0..n {
            let pts = gst::ClockTime::from_nseconds(i * 1_000_000_000 / fps);
            let mut a = Entry::new(2 * i, false, false, pts);
            a.seq = 2 * i;
            let mut v = Entry::new(2 * i + 1, true, i == 0, pts);
            v.seq = 2 * i + 1;
            out.extend([a, v]);
        }
        out
    }

    /// Owed 112: a sink that can send only after the encoder's first frames starts from the first
    /// keyframe when the ring still holds every video frame the encoder made and the newest is at
    /// most `FIRST_GOP_CATCH_UP_MAX_MS` after it; a frame more, and the backlog is dropped (Owed 84).
    /// The bound in frames comes from `config` and the profile, not from the rule: 30 at 30 fps.
    /// Each start it takes is run through the `Picker` from the ring's oldest entry, as the feeder
    /// does, and must start at the encoder's first keyframe, its PTS the base.
    #[test]
    fn the_encoders_first_frames_within_the_bound_start_the_stream_from_its_first_keyframe() {
        let fps = u64::from(domain::config::OutputProfile::P720.fps);
        let bound = domain::config::FIRST_GOP_CATCH_UP_MAX_MS * fps / 1_000;
        assert_eq!(
            bound, 30,
            "setup: the bound in frames at 30 fps (one VBV, `config`)"
        );
        let (mut taken, mut dropped) = (0, 0);
        for n in 1..=bound + 3 {
            let ring = first_frames(n);
            let got = first_gop_backlog(&ring, n);
            if n - 1 <= bound {
                let newest = gst::ClockTime::from_nseconds((n - 1) * 1_000_000_000 / fps);
                assert_eq!(got, Some(newest), "{n} frames: within the bound");
                let front = ring.front().expect("non-empty").seq;
                let mut pick = Picker::new(front);
                let mut out = Vec::new();
                for e in ring {
                    pick.take(e, |e, base| out.push((e.sample, e.video, base)));
                }
                assert_eq!(
                    out.first(),
                    Some(&(1, true, gst::ClockTime::ZERO)),
                    "{n} frames: the stream starts at the first keyframe"
                );
                assert_eq!(
                    out.iter().filter(|o| o.1).count() as u64,
                    n,
                    "{n} frames: every video frame is sent"
                );
                taken += 1;
            } else {
                assert_eq!(got, None, "{n} frames: past the bound");
                dropped += 1;
            }
        }
        eprintln!("first-GOP backlogs taken {taken}, dropped {dropped}");
        assert_eq!((taken, dropped), (bound + 1, 2));
    }

    /// F-040: the GOP's own keyframe is in the encoder's delay exactly while the newest frame the ring
    /// holds is one of the `X264_DELAY_FRAMES` before it (frames `gop - delay ..= gop - 1` of a GOP
    /// whose keyframe was frame 0), in both profiles. The expectation is counted in frames from
    /// `config`'s GOP and delay, not read from the rule's own arithmetic; the sweep checks every frame
    /// of two GOPs, reports how many were in flight (a rule that never says yes is a failure, as is
    /// one that always does), and the answer is the frames left to the keyframe.
    #[test]
    fn the_gops_own_keyframe_is_in_flight_for_exactly_the_delay_before_it() {
        let (mut checked, mut in_flight) = (0u64, 0u64);
        for profile in [
            domain::config::OutputProfile::P720,
            domain::config::OutputProfile::P1080,
        ] {
            let gop = profile.gop_frames();
            let delay = u64::from(domain::config::X264_DELAY_FRAMES);
            let mut yes = Vec::new();
            for n in 0..2 * u64::from(gop) {
                let got = own_key_in_flight(Some(n), gop);
                let want = n < u64::from(gop) && n + delay >= u64::from(gop);
                assert_eq!(got.is_some(), want, "{}p, newest frame {n}", profile.height);
                if let Some(due_in) = got {
                    assert_eq!(
                        due_in,
                        u64::from(gop) - n,
                        "{}p, newest frame {n}",
                        profile.height
                    );
                    yes.push(n);
                }
                checked += 1;
            }
            assert_eq!(yes.len() as u64, delay, "{}p: {yes:?}", profile.height);
            in_flight += yes.len() as u64;
        }
        eprintln!("keyframe-in-flight sweep: checked {checked}, in flight {in_flight}");
        assert!(checked > 0 && in_flight > 0 && in_flight < checked);
    }

    /// C7: a keyframe is asked for exactly when it would be made BEFORE the GOP's own. The expected
    /// answer is GstVideoEncoder's own model, not this rule's arithmetic: a request made when the
    /// newest frame the ring holds is `since` frames after the last keyframe reaches the encoder
    /// `delay` frames on (x264's look-ahead and frame threads), is held to the floor after the last
    /// keyframe, and so is made at frame `max(since + delay, floor)` after it; the GOP's own comes at
    /// frame `gop`, and a request made at or after it cuts a GOP of the frames between (under
    /// Cloudflare's floor). The sweep covers the shipped 2 s GOP (never asked: the floor is over it),
    /// the 3 s GOP before C7 (asked until the GOP's keyframe is in the delay), and a 1 s GOP; it
    /// reports how many of each answer it saw, so a rule that is always yes or always no fails.
    #[test]
    fn a_keyframe_is_asked_only_when_it_would_be_made_before_the_gops_own() {
        let floor = u64::from(domain::config::OutputProfile::P1080.forced_keyframe_min_frames());
        let delay = u64::from(domain::config::X264_DELAY_FRAMES);
        let (mut checked, mut asked, mut refused) = (0u64, 0u64, 0u64);
        let mut asked_at_the_shipped_gop = 0u64;
        let shipped = domain::config::OutputProfile::P1080.gop_frames();
        for gop in [shipped, 90, 30] {
            let gop = u32::try_from(gop).expect("frames");
            for since in 0..u64::from(gop) {
                let lands_at = (since + delay).max(floor);
                let want = lands_at < u64::from(gop);
                let got = forced_key_can_beat_the_gop(gop, floor as u32)
                    && own_key_in_flight(Some(since), gop).is_none();
                assert_eq!(
                    got, want,
                    "GOP {gop}, newest frame {since} after the last keyframe"
                );
                if got {
                    asked += 1;
                    if gop == shipped {
                        asked_at_the_shipped_gop += 1;
                    }
                } else {
                    refused += 1;
                }
                checked += 1;
            }
        }
        eprintln!("ask sweep: checked {checked}, asked {asked}, refused {refused}");
        assert!(checked > 0 && asked > 0 && refused > 0);
        // The shipped GOP: the floor is over it, so none is ever asked (the owner's 2 s, C7).
        assert_eq!(asked_at_the_shipped_gop, 0);
        assert_eq!(u64::from(shipped), 60, "setup: the shipped GOP is 2 s at 30 fps");
    }

    /// C7: the comparison itself, at its edge and with nothing set. Equal is refused: a request held
    /// to a floor equal to the GOP is made on the GOP's own keyframe's frame or after it.
    #[test]
    fn the_floor_must_be_strictly_under_the_gop_and_unset_values_ask_as_before() {
        for (gop, floor, want) in [
            (90, 66, true),
            (67, 66, true),
            (66, 66, false),
            (65, 66, false),
            (60, 66, false),
            (0, 66, true),
            (60, 0, true),
        ] {
            assert_eq!(
                forced_key_can_beat_the_gop(gop, floor),
                want,
                "GOP {gop}, floor {floor}"
            );
        }
        for p in [
            domain::config::OutputProfile::P720,
            domain::config::OutputProfile::P1080,
        ] {
            assert_eq!(
                forced_key_can_beat_the_gop(p.gop_frames(), p.forced_keyframe_min_frames()),
                p.forced_key_can_beat_the_gop(),
                "{}p: the frames rule and the declared intervals agree",
                p.height
            );
        }
    }

    /// F-040: no keyframe taken yet, or a GOP the programme has not set, is nothing to predict from:
    /// asked as before (the feeder's `None` branch covers an empty ring).
    #[test]
    fn nothing_is_predicted_before_a_keyframe_is_taken_or_a_gop_is_set() {
        let gop = domain::config::OutputProfile::P720.gop_frames();
        assert_eq!(own_key_in_flight(None, gop), None);
        assert_eq!(own_key_in_flight(Some(0), 0), None);
        assert_eq!(own_key_in_flight(Some(5), 0), None);
        // Control: the same count with a GOP set is in flight, so the refusals above are the missing
        // input and nothing else.
        assert!(own_key_in_flight(Some(u64::from(gop) - 1), gop).is_some());
        assert_eq!(since_key_after(None, false), None);
        assert_eq!(since_key_after(None, true), Some(0));
        assert_eq!(since_key_after(Some(7), false), Some(8));
        assert_eq!(since_key_after(Some(7), true), Some(0));
    }

    /// F-040 (the review's I3): x264 places its next keyframe a GOP of FRAMES after the last, so a
    /// frame dropped before the encoder (the leaky queue, an appsrc: `EncoderOverrun`) moves it later
    /// in PTS and must not move the answer. A GOP in which the encoder lost `lost` frames before x264
    /// delivers the keyframe at PTS `gop + lost` frames; the ring is read at every frame it takes and
    /// the answer must be the same set of delivered frames for every `lost`. The control is the
    /// PTS rule this replaced (due = last key + the GOP in time, in flight while due is after the
    /// newest and within the delay of it): on a GOP that lost frames it lets a request through while
    /// the key is in the delay, so the cases below separate the two rules and are not vacuous.
    #[test]
    fn frames_lost_before_the_encoder_do_not_move_the_keyframes_prediction() {
        let (mut checked, mut old_rule_wrong) = (0u64, 0u64);
        for profile in [
            domain::config::OutputProfile::P720,
            domain::config::OutputProfile::P1080,
        ] {
            let fps = u64::from(profile.fps);
            let gop = profile.gop_frames();
            let delay = u64::from(domain::config::X264_DELAY_FRAMES);
            let frame_ns = 1_000_000_000 / fps;
            for lost in [0u64, 1, delay / 2, delay, 3 * delay] {
                let key = gst::ClockTime::from_nseconds(0);
                let mut since = since_key_after(None, true);
                assert_eq!(since, Some(0));
                for delivered in 1..u64::from(gop) {
                    since = since_key_after(since, false);
                    // The frames the encoder lost shift the PTS of what it delivers after them: they
                    // were dropped inside this GOP (at its start), so frame `delivered` is stamped
                    // `delivered + lost` frames after the key.
                    let newest = gst::ClockTime::from_nseconds((delivered + lost) * frame_ns);
                    let want = delivered + delay >= u64::from(gop);
                    assert_eq!(
                        own_key_in_flight(since, gop).is_some(),
                        want,
                        "{}p, {lost} lost, delivered {delivered}",
                        profile.height
                    );
                    // The rule this replaced.
                    let due = key + gst::ClockTime::from_nseconds(u64::from(gop) * frame_ns);
                    let latest = newest + gst::ClockTime::from_nseconds(delay * frame_ns);
                    let by_time = due > newest && due <= latest;
                    if by_time != want {
                        old_rule_wrong += 1;
                        assert!(
                            lost > 0,
                            "{}p: the time rule is right with nothing lost",
                            profile.height
                        );
                    }
                    checked += 1;
                }
            }
        }
        eprintln!("lost-frames sweep: checked {checked}, the time rule wrong on {old_rule_wrong}");
        assert!(checked > 0);
        assert!(old_rule_wrong > 0, "no case separates the rules");
    }

    /// Owed 112's rule is the encoder's FIRST frames only: no video (fact 8's path), a video frame the
    /// ring dropped or an output took (so the oldest held is not the encoder's first), or an oldest
    /// held frame that is not a keyframe each leave Owed 84's drop in place.
    #[test]
    fn a_ring_without_every_first_frame_or_its_keyframe_is_not_taken() {
        let mut checked = 0;
        // Empty, and audio alone: no video to start from.
        assert_eq!(first_gop_backlog(&VecDeque::<Entry<u64>>::new(), 0), None);
        let audio: VecDeque<_> = first_frames(3).into_iter().filter(|e| !e.video).collect();
        assert_eq!(first_gop_backlog(&audio, 0), None);
        checked += 2;
        for n in [1, 2, 3, 20] {
            // The encoders pushed one video frame more than the ring holds: the first was dropped or
            // taken, whether or not the oldest held is a keyframe.
            assert_eq!(first_gop_backlog(&first_frames(n), n + 1), None, "{n}: one gone");
            let mut gone = first_frames(n + 1);
            gone.retain(|e| !(e.video && e.key));
            assert_eq!(first_gop_backlog(&gone, n + 1), None, "{n}: the keyframe gone");
            // Every frame held, but the oldest is no keyframe.
            let mut delta = first_frames(n);
            for e in delta.iter_mut() {
                e.key = false;
            }
            assert_eq!(first_gop_backlog(&delta, n), None, "{n}: no keyframe");
            checked += 3;
        }
        eprintln!("refusals checked {checked}");
        assert_eq!(checked, 14);
    }

    /// Owed 112 (the C3b review's m2): first-GOP entries go out past `backed_up`, so they are taken
    /// only under `OUTPUT_QUEUE_BYTES` a track, counted as `backed_up` counts it (each track on its
    /// own, refused AT the cap). The samples here are seqs: odd ones video, even ones audio
    /// (`first_frames`), so a size per kind.
    #[test]
    fn first_gop_entries_at_the_output_queue_are_not_taken() {
        let cap = OUTPUT_QUEUE_BYTES;
        let sized = |v: u64, a: u64| move |s: &u64| if s % 2 == 1 { v } else { a };
        let mut checked = 0;
        // (frames, video bytes each, audio bytes each, the larger track if refused)
        for (n, v, a, want) in [
            (1, cap - 1, 0, None),
            (1, cap, 0, Some(cap)),
            (1, 0, cap, Some(cap)),
            // Each track under the cap, their sum over it: not backed up.
            (1, cap / 2 + 1, cap / 2 + 1, None),
            // Spread over the bound's frames: 30 at 30 fps.
            (30, cap / 30, 1, None),
            (30, cap / 30 + 1, 1, Some(30 * (cap / 30 + 1))),
            (0, cap, cap, None),
        ] {
            let got = first_gop_over_queue(&first_frames(n), sized(v, a));
            assert_eq!(got, want, "{n} frames of {v} B video, {a} B audio");
            checked += 1;
        }
        eprintln!("queue sizes checked {checked}");
        assert_eq!(checked, 7);
    }

    #[test]
    fn a_gap_after_the_start_resumes_the_video_at_a_keyframe_and_keeps_the_audio() {
        let mut ring = ring_with_an_old_keyframe();
        ring.extend([
            // The ring dropped seq 15 to 19, so 20 (a delta) follows a gap and is not sent.
            entry(20, true, false, 300),
            entry(21, false, false, 299),
            entry(22, true, true, 333),
            entry(23, true, false, 367),
        ]);
        assert_eq!(sent(6, ring)[4..].to_vec(), vec![(21, 133), (22, 133), (23, 133)]);
    }

    /// A deterministic generator (an LCG, so a failure replays).
    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 33) % n
        }
    }

    /// One generated ring: what the encoders push (30 fps video with a keyframe every `gop` frames or
    /// at random, AAC frames of 1 024 samples at 48 kHz), each encoder late by up to 60 ms of its own,
    /// so the audio arrives ahead of the video or behind it; with runs of entries this output never
    /// took (the ring dropped them), and a reconnect point anywhere.
    fn generated(rng: &mut Lcg) -> (u64, Vec<Entry<u64>>) {
        let gop = 1 + rng.below(60);
        let (video_late, audio_late) = (rng.below(61), rng.below(61));
        let frames = 1 + rng.below(240);
        let mut timed: Vec<(u64, bool, bool)> = Vec::new();
        for i in 0..frames {
            let key = i % gop == 0 || rng.below(40) == 0;
            timed.push((i * 1_000 / 30, true, key));
        }
        let mut a = 0u64;
        while a * 1_024 * 1_000 / 48_000 < frames * 1_000 / 30 {
            timed.push((a * 1_024 * 1_000 / 48_000, false, false));
            a += 1;
        }
        let arrival =
            |&(pts, video, _): &(u64, bool, bool)| pts + if video { video_late } else { audio_late };
        timed.sort_by_key(arrival);
        let mut out = Vec::new();
        let mut seq = 0u64;
        for (pts, video, key) in timed {
            // About one entry in 50 starts a dropped run of 1 to 30.
            if rng.below(50) == 0 {
                seq += 1 + rng.below(30);
            }
            out.push(entry(seq, video, key, pts));
            seq += 1;
        }
        let reconnect = rng.below(seq + 2);
        (reconnect, out)
    }

    /// Owed 84 and the A/V rule over 4 000 generated rings: the first entry sent is a video keyframe
    /// at or past the reconnect point and its PTS is the base; no video from before that point is
    /// sent; nothing older than the base is sent; no audio goes before the first video; the video
    /// after a dropped run restarts at a keyframe; each kind's PTS never goes back.
    #[test]
    fn a_generated_sweep_never_sends_a_frame_from_before_the_reconnect_point() {
        let mut rng = Lcg(84);
        let (mut cases, mut started, mut old_key_refused, mut gaps_resumed, mut audio_held) = (0, 0, 0, 0, 0);
        let mut late_audio_refused = 0;
        for _ in 0..4_000 {
            let (reconnect, ring) = generated(&mut rng);
            let old_key = ring.iter().any(|e| e.video && e.key && e.seq < reconnect);
            let mut pick = Picker::new(reconnect);
            let mut out: Vec<(Entry<u64>, u64)> = Vec::new();
            let mut last_taken: Option<u64> = None;
            let mut gap_since_video = false;
            for e in ring {
                if last_taken.is_some_and(|s| s + 1 != e.seq) {
                    gap_since_video = true;
                }
                last_taken = Some(e.seq);
                let before = out.len();
                let late = before > 0 && !e.video && e.pts.mseconds() < out[0].1;
                pick.take(e, |e, base| out.push((e.clone(), base.mseconds())));
                if late && out.len() == before {
                    late_audio_refused += 1;
                }
                if before == 0 && out.len() > 1 {
                    audio_held += 1;
                }
                for (e, _) in &out[before..] {
                    if e.video {
                        if gap_since_video && before > 0 {
                            assert!(
                                e.key,
                                "case {}: video seq {} after a dropped run is not a keyframe",
                                cases + 1,
                                e.seq
                            );
                            gaps_resumed += 1;
                        }
                        gap_since_video = false;
                    }
                }
            }
            cases += 1;
            let Some((first, base)) = out.first() else {
                continue;
            };
            started += 1;
            assert!(
                first.video && first.key,
                "case {cases}: the first entry sent is not a keyframe"
            );
            assert!(
                first.seq >= reconnect,
                "case {cases}: it was queued before the reconnect point"
            );
            assert_eq!(
                first.pts.mseconds(),
                *base,
                "case {cases}: its PTS is not the base"
            );
            if old_key {
                old_key_refused += 1;
            }
            let (mut last_v, mut last_a) = (None, None);
            for (e, b) in &out {
                assert_eq!(b, base, "case {cases}: one base for the whole output");
                assert!(
                    e.pts.mseconds() >= *base,
                    "case {cases}: seq {} is older than the base",
                    e.seq
                );
                assert!(
                    !e.video || e.seq >= reconnect,
                    "case {cases}: video seq {} predates the reconnect",
                    e.seq
                );
                let last = if e.video { &mut last_v } else { &mut last_a };
                assert!(
                    last.is_none_or(|p| e.pts >= p),
                    "case {cases}: seq {} goes back",
                    e.seq
                );
                *last = Some(e.pts);
            }
        }
        eprintln!(
            "cases {cases}, started {started}, with an old keyframe refused {old_key_refused}, \
             video resumed after a gap {gaps_resumed}, held audio sent {audio_held}, \
             late audio refused {late_audio_refused}"
        );
        assert_eq!(cases, 4_000);
        for (what, n) in [
            ("started", started),
            ("an old keyframe refused", old_key_refused),
            ("resumed after a gap", gaps_resumed),
            ("held audio sent with the keyframe", audio_held),
            (
                "audio older than the base arriving after it, refused",
                late_audio_refused,
            ),
        ] {
            assert!(n >= 30, "the sweep reached {what} only {n} times");
        }
    }

    /// The ring as `push` fills it: the entry goes on the back, then the bounds drop what they must.
    /// Returns what was dropped, (video, audio).
    fn pushed(ring: &mut VecDeque<Entry<u64>>, e: Entry<u64>) -> (u64, u64) {
        ring.push_back(e);
        prune(ring)
    }

    /// An entry stamped `pts_ns`, its seq and sample the order it is made in.
    fn stamped(seq: u64, video: bool, pts_ns: u64) -> Entry<u64> {
        let mut e = Entry::new(seq, video, false, gst::ClockTime::from_nseconds(pts_ns));
        e.seq = seq;
        e
    }

    /// One AAC frame's span in ns: 1 024 samples at 48 kHz.
    const AAC_NS: u64 = 1_024 * 1_000_000_000 / 48_000;

    /// The oldest and newest PTS the ring holds on one track, `None` for an empty track.
    fn track_pts(ring: &VecDeque<Entry<u64>>, video: bool) -> Option<(gst::ClockTime, gst::ClockTime)> {
        let mut pts = ring.iter().filter(|e| e.video == video).map(|e| e.pts);
        let first = pts.next()?;
        Some(pts.fold((first, first), |(lo, hi), p| (lo.min(p), hi.max(p))))
    }

    /// F-040 (the C5 check run 45750515672, a 2 s encoder stall): the stalled encoder's in-flight
    /// frames come out late, behind audio that never stalled. They sit in the ring a span and the
    /// delay behind the newest audio, and none may be dropped for it: one dropped picture made the
    /// output wait for the encoder's next keyframe, up to a GOP away, before it sent any. The audio
    /// track stays bounded by the same span, so the check reads both tracks.
    #[test]
    fn pictures_a_stalled_encoder_delivers_late_are_not_dropped_for_the_audio_ahead_of_them() {
        let span_ns = ENCODED_RING_SPAN.as_nanos() as u64;
        let delay_ns = domain::config::X264_DELAY_MS * 1_000_000;
        let (mut checked, mut audio_dropped) = (0u64, 0u64);
        for profile in [
            domain::config::OutputProfile::P720,
            domain::config::OutputProfile::P1080,
        ] {
            let fps = u64::from(profile.fps);
            let late = u64::from(domain::config::X264_DELAY_FRAMES);
            let mut ring = VecDeque::new();
            let mut video_dropped = 0;
            let mut seq = 0;
            for i in 0..late {
                let (v, a) = pushed(&mut ring, stamped(seq, true, i * 1_000_000_000 / fps));
                (video_dropped, audio_dropped) = (video_dropped + v, audio_dropped + a);
                seq += 1;
            }
            // The audio that kept running: to a span and the delay past the newest picture.
            let newest_picture = (late - 1) * 1_000_000_000 / fps;
            let audio_to = newest_picture + span_ns + delay_ns;
            assert!(
                audio_to > span_ns,
                "{}p: the audio is not past the first picture by a span, so nothing is witnessed",
                profile.height
            );
            let mut t = 0;
            while t <= audio_to {
                let (v, a) = pushed(&mut ring, stamped(seq, false, t));
                (video_dropped, audio_dropped) = (video_dropped + v, audio_dropped + a);
                seq += 1;
                t += AAC_NS;
            }
            let kept = ring.iter().filter(|e| e.video).count() as u64;
            assert_eq!(video_dropped, 0, "{}p: pictures dropped", profile.height);
            assert_eq!(kept, late, "{}p: pictures kept", profile.height);
            let (lo, hi) = track_pts(&ring, false).expect("audio is held");
            assert!(
                hi.saturating_sub(lo) <= span(),
                "{}p: the audio spans {lo}..{hi}",
                profile.height
            );
            checked += seq;
        }
        assert!(audio_dropped > 0, "the audio track was never bounded");
        assert!(checked > 0, "nothing was pushed");
    }

    /// Each track is bounded by the span behind its own newest entry, keeping everything inside it,
    /// and the ring by its count when PTS never moves. The expected counts are the span's frames at
    /// each track's own rate, not read from the rule.
    #[test]
    fn each_track_keeps_exactly_a_span_and_the_ring_its_count() {
        let span_ns = ENCODED_RING_SPAN.as_nanos() as u64;
        let mut checked = 0;
        for profile in [
            domain::config::OutputProfile::P720,
            domain::config::OutputProfile::P1080,
        ] {
            let fps = u64::from(profile.fps);
            // Video alone, then audio alone, each for three spans.
            for video in [true, false] {
                let step = if video { 1_000_000_000 / fps } else { AAC_NS };
                let mut ring = VecDeque::new();
                let mut dropped = 0;
                let n = 3 * span_ns / step;
                for i in 0..n {
                    let (v, a) = pushed(&mut ring, stamped(i, video, i * step));
                    dropped += v + a;
                }
                let kept = ring.len() as u64;
                let (lo, hi) = track_pts(&ring, video).expect("the track is held");
                assert!(
                    hi.saturating_sub(lo) <= span(),
                    "{}p video {video}",
                    profile.height
                );
                // Everything within a span of the newest stays: a span holds `span / step` whole
                // intervals, one entry more than that.
                assert_eq!(kept, span_ns / step + 1, "{}p video {video}", profile.height);
                assert_eq!(kept + dropped, n, "{}p video {video}", profile.height);
                checked += n;
            }
        }
        // The edge itself: an entry exactly a span behind its track's newest stays, a nanosecond
        // more and it goes, on each track.
        for video in [true, false] {
            let mut ring = VecDeque::new();
            assert_eq!(pushed(&mut ring, stamped(0, video, 0)), (0, 0));
            assert_eq!(pushed(&mut ring, stamped(1, video, span_ns)), (0, 0));
            assert_eq!(ring.len(), 2, "video {video}: a span behind");
            let gone = pushed(&mut ring, stamped(2, video, span_ns + 1));
            assert_eq!(gone, if video { (1, 0) } else { (0, 1) }, "video {video}");
            assert_eq!(ring.len(), 2, "video {video}: a span and a ns behind");
            checked += 3;
        }
        // The count bound, at one PTS so that no span applies.
        let mut ring = VecDeque::new();
        let (mut dropped, extra) = (0, 5);
        for i in 0..ENCODED_RING_MAX_ENTRIES as u64 + extra {
            let (v, a) = pushed(&mut ring, stamped(i, i % 2 == 0, 0));
            dropped += v + a;
            checked += 1;
        }
        assert_eq!(ring.len(), ENCODED_RING_MAX_ENTRIES);
        assert_eq!(dropped, extra);
        assert_eq!(ring.front().map(|e| e.seq), Some(extra), "the oldest went");
        assert!(checked > 0, "nothing was pushed");
    }
}
