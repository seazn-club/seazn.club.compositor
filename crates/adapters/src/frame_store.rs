//! The slots the four pipelines share (C09; spec §3.3's refinement). The input pipeline (C10)
//! writes the camera's playout queue and its audio ring, the graphics pipeline (C11) writes the
//! overlay slot, and the programme pipeline (C09) reads them once per tick. Every slot is bounded:
//! the overlay slot holds one sample (the newest replaces it); the camera's frames and audio wait in
//! the playout (C4, `playout.rs`: a queue played on a clock slaved to the camera's PTS) of at most
//! `PLAYOUT_CAP_MS`, dropping the oldest, counted (the plan's Global Constraints, "audio 400 ms …
//! leaky, dropping the oldest", amended by C4 to the playout's cap). The playout also counts the
//! frames written, and the frames and audio a tick passed over (`Stats::on_camera_tick`, Owed 81).
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use domain::config::PLAYOUT_CAP_MS;
use gstreamer as gst;

use crate::playout::{AudioTake, Playout, Tick};

/// The audio the programme encodes: S16LE, 48 kHz, stereo, interleaved (C10's capsfilter; the F-004
/// verdict's AAC-LC 128 kbps 48 kHz stereo is encoded from it).
pub const AUDIO_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: u32 = 2;
/// One S16LE stereo frame: two channels of two bytes.
pub const AUDIO_FRAME_BYTES: usize = 4;
/// The ring's cap: the playout's, `PLAYOUT_CAP_MS` (C4; the plan's "audio 400 ms", amended so the
/// audio holds what the camera's queue may) of 48 kHz stereo S16LE: 0.5 × 48 000 × 4 = 96 000 bytes.
pub const AUDIO_RING_BYTES: usize = PLAYOUT_CAP_MS as usize * AUDIO_RATE as usize / 1_000 * AUDIO_FRAME_BYTES;

/// Shared by every pipeline; a clone shares the same slots.
#[derive(Clone, Default)]
pub struct FrameStore {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    /// The camera's frames and audio, under one lock: a tick's picture and its audio are read off
    /// the same clock.
    camera: Mutex<Playout<gst::Sample>>,
    overlay: Mutex<Option<gst::Sample>>,
    /// Audio pushes refused because they were not whole S16LE stereo frames.
    audio_refused: AtomicU64,
    /// The ring's real bytes handed out by `take_audio*` (the padding is not counted).
    audio_taken: AtomicU64,
}

/// A sample's media time (ns): its buffer's PTS as running time in its time segment (the time base
/// the camera's video and audio appsinks share, one input build's pipeline); a sample with no time
/// segment (the programme's tests) reads its raw PTS. `None` without a PTS, and for a PTS outside its
/// segment (it has no running time): the raw PTS there is another time base, a jump of hours on a
/// live chain (the C4 review's m3), so such a sample plays as unstamped.
pub fn media_time(s: &gst::Sample) -> Option<u64> {
    let pts = s.buffer()?.pts()?;
    match s.segment().and_then(|seg| seg.downcast_ref::<gst::ClockTime>()) {
        Some(seg) => seg.to_running_time(pts).map(gst::ClockTime::nseconds),
        None => Some(pts.nseconds()),
    }
}

/// A slot's lock, taken even if a writer panicked while holding it: the slot holds plain data, so
/// the value is still whole, and a panicked pipeline thread must not take the others down with it.
fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FrameStore {
    /// An unstamped camera frame (the programme's tests): due at once, the newest wins (the old
    /// slot's rule, `playout.rs`).
    pub fn set_camera(&self, s: gst::Sample) {
        locked(&self.inner.camera).push_video(None, None, s);
    }

    /// A decoded camera frame of input generation `gen`, queued on its media time (`media_time`).
    pub fn push_camera(&self, gen: u64, s: gst::Sample) {
        let pts = media_time(&s);
        locked(&self.inner.camera).push_video(Some(gen), pts, s);
    }

    /// The newest camera frame written, aired or not.
    pub fn camera(&self) -> Option<gst::Sample> {
        locked(&self.inner.camera).newest().cloned()
    }

    /// The newest camera frame written and the frames written so far, read under one lock.
    pub fn camera_frame(&self) -> (Option<gst::Sample>, u64) {
        let p = locked(&self.inner.camera);
        (p.newest().cloned(), p.written())
    }

    /// The camera frames written so far, every generation's.
    pub fn camera_written(&self) -> u64 {
        locked(&self.inner.camera).written()
    }

    /// The programme's tick at `at_ns` of its own timeline: the camera frame on air after it, and
    /// the playout's counts (`playout::Playout::tick`). Every tick calls it, a slate's too.
    pub fn playout_tick(&self, at_ns: u64) -> Tick<gst::Sample> {
        locked(&self.inner.camera).tick(at_ns)
    }

    pub fn set_overlay(&self, s: gst::Sample) {
        *locked(&self.inner.overlay) = Some(s);
    }

    pub fn overlay(&self) -> Option<gst::Sample> {
        locked(&self.inner.overlay).clone()
    }

    /// Empties the overlay slot (`hide_overlay`, C05's graphics Failed). It sets nothing else: the
    /// next `set_overlay` puts a frame on air again (Owed 17: no hide is sticky, because no
    /// `show_overlay` port exists to lift one).
    pub fn clear_overlay(&self) {
        *locked(&self.inner.overlay) = None;
    }

    /// Appends unstamped `bytes` (whole S16LE stereo frames), played first in, first out, and drops
    /// the oldest bytes past `AUDIO_RING_BYTES`. A push that is not a whole number of frames is
    /// refused and counted: taking it would shift every later sample by a byte, and the rest of the
    /// session would encode as noise.
    pub fn push_audio(&self, bytes: &[u8]) {
        if self.whole(bytes) {
            locked(&self.inner.camera).push_audio(None, None, bytes);
        }
    }

    /// `push_audio` for input generation `gen`, stamped `pts` ns of media time (`media_time`), so
    /// the programme plays it on the camera's clock (`take_audio_due`).
    pub fn push_audio_at(&self, gen: u64, pts: Option<u64>, bytes: &[u8]) {
        if self.whole(bytes) {
            locked(&self.inner.camera).push_audio(Some(gen), pts, bytes);
        }
    }

    fn whole(&self, bytes: &[u8]) -> bool {
        let whole = bytes.len().is_multiple_of(AUDIO_FRAME_BYTES);
        if !whole {
            self.inner.audio_refused.fetch_add(1, Ordering::Relaxed);
        }
        whole
    }

    /// Exactly `n` bytes: the oldest `min(n, level)` bytes of the ring, after `n − level` bytes of
    /// digital silence when the ring holds less than `n`. The programme never encodes these zeros
    /// (F-004): it calls `take_audio_due` with its noise filler.
    pub fn take_audio(&self, n: usize) -> Vec<u8> {
        self.take_audio_padded(n, |pad| pad.fill(0))
    }

    /// `take_audio` with the padding written by `pad`. The padding comes FIRST and the ring's bytes
    /// last: the bytes kept are the newest of the timeline, so they stay at its end.
    pub fn take_audio_padded(&self, n: usize, pad: impl FnOnce(&mut [u8])) -> Vec<u8> {
        let mut p = locked(&self.inner.camera);
        let take = p.take_audio_fifo(n, pad);
        // Counted under the ring's lock, so a `clear_audio` that returns has seen every take before it.
        self.inner
            .audio_taken
            .fetch_add(take.real as u64, Ordering::Relaxed);
        take.bytes
    }

    /// One period of `n` bytes for the programme's tick at `at_ns`: the camera's audio its clock has
    /// due then, `pad` writing the filler wherever the ring has none (`playout::Playout::take_audio_due`;
    /// unstamped audio plays first in, first out, as `take_audio_padded`).
    pub fn take_audio_due(&self, at_ns: u64, n: usize, pad: impl FnMut(&mut [u8])) -> AudioTake {
        let mut p = locked(&self.inner.camera);
        let take = p.take_audio_due(at_ns, n, pad);
        self.inner
            .audio_taken
            .fetch_add(take.real as u64, Ordering::Relaxed);
        take
    }

    /// Empties the ring and returns the bytes it dropped. The input calls it when a build stops
    /// passing (its abandon, its loss, its rebuild), with that build's gate shut, so none of the
    /// build's audio still in the ring plays after it (the PR #35 review's m1).
    pub fn clear_audio(&self) -> usize {
        locked(&self.inner.camera).clear_audio()
    }

    /// The ring's real bytes handed out by `take_audio*` so far; the padding is not counted.
    pub fn audio_taken(&self) -> u64 {
        self.inner.audio_taken.load(Ordering::Relaxed)
    }

    /// A build's loss or retire (`gst_input`'s gate): drops the camera's queued frames (counted
    /// unaired; the frame on air stays, for the domain's freeze) and empties the ring, under one
    /// lock, so nothing the build pushed airs after it (the C4 review's m5). The ring's bytes dropped.
    pub fn flush_camera(&self) -> usize {
        let mut p = locked(&self.inner.camera);
        p.clear_video();
        p.clear_audio()
    }

    /// The camera frames queued, not yet aired or dropped.
    pub fn camera_queued(&self) -> usize {
        locked(&self.inner.camera).queued()
    }

    /// The media time (ns) just past the ring's newest stamped audio, on the video's stamps
    /// (`playout::Playout::audio_end_ns`); `None` when it holds none.
    pub fn audio_end_ns(&self) -> Option<i128> {
        locked(&self.inner.camera).audio_end_ns()
    }

    /// The bytes the ring holds now.
    pub fn audio_level(&self) -> usize {
        locked(&self.inner.camera).audio_level()
    }

    /// Audio pushes refused so far (not whole frames).
    pub fn audio_refused(&self) -> u64 {
        self.inner.audio_refused.load(Ordering::Relaxed)
    }
}

impl fmt::Debug for FrameStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FrameStore")
            .field("camera", &locked(&self.inner.camera).newest().is_some())
            .field("overlay", &locked(&self.inner.overlay).is_some())
            .field("audio_bytes", &self.audio_level())
            .finish()
    }
}
