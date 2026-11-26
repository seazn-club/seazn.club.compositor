//! C09: the programme pipeline (never rebuilt) and the swappable output pipeline, against real
//! GStreamer. Every test writes to a `file://` target in its own temp dir and reads the result back
//! through `filesrc ! flvdemux ! …` (`decode`). The tests share GStreamer and the machine's CPU, so
//! each takes `serial()` first: they run one at a time whatever the harness's thread count, and the
//! real-pace timing tests never compete with another test's encoder.
//!
//! Expected values come from the brief, `Thresholds::default()`, `OutputProfile` and the findings
//! (F-004, F-005), never from the adapter.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use adapters::frame_store::{media_time, FrameStore, AUDIO_RING_BYTES};
use adapters::gst_output::{
    ENCODED_RING_MAX_ENTRIES, ENCODED_RING_SPAN, OUTPUT_PROGRESS_POLL_MS, OUTPUT_QUEUE_BYTES,
};
use adapters::gst_programme::{GstProgramme, OVERLAY_SIZE};
use adapters::metrics_proc::ProcMetrics;
#[path = "support/rtmp.rs"]
mod rtmp;
use adapters::slate::{logo_box, Slate, LOGO_PNG, SLATE_BACKDROP_RGB};
use adapters::stats::StatsSnapshot;
use app::ports::{Inbound, Metrics, ProgrammeOutput, TargetSpec};
use crossbeam_channel::Receiver;
use domain::config::{
    OutputProfile, Thresholds, CF_KEYFRAME_INTERVAL_MAX_MS, CF_KEYFRAME_INTERVAL_MIN_MS,
    ENCODER_SPEED_REALTIME_MIN, FIRST_GOP_CATCH_UP_MAX_MS, FORCED_KEYFRAME_MIN_INTERVAL_MS,
    OUTPUT_FPS_MIN_WINDOW_MS, OUTPUT_SINK_TIMEOUT_MS, PLAYOUT_CAP_MS, PLAYOUT_TARGET_MS, PROGRAMME_GOP_MS,
};
use domain::ladder::Rung;
use domain::output_link::{Encoded, OutputLink, OutputState, SwapReason};
use domain::programme::{FrameChoice, SlateKind};
use domain::supervisor::Event;
use domain::time::Ms;
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;
use rtmp::FakeRtmp;
use sha2::{Digest, Sha256};

const P720: OutputProfile = OutputProfile::P720;
/// The house backdrop, from the rulebook rather than the adapter: seazn.club's `--mk-night`,
/// `#150b36` (`apps/web/src/app/globals.css:535`; the overlay templates spec §4.2).
const HOUSE_NIGHT: [u8; 3] = [0x15, 0x0b, 0x36];
/// The slate on air, decoded from the programme, against `HOUSE_NIGHT`: measured 1 level off it
/// ([22, 11, 54]; x264 codes the flat backdrop), so 2 leaves one level of slack. Left to
/// negotiation the compositor mixes in RGB and the backdrop comes out [18, 7, 51] (F-010), which 2
/// tells apart and the looser 8 did not (mutant G24).
const ON_AIR_TOL: u8 = 2;
/// The brief's pin of the vendored logo (C09 amendment 2026-10-15).
const LOGO_SHA256: &str = "bee2648f76fa2590ac81d2c1e14050fe4c62687b8630437b0d6e6b080c2f90e8";
/// Owed 15, option (a): poll the bytes-sent total at most every 250 ms (F-011's choice; since Owed
/// 87 it no longer bounds a stall's detection, which is rtmp2sink's `timeout`).
const OWED_15_POLL_MAX_MS: u64 = 250;
/// F-004: AAC at 48 kHz carries 1 024 samples a frame, 46.875 frames/s, and the verified encode is
/// 128 kbps.
const AAC_FRAMES_PER_S: f64 = 48_000.0 / 1_024.0;
const AAC_KBPS: f64 = 128.0;
/// The brief's A/V tolerance (PDF rank 4): ±40 ms.
const AV_TOLERANCE_MS: u64 = 40;
/// One tick of S16LE 48 kHz stereo at 30 fps: 1 600 frames of 4 bytes.
const TICK_AUDIO_BYTES: usize = 48_000 / 30 * 4;

// ---------------------------------------------------------------------------------------------
// Harness

fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A temp dir that removes itself.
struct Tmp(PathBuf);

impl Tmp {
    fn new(name: &str) -> Tmp {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("c09-{}-{name}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        Tmp(dir)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Tmp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn file_target(path: &Path) -> TargetSpec {
    TargetSpec {
        url: format!("file://{}", path.display()),
        stream_key: String::new(),
    }
}

type Lines = Arc<Mutex<Vec<String>>>;

/// A started 720p programme, its inbound receiver, its frame store and its captured log.
fn programme_with(profile: &OutputProfile) -> (GstProgramme, Receiver<Inbound>, FrameStore, Lines) {
    gst::init().expect("gstreamer");
    let frames = FrameStore::default();
    let (tx, rx) = crossbeam_channel::bounded(8_192);
    let lines: Lines = Arc::default();
    let sink = Arc::clone(&lines);
    let mut p = GstProgramme::new(frames.clone(), tx).with_log(Arc::new(move |l: &str| {
        sink.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(l.to_owned());
    }));
    p.start(profile).expect("start");
    (p, rx, frames, lines)
}

fn programme() -> (GstProgramme, Receiver<Inbound>, FrameStore, Lines) {
    programme_with(&P720)
}

fn period(profile: &OutputProfile) -> Duration {
    Duration::from_nanos(1_000_000_000 / u64::from(profile.fps))
}

/// Pushes `range` at the profile's real frame rate, as C08's loop does, calling `before(i)` first.
fn pace_with(
    p: &mut GstProgramme,
    range: std::ops::Range<u64>,
    mut choice: impl FnMut(u64) -> FrameChoice,
    mut before: impl FnMut(u64),
) {
    let step = period(&P720);
    let t0 = Instant::now();
    for (k, i) in range.enumerate() {
        let due = t0 + step * k as u32;
        if let Some(wait) = due.checked_duration_since(Instant::now()) {
            thread::sleep(wait);
        }
        before(i);
        p.push(i, choice(i));
    }
}

fn pace(p: &mut GstProgramme, range: std::ops::Range<u64>, choice: FrameChoice) {
    pace_with(p, range, |_| choice, |_| {});
}

/// Every event until `pred` matches one (included), or panics after `timeout`.
fn events_until(rx: &Receiver<Inbound>, timeout: Duration, pred: impl Fn(&Event) -> bool) -> Vec<Event> {
    let end = Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        let left = end.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Inbound::Event(e)) => {
                let hit = pred(&e);
                seen.push(e);
                if hit {
                    return seen;
                }
            }
            Ok(Inbound::Session(_)) => {}
            Err(_) => panic!("no matching event within {timeout:?}; saw {seen:?}"),
        }
    }
}

/// Every event that arrives within `window`.
fn events_for(rx: &Receiver<Inbound>, window: Duration) -> Vec<Event> {
    let end = Instant::now() + window;
    let mut seen = Vec::new();
    while let Ok(m) = rx.recv_timeout(end.saturating_duration_since(Instant::now())) {
        if let Inbound::Event(e) = m {
            seen.push(e);
        }
    }
    seen
}

fn drain_and_wait(p: &mut GstProgramme, rx: &Receiver<Inbound>) -> Vec<Event> {
    p.drain();
    events_until(rx, Duration::from_secs(15), |e| *e == Event::OutputDrained)
}

// ---------------------------------------------------------------------------------------------
// Pictures

type Rgb = [u8; 3];

fn close(a: Rgb, b: Rgb, tol: u8) -> bool {
    a.iter().zip(b).all(|(x, y)| x.abs_diff(y) <= tol)
}

fn is_blue(p: Rgb) -> bool {
    p[2] > 150 && p[0] < 90 && p[1] < 90
}

fn is_red(p: Rgb) -> bool {
    p[0] > 150 && p[1] < 90 && p[2] < 90
}

fn is_green(p: Rgb) -> bool {
    p[1] > 150 && p[0] < 90 && p[2] < 90
}

/// A `w` × `h` sample of `format`, every pixel `rgba`, converted from RGBA by GStreamer.
fn solid(format: gst_video::VideoFormat, w: u32, h: u32, rgba: [u8; 4]) -> gst::Sample {
    picture(format, w, h, |_, _| rgba)
}

/// A `w` × `h` sample of `format` drawn by `px(x, y) -> rgba`.
fn picture(format: gst_video::VideoFormat, w: u32, h: u32, px: impl Fn(u32, u32) -> [u8; 4]) -> gst::Sample {
    let rgba = gst_video::VideoInfo::builder(gst_video::VideoFormat::Rgba, w, h)
        .build()
        .expect("rgba info");
    let stride = rgba.stride()[0] as usize;
    let mut data = vec![0u8; rgba.size()];
    for y in 0..h {
        for x in 0..w {
            let o = y as usize * stride + x as usize * 4;
            data[o..o + 4].copy_from_slice(&px(x, y));
        }
    }
    let src = gst::Buffer::from_mut_slice(data);
    let out_info = gst_video::VideoInfo::builder(format, w, h)
        .build()
        .expect("out info");
    let buffer = if format == gst_video::VideoFormat::Rgba {
        src
    } else {
        let conv = gst_video::VideoConverter::new(&rgba, &out_info, None).expect("converter");
        let in_frame = gst_video::VideoFrame::from_buffer_readable(src, &rgba).expect("in frame");
        let out = gst::Buffer::with_size(out_info.size()).expect("out buffer");
        let mut out_frame = gst_video::VideoFrame::from_buffer_writable(out, &out_info).expect("out frame");
        conv.frame(&in_frame, &mut out_frame);
        out_frame.into_buffer()
    };
    gst::Sample::builder()
        .buffer(&buffer)
        .caps(&out_info.to_caps().expect("caps"))
        .build()
}

/// CEF's overlay: BGRA 1920 × 1080, opaque blue in the top-left 100 × 100, transparent elsewhere.
fn overlay_blue_corner() -> gst::Sample {
    let (w, h) = OVERLAY_SIZE;
    picture(gst_video::VideoFormat::Bgra, w, h, |x, y| {
        if x < 100 && y < 100 {
            [0, 0, 255, 255]
        } else {
            [0, 0, 0, 0]
        }
    })
}

/// BT.709's luma weights (ITU-R BT.709-6, item 3.2), from which the decode matrix is derived.
const BT709_KR: f64 = 0.2126;
const BT709_KB: f64 = 0.0722;

/// An I420 picture, decoded to RGB by the test itself: BT.709 limited range, in floating point and
/// rounded. GStreamer's own 8-bit converter truncates (#150b36 comes back as [16, 7, 49] after one
/// RGB → I420 → RGB trip, F-010), so a reading through it would measure the converter, not the
/// adapter.
struct Pic {
    w: u32,
    h: u32,
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

impl Pic {
    fn of(sample: &gst::Sample) -> Pic {
        let info = gst_video::VideoInfo::from_caps(sample.caps().expect("caps")).expect("info");
        assert_eq!(
            info.format(),
            gst_video::VideoFormat::I420,
            "the test decodes I420 only"
        );
        let c = info.colorimetry();
        assert_eq!(
            (c.matrix(), c.range()),
            (
                gst_video::VideoColorMatrix::Bt709,
                gst_video::VideoColorRange::Range16_235
            ),
            "the test decodes BT.709 limited range; the caps say {c}"
        );
        let (w, h) = (info.width(), info.height());
        let frame =
            gst_video::VideoFrame::from_buffer_readable(sample.buffer_owned().expect("buffer"), &info)
                .expect("frame");
        let plane = |n: u32, pw: u32, ph: u32| {
            let stride = frame.plane_stride()[n as usize] as usize;
            let data = frame.plane_data(n).expect("plane");
            let mut packed = Vec::with_capacity((pw * ph) as usize);
            for row in 0..ph as usize {
                packed.extend_from_slice(&data[row * stride..row * stride + pw as usize]);
            }
            packed
        };
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        Pic {
            w,
            h,
            y: plane(0, w, h),
            u: plane(1, cw, ch),
            v: plane(2, cw, ch),
        }
    }

    fn at(&self, x: u32, y: u32) -> Rgb {
        let c = (y / 2 * self.w.div_ceil(2) + x / 2) as usize;
        let luma = (f64::from(self.y[(y * self.w + x) as usize]) - 16.0) * 255.0 / 219.0;
        let (cb, cr) = (
            (f64::from(self.u[c]) - 128.0) * 255.0 / 224.0,
            (f64::from(self.v[c]) - 128.0) * 255.0 / 224.0,
        );
        let kg = 1.0 - BT709_KR - BT709_KB;
        let r = luma + 2.0 * (1.0 - BT709_KR) * cr;
        let b = luma + 2.0 * (1.0 - BT709_KB) * cb;
        let g =
            luma - 2.0 * BT709_KB * (1.0 - BT709_KB) / kg * cb - 2.0 * BT709_KR * (1.0 - BT709_KR) / kg * cr;
        [r, g, b].map(|v| v.round().clamp(0.0, 255.0) as u8)
    }

    /// The share of the logo box's pixels more than 40 away from the backdrop on any channel.
    fn logo_share(&self) -> f64 {
        let (x0, y0, lw, lh) = logo_box(self.w, self.h);
        let mut off = 0usize;
        for y in y0..y0 + lh {
            for x in x0..x0 + lw {
                if !close(self.at(x, y), HOUSE_NIGHT, 40) {
                    off += 1;
                }
            }
        }
        off as f64 / f64::from(lw * lh)
    }
}

// ---------------------------------------------------------------------------------------------
// Reading a target back

/// One decoded frame: its size and the pixels the tests look at.
#[derive(Debug, Clone)]
struct Frame {
    w: u32,
    h: u32,
    centre: Rgb,
    /// (10, 10) and (W − 10, H − 10): the slate's corners.
    corner_tl: Rgb,
    corner_br: Rgb,
    /// (30, 30): inside the overlay's blue 100 × 100 at 1080, 66 × 66 at 720.
    spot: Rgb,
    /// (20, H / 2): the pillarbox border of a 4:3 camera.
    side: Rgb,
    logo_share: f64,
}

#[derive(Debug, Default)]
struct Decoded {
    video_pts_ms: Vec<u64>,
    keys: Vec<bool>,
    /// What h264parse passed on, before the decoder: (PTS ms, keyframe).
    parsed: Vec<(u64, bool)>,
    frames: Vec<Frame>,
    /// The raw AAC payload (flvdemux's access units) and their count.
    aac: Vec<u8>,
    aac_frames: usize,
    audio_pts_ms: Vec<u64>,
    pcm: Vec<i16>,
}

impl Decoded {
    fn first_is_key(&self) -> bool {
        self.keys.first().copied().unwrap_or(false)
    }

    fn audio_ms(&self) -> u64 {
        (self.aac_frames as u64 * 1_024 * 1_000) / 48_000
    }

    fn zero_share(&self) -> f64 {
        self.aac.iter().filter(|&&b| b == 0).count() as f64 / self.aac.len().max(1) as f64
    }

    fn aac_kbps(&self) -> f64 {
        self.aac.len() as f64 * 8.0 / 1_000.0 / (self.audio_ms().max(1) as f64 / 1_000.0)
    }

    /// RMS in dBFS of the decoded stereo PCM between `from_ms` and `to_ms`.
    fn rms_dbfs(&self, from_ms: u64, to_ms: u64) -> f64 {
        let a = (from_ms * 48 * 2) as usize;
        let b = ((to_ms * 48 * 2) as usize).min(self.pcm.len());
        let s = &self.pcm[a.min(b)..b];
        let rms = (s.iter().map(|&v| f64::from(v).powi(2)).sum::<f64>() / s.len().max(1) as f64).sqrt();
        20.0 * (rms.max(1e-9) / f64::from(i16::MAX)).log10()
    }
}

fn decode(path: &Path) -> Decoded {
    let launch = format!(
        "filesrc location={} ! flvdemux name=d \
         d.video ! queue ! h264parse name=hp ! avdec_h264 ! video/x-raw,format=I420 ! appsink name=v sync=false \
         d.audio ! queue ! identity name=araw ! avdec_aac ! audioconvert ! audioresample ! \
         audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved ! appsink name=a sync=false",
        path.display()
    );
    let pipeline = gst::parse::launch(&launch)
        .expect("decode pipeline")
        .downcast::<gst::Pipeline>()
        .expect("pipeline");
    let out = Arc::new(Mutex::new(Decoded::default()));

    let hp = pipeline.by_name("hp").expect("hp");
    let o = Arc::clone(&out);
    hp.static_pad("src")
        .expect("hp src")
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(b) = info.buffer() {
                let key = !b.flags().contains(gst::BufferFlags::DELTA_UNIT);
                let mut d = o.lock().unwrap();
                d.keys.push(key);
                d.parsed.push((b.pts().map_or(u64::MAX, |p| p.mseconds()), key));
            }
            gst::PadProbeReturn::Ok
        });
    let araw = pipeline.by_name("araw").expect("araw");
    let o = Arc::clone(&out);
    araw.static_pad("src")
        .expect("araw src")
        .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(b) = info.buffer() {
                if let Ok(m) = b.map_readable() {
                    let mut d = o.lock().unwrap();
                    d.aac.extend_from_slice(&m);
                    d.aac_frames += 1;
                    d.audio_pts_ms.push(b.pts().map_or(u64::MAX, |p| p.mseconds()));
                }
            }
            gst::PadProbeReturn::Ok
        });
    let v = pipeline
        .by_name("v")
        .expect("v")
        .downcast::<gst_app::AppSink>()
        .expect("appsink");
    let o = Arc::clone(&out);
    v.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                let pic = Pic::of(&sample);
                let (w, h) = (pic.w, pic.h);
                let pts = sample.buffer().and_then(|b| b.pts()).map_or(0, |p| p.mseconds());
                let f = Frame {
                    w,
                    h,
                    centre: pic.at(w / 2, h / 2),
                    corner_tl: pic.at(10, 10),
                    corner_br: pic.at(w - 10, h - 10),
                    spot: pic.at(30, 30),
                    side: pic.at(20, h / 2),
                    logo_share: pic.logo_share(),
                };
                let mut d = o.lock().unwrap();
                d.video_pts_ms.push(pts);
                d.frames.push(f);
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    let a = pipeline
        .by_name("a")
        .expect("a")
        .downcast::<gst_app::AppSink>()
        .expect("appsink");
    let o = Arc::clone(&out);
    a.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if let Some(b) = sample.buffer() {
                    if let Ok(m) = b.map_readable() {
                        let mut d = o.lock().unwrap();
                        d.pcm
                            .extend(m.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])));
                    }
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    pipeline.set_state(gst::State::Playing).expect("decode playing");
    let bus = pipeline.bus().expect("bus");
    let msg = bus.timed_pop_filtered(
        gst::ClockTime::from_seconds(30),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    let _ = pipeline.set_state(gst::State::Null);
    match msg.as_ref().map(|m| m.view()) {
        Some(gst::MessageView::Eos(_)) => {}
        other => panic!("decoding {} did not finish: {other:?}", path.display()),
    }
    drop(pipeline);
    let d = std::mem::take(&mut *out.lock().unwrap());
    d
}

// ---------------------------------------------------------------------------------------------
// The brief's tests (Step 3, amended)

#[test]
fn ninety_slate_ticks_give_ninety_frames_at_constant_spacing() {
    let _s = serial();
    let dir = Tmp::new("ninety");
    let (mut p, rx, _f, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..90, FrameChoice::Slate(SlateKind::Waiting));
    drain_and_wait(&mut p, &rx);
    let d = decode(&dir.file("a.flv"));
    assert_eq!(d.frames.len(), 90, "frames");
    let expected = (1_000.0 / f64::from(P720.fps)).round() as u64;
    let deltas: Vec<u64> = d.video_pts_ms.windows(2).map(|w| w[1] - w[0]).collect();
    eprintln!("pts deltas checked: {}", deltas.len());
    assert_eq!(deltas.len(), 89);
    for (i, &dt) in deltas.iter().enumerate() {
        assert!(
            dt.abs_diff(expected) <= 1,
            "delta {i}: {dt} ms, want {expected} ± 1"
        );
    }
    assert!(
        d.audio_ms().abs_diff(3_000) <= AV_TOLERANCE_MS,
        "audio {} ms against 3 000 ± {AV_TOLERANCE_MS}",
        d.audio_ms()
    );
    // The track starts with the picture: the first AAC frame is the keyframe's, within one frame
    // (1 024 samples, 21 ms) of it.
    let first_audio = *d.audio_pts_ms.first().expect("audio");
    eprintln!("first video pts 0 ms, first audio pts {first_audio} ms");
    assert!(
        first_audio <= 1_024 * 1_000 / 48_000,
        "audio starts {first_audio} ms after the picture"
    );
}

// ---------------------------------------------------------------------------------------------
// F-036: a fixed GOP, every keyframe interval inside Cloudflare's 2 to 10 s

/// Ticks between hard cuts. x264's default scene cut may start a GOP at a cut at least `keyint_min`
/// frames after the last keyframe, which x264 sets to `keyint / 10` (9 of 90) when nothing else does,
/// so a cut every 10 ticks is one it could take every time.
const CUT_TICKS: u64 = 10;

/// The GOP in ticks (`PROGRAMME_GOP_MS` in the 720p profile's frames).
fn gop_ticks() -> u64 {
    u64::from(P720.gop_frames())
}

/// Which of the three pictures tick `i` shows: the flat house night (0), then textured picture 1,
/// then textured picture 2, each for `CUT_TICKS`, round and round.
fn cut_picture(i: u64) -> usize {
    ((i / CUT_TICKS) % 3) as usize
}

/// A `profile`-sized picture with detail in it, unlike a flat backdrop: 32-pixel squares,
/// each a colour hashed from its place and `seed`, so a cut to or from it is a hard one.
fn textured(profile: &OutputProfile, seed: u32) -> gst::Sample {
    picture(
        gst_video::VideoFormat::I420,
        profile.width,
        profile.height,
        |x, y| {
            let mut h = (x / 32).wrapping_mul(0x9E37_79B9) ^ (y / 32).wrapping_mul(0x85EB_CA6B) ^ seed;
            h ^= h >> 15;
            h = h.wrapping_mul(0x2C1B_3C6D);
            h ^= h >> 12;
            let [r, g, b, _] = h.to_le_bytes();
            [r, g, b, 255]
        },
    )
}

/// The frame indexes h264parse flagged as keyframes.
fn key_indexes(d: &Decoded) -> Vec<u64> {
    (0u64..)
        .zip(&d.parsed)
        .filter(|(_, f)| f.1)
        .map(|(i, _)| i)
        .collect()
}

/// The control: the keyframes x264 codes for `n` ticks of `pics` (in `cut_picture`'s order) with its
/// scene cut at the default and the programme's other x264 settings, written here from C09's brief
/// and F-036 (`zerolatency`, `medium`, no B-frames, the GOP). It is the premise that these cuts are
/// hard enough for x264's scene cut to start a GOP at them, so a programme that codes none of them
/// as keyframes has its scene cut off rather than cuts too soft to tell.
fn default_scene_cut_keys(profile: &OutputProfile, pics: &[gst::Sample; 3], n: u64) -> Vec<u64> {
    let launch = format!(
        "appsrc name=src format=time block=true ! videoconvert ! x264enc tune=zerolatency \
         speed-preset=medium bframes=0 key-int-max={} ! h264parse ! appsink name=sink sync=false",
        profile.gop_frames()
    );
    let pipeline = gst::parse::launch(&launch)
        .expect("control pipeline")
        .downcast::<gst::Pipeline>()
        .expect("pipeline");
    let src = pipeline
        .by_name("src")
        .expect("src")
        .downcast::<gst_app::AppSrc>()
        .expect("appsrc");
    // The programme's frame rate, so x264 paces its rate control as the programme's encoder does.
    let mut caps = pics[0].caps().expect("caps").to_owned();
    let fps = i32::try_from(profile.fps).expect("fps");
    caps.make_mut().set("framerate", gst::Fraction::new(fps, 1));
    src.set_caps(Some(&caps));
    let keys = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&keys);
    let count = Arc::new(AtomicU64::new(0));
    pipeline
        .by_name("sink")
        .expect("sink")
        .downcast::<gst_app::AppSink>()
        .expect("appsink")
        .set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |s| {
                    let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                    let i = count.fetch_add(1, Ordering::SeqCst);
                    if sample
                        .buffer()
                        .is_some_and(|b| !b.flags().contains(gst::BufferFlags::DELTA_UNIT))
                    {
                        seen.lock().unwrap().push(i);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
    pipeline.set_state(gst::State::Playing).expect("control playing");
    let step = 1_000_000_000 / u64::from(profile.fps);
    for i in 0..n {
        let mut b = pics[cut_picture(i)].buffer().expect("a buffer").copy();
        b.get_mut()
            .expect("writable")
            .set_pts(gst::ClockTime::from_nseconds(i * step));
        src.push_buffer(b).expect("control push");
    }
    src.end_of_stream().expect("control eos");
    let msg = pipeline.bus().expect("bus").timed_pop_filtered(
        gst::ClockTime::from_seconds(60),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    );
    let _ = pipeline.set_state(gst::State::Null);
    assert!(
        matches!(msg.as_ref().map(|m| m.view()), Some(gst::MessageView::Eos(_))),
        "control: {msg:?}"
    );
    let k = keys.lock().unwrap().clone();
    k
}

/// F-036: Cloudflare errored the programme input for GOPs under 2 s, and x264's scene cut started
/// one at every hard cut. In both modes, with a hard cut every `CUT_TICKS` (three camera pictures,
/// the flat house night and two textured ones, round and round), the programme's keyframes land on
/// the GOP alone, and
/// each interval in the file is the GOP, inside Cloudflare's range. The sink comes mid-GOP, so the
/// file starts at the keyframe its swap forced (Owed 84), and the GOP restarts there: the encoder's
/// own next keyframe is a whole GOP after a forced one, never sooner. The control codes the same
/// pictures with x264's scene cut on and must find a keyframe at a cut and off the GOP, or the cuts
/// proved nothing. Every tick is a camera tick, so the programme codes exactly the control's
/// pictures (the C1 review's m5: a slate tick has the logo on it, which no control picture has).
#[test]
fn every_keyframe_lands_on_the_gop_and_a_hard_cut_never_starts_one() {
    let _s = serial();
    let mut intervals = 0;
    for profile in [P720, OutputProfile::P1080] {
        let dir = Tmp::new("gop");
        let gop = u64::from(profile.gop_frames());
        // Past the forced keyframe's floor, short of the GOP's second keyframe, and half a cut from
        // the last cut: the keyframe the swap asks for is made at once, on no cut.
        let lead = u64::from(profile.forced_keyframe_min_frames()) + CUT_TICKS / 2;
        assert!(
            lead < gop && !lead.is_multiple_of(CUT_TICKS),
            "setup: the swap at {lead}"
        );
        // Two whole GOPs from the swap and three cuts into the third: keyframes at the file's first
        // frame, the GOP and twice it.
        let n = lead + 2 * gop + 3 * CUT_TICKS;
        let [r, g, b] = HOUSE_NIGHT;
        let pics = [
            solid(
                gst_video::VideoFormat::I420,
                profile.width,
                profile.height,
                [r, g, b, 255],
            ),
            textured(&profile, 1),
            textured(&profile, 2),
        ];
        let (mut p, rx, frames, _l) = programme_with(&profile);
        let choose = |_| FrameChoice::Emit;
        let cut = |i| {
            if i % CUT_TICKS == 0 {
                frames.set_camera(pics[cut_picture(i)].clone());
            }
        };
        pace_with(&mut p, 0..lead, &choose, &cut);
        p.swap_sink(1, &file_target(&dir.file("a.flv")));
        pace_with(&mut p, lead..n, &choose, &cut);
        drain_and_wait(&mut p, &rx);
        let d = decode(&dir.file("a.flv"));
        let keys = key_indexes(&d);
        let control = default_scene_cut_keys(&profile, &pics, n);
        let cuts = (1..n).filter(|i| i % CUT_TICKS == 0).count();
        eprintln!(
            "{}p: {cuts} hard cuts over {} frames; keyframes at {keys:?}; x264's default scene cut: {control:?}",
            profile.height,
            d.parsed.len()
        );
        // A keyframe x264's own scene cut started at a cut, off the GOP. Which cut it takes is x264's
        // to choose: its threshold falls with the frames since the last keyframe, and CI on PR #109
        // took each cut from the flat picture into the first textured one 60 frames after its last
        // keyframe ([0, 70, 130, 190, 250] at 720p), so no textured-to-textured cut came that late.
        // Any cut it keys proves them hard (the C1 review's m5 asked for the textured pair alone, a
        // premise CI found false; m5's concern, a picture the programme does not show, is met above).
        assert!(
            control.iter().any(|k| k % CUT_TICKS == 0 && k % gop != 0),
            "setup: x264's own scene cut took no cut: {control:?}"
        );
        // By frame count, so a frame the leaky queue dropped before x264 moves nothing.
        let want: Vec<u64> = (0..d.parsed.len() as u64).step_by(gop as usize).collect();
        assert!(
            want.len() >= 3,
            "setup: two whole GOPs reached the file: {}",
            d.parsed.len()
        );
        assert_eq!(keys, want, "{}p: the keyframes", profile.height);
        let key_ms: Vec<u64> = d.parsed.iter().filter(|f| f.1).map(|f| f.0).collect();
        for w in key_ms.windows(2) {
            let gap = w[1] - w[0];
            assert!(
                (CF_KEYFRAME_INTERVAL_MIN_MS..=CF_KEYFRAME_INTERVAL_MAX_MS).contains(&gap),
                "{}p: a {gap} ms GOP",
                profile.height
            );
            assert!(gap >= PROGRAMME_GOP_MS, "{}p: a {gap} ms GOP", profile.height);
            intervals += 1;
        }
    }
    eprintln!("GOP intervals checked: {intervals}");
    assert!(intervals >= 4, "intervals checked: {intervals}");
}

#[test]
fn emit_uses_the_latest_camera_frame() {
    let _s = serial();
    let dir = Tmp::new("emit");
    let (mut p, rx, frames, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    frames.set_camera(solid(gst_video::VideoFormat::Bgra, 1_280, 720, [0, 255, 0, 255]));
    pace(&mut p, 0..10, FrameChoice::Emit);
    // The latest frame wins: red replaces green.
    frames.set_camera(solid(gst_video::VideoFormat::I420, 1_280, 720, [255, 0, 0, 255]));
    pace(&mut p, 10..20, FrameChoice::Emit);
    drain_and_wait(&mut p, &rx);
    let d = decode(&dir.file("a.flv"));
    assert_eq!(d.frames.len(), 20);
    assert!(is_green(d.frames[9].centre), "{:?}", d.frames[9].centre);
    assert!(is_red(d.frames[19].centre), "{:?}", d.frames[19].centre);
    assert_eq!(p.camera_missing(), 0);
}

/// Owed 81 (i), the seam: `FrameStore::set_camera` (what C10's input calls for every frame) through
/// `push` into the programme's `Stats`. `writes(i)` frames go into the slot just before tick `i`; a
/// camera tick shows the newest of them, so it loses `writes(i) − 1`, and a slate tick loses none.
/// The script's count, by hand: the live bursts of 3 before ticks 6, 9 and 12 lose 2 each, the
/// burst of 3 before the `Repeat` tick 17 loses 2 (a frozen tick is the camera on air: the domain's
/// freeze can lag the slot while the burst's frame events are still queued), the 2 frames before
/// tick 25 lose 1, and the frames under the waiting and reconnecting slates lose none.
#[test]
fn camera_frames_replaced_counts_the_frames_no_tick_showed() {
    let _s = serial();
    let (mut p, _rx, frames, _l) = programme();
    let writes = |i: u64| -> usize {
        match i {
            0..=4 | 20..=24 => 3, // under a slate
            5 => 1,
            6 | 9 | 12 => 3,
            7 | 8 | 10 | 11 | 13 | 14 => 0,
            15 | 16 | 18 | 19 => 0, // frozen: no frame
            17 => 3,                // frozen, but the slot took a burst
            25 => 2,
            _ => 1,
        }
    };
    let choice = |i: u64| match i {
        0..=4 => FrameChoice::Slate(SlateKind::Waiting),
        15..=19 => FrameChoice::Repeat,
        20..=24 => FrameChoice::Slate(SlateKind::Reconnecting),
        _ => FrameChoice::Emit,
    };
    let replaced = |p: &GstProgramme| p.stats().snapshot().camera_frames_replaced;
    // One picture, written as many frames: the slot counts writes, not pictures.
    let red = solid(gst_video::VideoFormat::I420, 1_280, 720, [255, 0, 0, 255]);
    let mut written = 0;
    let mut phase = |p: &mut GstProgramme, range: std::ops::Range<u64>| {
        pace_with(p, range, choice, |i| {
            for _ in 0..writes(i) {
                frames.set_camera(red.clone());
                written += 1;
            }
        });
    };
    phase(&mut p, 0..5);
    assert_eq!(replaced(&p), None, "the waiting slate alone measures nothing");
    phase(&mut p, 5..15);
    assert_eq!(replaced(&p), Some(6), "three live bursts of 3");
    phase(&mut p, 15..20);
    assert_eq!(
        replaced(&p),
        Some(8),
        "frozen: the burst of 3 before the Repeat tick 17 loses 2"
    );
    phase(&mut p, 20..25);
    assert_eq!(
        replaced(&p),
        Some(8),
        "under the reconnecting slate, nothing counts"
    );
    phase(&mut p, 25..40);
    assert_eq!(
        replaced(&p),
        Some(9),
        "two frames before tick 25, then one a tick"
    );
    eprintln!("frames written to the camera slot: {written}");
    assert_eq!(written, 15 + 1 + 9 + 3 + 15 + 2 + 14, "the script ran in full");
    assert_eq!(p.camera_missing(), 0, "every camera tick had a frame");
}

/// `sample` with its buffer stamped `pts_ms` (no segment: the playout reads the raw PTS).
fn stamped(sample: &gst::Sample, pts_ms: u64) -> gst::Sample {
    let mut buffer = sample.buffer().expect("a buffer").copy();
    buffer
        .get_mut()
        .expect("a fresh copy")
        .set_pts(gst::ClockTime::from_mseconds(pts_ms));
    gst::Sample::builder()
        .buffer(&buffer)
        .caps(&sample.caps().expect("caps").to_owned())
        .build()
}

/// C4, the seam: `FrameStore::push_camera` (what C10's input calls for every frame, stamped) through
/// `push` and the encoder. Two stamped frames written together before tick 0, green then red 100 ms
/// later on the camera's clock, air on the playout's clock: the clock anchors at tick 0 a target
/// behind the newest (red), so green is due at `PLAYOUT_TARGET_MS − 100` and red at the target, and
/// the ticks before green have no camera picture yet (the waiting slate's). The newest-frame slot
/// would have put red on air at tick 0. The ticks after a frame airs with nothing new are repeats.
#[test]
fn stamped_camera_frames_air_on_the_playout_clock() {
    let _s = serial();
    let dir = Tmp::new("playout");
    let (mut p, rx, frames, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    let at_ms = |i: u64| i * 1_000 / u64::from(P720.fps);
    let first_at = |ms: u64| (0..).find(|&i| at_ms(i) >= ms).expect("a tick");
    let (green_tick, red_tick) = (first_at(PLAYOUT_TARGET_MS - 100), first_at(PLAYOUT_TARGET_MS));
    assert!(
        0 < green_tick && green_tick + 1 < red_tick,
        "the premise: {green_tick}, {red_tick}"
    );
    let green = solid(gst_video::VideoFormat::I420, 1_280, 720, [0, 255, 0, 255]);
    let red = solid(gst_video::VideoFormat::I420, 1_280, 720, [255, 0, 0, 255]);
    frames.push_camera(1, stamped(&green, 7_000));
    frames.push_camera(1, stamped(&red, 7_100));
    let ticks = 20;
    pace(&mut p, 0..ticks, FrameChoice::Emit);
    drain_and_wait(&mut p, &rx);
    let d = decode(&dir.file("a.flv"));
    assert_eq!(d.frames.len() as u64, ticks);
    let mut checked = 0;
    for (i, f) in d.frames.iter().enumerate() {
        let i = i as u64;
        if i < green_tick {
            assert!(
                !is_green(f.centre) && !is_red(f.centre),
                "{i}: no camera yet: {:?}",
                f.centre
            );
        } else if i < red_tick {
            assert!(is_green(f.centre), "{i}: green on air: {:?}", f.centre);
        } else {
            assert!(is_red(f.centre), "{i}: red on air: {:?}", f.centre);
        }
        checked += 1;
    }
    eprintln!("playout frames checked: {checked}");
    assert_eq!(
        p.camera_missing(),
        green_tick,
        "the ticks before the first frame aired"
    );
    let snap = p.stats().snapshot();
    assert_eq!(snap.camera_frames_replaced, Some(0), "both frames aired");
    let repeats = (red_tick - green_tick - 1) + (ticks - red_tick - 1);
    assert_eq!(snap.camera_playout.map(|t| t.repeat_ticks), Some(repeats));
}

/// C4 fix round 1 (the review's I2 and m3): `media_time` reads a sample's PTS as running time in
/// its time segment, the time base the input's video and audio appsinks share (a segment starting at
/// 10 s maps a PTS of 10.5 s to 0.5 s; its base adds on), a sample with no segment (the programme's
/// tests) its raw PTS, and a PTS before its segment's start (no running time) as unstamped, never as
/// the raw PTS of another time base. Each expected value is GStreamer's running-time rule
/// (`base + pts − start`) worked by hand.
#[test]
fn media_time_is_the_running_time_in_the_samples_segment() {
    gst::init().expect("gstreamer");
    let buffer = |pts_ms: u64| {
        let mut b = gst::Buffer::new();
        b.get_mut()
            .expect("a new buffer")
            .set_pts(gst::ClockTime::from_mseconds(pts_ms));
        b
    };
    let ms = |m: u64| Some(m * 1_000_000);
    let mut checked = 0;
    for (pts, start, base, want) in [
        (10_500, 10_000, 0, ms(500)),
        (10_500, 10_000, 2_000, ms(2_500)),
        (10_000, 10_000, 0, ms(0)),
        (9_000, 10_000, 0, None),
    ] {
        let mut seg = gst::FormattedSegment::<gst::ClockTime>::new();
        seg.set_start(gst::ClockTime::from_mseconds(start));
        seg.set_base(gst::ClockTime::from_mseconds(base));
        let s = gst::Sample::builder().buffer(&buffer(pts)).segment(&seg).build();
        assert_eq!(
            media_time(&s),
            want,
            "PTS {pts} ms, segment from {start} ms, base {base} ms"
        );
        checked += 1;
    }
    let raw = gst::Sample::builder().buffer(&buffer(7_000)).build();
    assert_eq!(media_time(&raw), ms(7_000), "no segment: the raw PTS");
    let unstamped = gst::Sample::builder().buffer(&gst::Buffer::new()).build();
    assert_eq!(media_time(&unstamped), None, "no PTS");
    eprintln!("segments checked: {checked}");
    assert_eq!(checked, 4);
}

#[test]
fn camera_of_a_different_size_is_letterboxed_not_renegotiated() {
    let _s = serial();
    let dir = Tmp::new("letterbox");
    let (mut p, rx, frames, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    let i420 = gst_video::VideoFormat::I420;
    frames.set_camera(solid(i420, 640, 360, [255, 0, 0, 255]));
    pace(&mut p, 0..10, FrameChoice::Emit);
    frames.set_camera(solid(i420, 1_920, 1_080, [0, 255, 0, 255]));
    pace(&mut p, 10..20, FrameChoice::Repeat);
    // A 4:3 camera (a phone rotated, or a transcode change) is pillarboxed: 960 × 720, centred.
    frames.set_camera(solid(i420, 640, 480, [0, 0, 255, 255]));
    pace(&mut p, 20..30, FrameChoice::Emit);
    drain_and_wait(&mut p, &rx);
    let d = decode(&dir.file("a.flv"));
    assert_eq!(d.frames.len(), 30);
    let checked = d.frames.iter().filter(|f| (f.w, f.h) == (1_280, 720)).count();
    eprintln!("frames at 1280x720: {checked}");
    assert_eq!(checked, 30, "every frame keeps the programme's caps");
    assert!(is_red(d.frames[9].centre));
    assert!(is_green(d.frames[19].centre));
    assert!(is_blue(d.frames[29].centre));
    assert!(
        close(d.frames[29].side, [0, 0, 0], 24),
        "pillarbox border {:?}",
        d.frames[29].side
    );
    assert!(
        close(d.frames[9].side, [255, 0, 0], 90) || is_red(d.frames[9].side),
        "16:9 fills the frame"
    );
    assert_eq!(p.builds(), 1);
}

#[test]
fn overlay_is_composited_on_top_and_hide_clears_it() {
    let _s = serial();
    let dir = Tmp::new("overlay");
    let (mut p, rx, frames, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    frames.set_overlay(overlay_blue_corner());
    pace(&mut p, 0..15, FrameChoice::Slate(SlateKind::Waiting));
    p.hide_overlay();
    pace(&mut p, 15..30, FrameChoice::Slate(SlateKind::Waiting));
    drain_and_wait(&mut p, &rx);
    let d = decode(&dir.file("a.flv"));
    assert_eq!(d.frames.len(), 30);
    assert!(is_blue(d.frames[14].spot), "{:?}", d.frames[14].spot);
    assert!(!is_blue(d.frames[29].spot), "{:?}", d.frames[29].spot);
    assert!(
        close(d.frames[29].spot, HOUSE_NIGHT, ON_AIR_TOL),
        "{:?}",
        d.frames[29].spot
    );
}

#[test]
fn half_rate_rung_halves_overlay_pushes() {
    let _s = serial();
    let (mut p, _rx, _f, _l) = programme();
    p.step_down(Rung::OverlayHalfRate);
    for i in 0..60 {
        p.push(i, FrameChoice::Slate(SlateKind::Waiting));
    }
    assert_eq!(p.overlay_pushes(), 30);
}

#[test]
fn swap_sink_starts_new_file_at_keyframe_without_rebuilding_programme() {
    let _s = serial();
    let dir = Tmp::new("swap");
    let (mut p, rx, _f, lines) = programme();
    // Swapped in before the first frame, so frame 0 is the last keyframe and the second swap, a
    // floor on, is due at once (F-036): b holds a floor of frames, not the GOP's keyframe on.
    swap_before_the_first_frame(&mut p, &lines, 1, &file_target(&dir.file("a.flv")));
    let swap = floor_ticks();
    assert_eq!(key_due(swap, 0), swap, "setup: b's keyframe is due at its swap");
    pace(&mut p, 0..swap, FrameChoice::Slate(SlateKind::Waiting));
    p.swap_sink(2, &file_target(&dir.file("b.flv")));
    let end = 2 * swap;
    assert!(
        end - last_natural_key(end) + OWED84_KEY_WAIT_TICKS < end - swap,
        "setup: b from the GOP's own keyframe is short"
    );
    pace(&mut p, swap..end, FrameChoice::Slate(SlateKind::Waiting));
    drain_and_wait(&mut p, &rx);
    let b = decode(&dir.file("b.flv"));
    eprintln!(
        "swap: b has {} frames for the {} paced from its swap",
        b.frames.len(),
        end - swap
    );
    assert!(
        b.frames.len() as u64 + OWED84_KEY_WAIT_TICKS >= end - swap,
        "b frames {}",
        b.frames.len()
    );
    assert!(b.first_is_key(), "b starts at a keyframe");
    assert_eq!(b.video_pts_ms.first(), Some(&0), "b's PTS is rebased to 0");
    let a = decode(&dir.file("a.flv"));
    assert!(
        a.frames.len() as u64 + OWED84_MARGIN_TICKS >= swap,
        "a frames {}",
        a.frames.len()
    );
    assert!(a.first_is_key());
    assert_eq!(p.builds(), 1);
}

#[test]
fn unreachable_rtmp_target_reports_output_error_with_its_generation() {
    let _s = serial();
    let (mut p, rx, _f, _l) = programme();
    let target = TargetSpec {
        url: "rtmp://127.0.0.1:9/live".to_owned(),
        stream_key: "SECRETKEY-unreachable".to_owned(),
    };
    p.swap_sink(3, &target);
    let seen = events_until(&rx, Duration::from_secs(6), |e| {
        matches!(e, Event::OutputError { .. })
    });
    assert_eq!(seen.last(), Some(&Event::OutputError { gen: 3 }));
    // Once per generation: the domain's swap is the answer, and the dead sink says nothing more.
    let later = events_for(&rx, Duration::from_millis(1_000));
    let errors = later
        .iter()
        .filter(|e| matches!(e, Event::OutputError { .. }))
        .count();
    assert_eq!(errors, 0, "{later:?}");
    let t = Instant::now();
    let mut worst = Duration::ZERO;
    for i in 0..30 {
        let one = Instant::now();
        p.push(i, FrameChoice::Slate(SlateKind::Reconnecting));
        worst = worst.max(one.elapsed());
    }
    let took = t.elapsed();
    eprintln!("30 pushes after the error: {took:?}, worst {worst:?}");
    assert!(took < Duration::from_millis(1_500), "{took:?}");
    assert!(worst < period(&P720), "a push blocked for {worst:?}");
}

/// A failed output's own EOS is not a drain. Each appsrc whose push fails sends an EOS downstream,
/// so a sink that dies mid-stream posts its error and then an EOS (F-011's live legs: `OutputError`,
/// then `OutputDrained` 34 ms later with no drain asked for). That EOS neither samples progress after
/// the error nor spends the once-only `OutputDrained`, so the session's real drain, on the next
/// sink, still reports.
#[test]
fn a_failed_output_neither_reports_drained_nor_spends_the_drain() {
    let _s = serial();
    let dir = Tmp::new("failed-then-drain");
    let (mut p, rx, _f, lines) = programme();
    swap_before_the_first_frame(&mut p, &lines, 1, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..15, FrameChoice::Slate(SlateKind::Waiting));
    assert!(p.fail_sink_writes());
    pace(&mut p, 15..45, FrameChoice::Slate(SlateKind::Waiting));
    let after = events_for(&rx, Duration::from_millis(1_500));
    let error_at = after
        .iter()
        .position(|e| *e == Event::OutputError { gen: 1 })
        .unwrap_or_else(|| panic!("no OutputError: {after:?}"));
    let drained = after.iter().filter(|e| **e == Event::OutputDrained).count();
    let progress = after[error_at..]
        .iter()
        .filter(|e| matches!(e, Event::OutputProgress { gen: 1, .. }))
        .count();
    eprintln!(
        "after the sink failed: {} events, {drained} drained, {progress} gen-1 progress after the error",
        after.len()
    );
    assert_eq!(drained, 0, "{after:?}");
    assert_eq!(progress, 0, "{after:?}");

    // F-036: the second sink's keyframe is held to the floor after frame 0, the encoder's last, and
    // the pacing runs a margin past it, short of the GOP's own keyframe.
    let swap = 45;
    let due = key_due(swap, 0);
    let end = due + OWED84_MARGIN_TICKS;
    assert!(end < gop_ticks(), "setup: b ends before the GOP's own keyframe");
    p.swap_sink(2, &file_target(&dir.file("b.flv")));
    pace(&mut p, swap..end, FrameChoice::Slate(SlateKind::Waiting));
    let seen = drain_and_wait(&mut p, &rx);
    let mut drained = seen.iter().filter(|e| **e == Event::OutputDrained).count();
    drained += events_for(&rx, Duration::from_millis(500))
        .iter()
        .filter(|e| **e == Event::OutputDrained)
        .count();
    eprintln!("the real drain: {drained} drained");
    assert_eq!(drained, 1);
    let d = decode(&dir.file("b.flv"));
    eprintln!(
        "frames on the second sink: {} for the {} paced from its keyframe",
        d.frames.len(),
        end - due
    );
    assert!(
        d.first_is_key() && d.frames.len() as u64 + OWED84_KEY_WAIT_TICKS >= end - due,
        "{} frames",
        d.frames.len()
    );
}

/// Both shapes of key: one after `/live/` (the pattern rule catches it) and one after another path,
/// which only the scrub by value catches.
#[test]
fn errors_and_logs_never_contain_the_stream_key() {
    let _s = serial();
    let (mut p, rx, _f, lines) = programme();
    let keys = ["SECRETKEY-alpha-1234", "SECRETKEY/bravo+5678"];
    p.swap_sink(
        1,
        &TargetSpec {
            url: "rtmp://127.0.0.1:9/live/".to_owned(),
            stream_key: keys[0].to_owned(),
        },
    );
    events_until(&rx, Duration::from_secs(6), |e| {
        *e == Event::OutputError { gen: 1 }
    });
    p.swap_sink(
        2,
        &TargetSpec {
            url: "rtmp://127.0.0.1:9/app".to_owned(),
            stream_key: keys[1].to_owned(),
        },
    );
    events_until(&rx, Duration::from_secs(6), |e| {
        *e == Event::OutputError { gen: 2 }
    });
    // A refused scheme logs its target too.
    p.swap_sink(
        3,
        &TargetSpec {
            url: format!("srt://127.0.0.1:9?streamid={}", keys[0]),
            stream_key: keys[1].to_owned(),
        },
    );
    events_until(&rx, Duration::from_secs(6), |e| {
        *e == Event::OutputError { gen: 3 }
    });
    let debug = format!("{p:?}");
    let lines = lines.lock().unwrap().clone();
    let errors = lines.iter().filter(|l| l.contains("error")).count();
    eprintln!("log lines checked: {}, error lines: {errors}", lines.len());
    assert!(errors >= 2, "both failed sinks logged their error: {lines:?}");
    // Each generation names its sink, so the key reached the log path and the scrub is what hid it.
    for (gen, shown) in [
        (1, "rtmp://127.0.0.1:9/live/****"),
        (2, "rtmp://127.0.0.1:9/app/****"),
    ] {
        let named = format!("output gen={gen}: sink {shown}");
        assert!(lines.contains(&named), "{named:?} not in {lines:?}");
    }
    let forms: Vec<String> = keys
        .iter()
        .flat_map(|k| {
            [
                k.to_string(),
                k.replace('/', "%2F").replace('+', "%2B"),
                "SECRETKEY".to_owned(),
            ]
        })
        .collect();
    for text in lines.iter().chain([&debug]) {
        for f in &forms {
            assert!(!text.contains(f.as_str()), "{f:?} leaked in {text:?}");
        }
    }
}

/// Owed 13 replaced `OutputConnected`/`OutputAck` with `OutputProgress`: a file target reports the
/// growing total with its generation.
#[test]
fn file_target_reports_progress_with_its_generation() {
    let _s = serial();
    let dir = Tmp::new("progress-gen");
    let (mut p, rx, _f, _l) = programme();
    p.swap_sink(7, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..45, FrameChoice::Slate(SlateKind::Waiting));
    let seen = events_for(&rx, Duration::from_millis(600));
    let progress: Vec<(u64, u64)> = seen
        .iter()
        .filter_map(|e| match e {
            Event::OutputProgress { gen, total_bytes, .. } => Some((*gen, *total_bytes)),
            _ => None,
        })
        .collect();
    eprintln!("progress events read: {}", progress.len());
    assert!(progress.len() >= 4, "{progress:?}");
    assert!(progress.iter().all(|&(g, _)| g == 7), "{progress:?}");
    assert!(progress.iter().any(|&(_, t)| t > 0), "{progress:?}");
    assert!(
        !seen
            .iter()
            .any(|e| matches!(e, Event::OutputError { .. } | Event::OutputDrained)),
        "{seen:?}"
    );
}

#[test]
fn stats_are_absent_before_anything_is_measured() {
    let _s = serial();
    gst::init().expect("gstreamer");
    let (tx, _rx) = crossbeam_channel::bounded(16);
    let fresh = GstProgramme::new(FrameStore::default(), tx);
    assert_eq!(fresh.stats().snapshot(), StatsSnapshot::default());
    // Started, but nothing pushed and no sink: still nothing measured.
    let (p, _rx, _f, _l) = programme();
    assert_eq!(p.stats().snapshot(), StatsSnapshot::default());
}

/// Owed 86, the producer: the encoder's sink counts every video frame it hands on, and the count
/// is the frames the output actually carries, read back from the file by an independent decoder.
#[test]
fn stats_encoded_frames_are_the_frames_the_output_carries() {
    let _s = serial();
    let dir = Tmp::new("encoded");
    let (mut p, rx, _f, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..90, FrameChoice::Slate(SlateKind::Waiting));
    drain_and_wait(&mut p, &rx);
    let d = decode(&dir.file("a.flv"));
    eprintln!("frames decoded from the output: {}", d.frames.len());
    assert_eq!(d.frames.len(), 90, "the file's frames");
    assert_eq!(
        p.stats().snapshot().encoded_frames,
        Some(90),
        "the counter against the file"
    );
}

/// Owed 86 (the PR #69 review's I1), the producer through the beat's sampler: the pushes keep the
/// tick grid's pace while the encoder hangs (`hang_encoder`, the hook that holds its output), as C14
/// case 6's starvation held it (F-024: the beat counted 29 to 31 pushes a second while the output
/// carried 11.5). The beat reads under the floor though every tick was pushed on time, so the reading
/// is the encoder's output, never a push count; and after a whole window of real pace that lets the
/// encoder's backlog out (the review's m5), the next window holds the band again. Every window is past
/// the minimum (`OUTPUT_FPS_MIN_WINDOW_MS`). The band is the profile's: at least the floor, and no
/// further above the profile's rate than the floor is below it.
#[test]
fn the_beats_fps_falls_while_the_encoder_hangs_though_every_push_keeps_its_pace() {
    let _s = serial();
    let (mut p, _rx, _f, _l) = programme();
    let mut m = ProcMetrics::new(p.stats());
    let fps = u64::from(P720.fps);
    let (floor, rate) = (P720.fps_floor(), f64::from(P720.fps));
    let band = floor..=rate + (rate - floor);
    let window_ms = OUTPUT_FPS_MIN_WINDOW_MS + 500;
    let window = fps * window_ms / 1_000;
    let hang = Duration::from_millis(2_000);
    assert!(
        hang.as_millis() >= 1_500 && hang.as_millis() + 500 < u128::from(window_ms),
        "the hang lasts at least 1.5 s and ends inside its window"
    );
    let mut next = 0;
    let mut paced = |p: &mut GstProgramme, ticks: u64| -> Duration {
        let t = Instant::now();
        pace(p, next..next + ticks, FrameChoice::Slate(SlateKind::Waiting));
        next += ticks;
        t.elapsed()
    };
    paced(&mut p, fps);
    assert!(
        p.stats().snapshot().encoded_frames.is_some(),
        "the encoder has started"
    );
    assert_eq!(m.sample().fps, None, "the window opens");
    paced(&mut p, window);
    let healthy = m.sample().fps.expect("a window past the minimum");
    assert!(p.hang_encoder(hang), "the encoder hook");
    let took = paced(&mut p, window);
    let starved = m.sample().fps.expect("a window past the minimum");
    paced(&mut p, window);
    let settled = m.sample().fps.expect("a window past the minimum");
    paced(&mut p, window);
    let resumed = m.sample().fps.expect("a window past the minimum");
    let on_time = period(&P720) * (window as u32 - 1);
    eprintln!(
        "beat fps over {window} ticks a window: {healthy:.3} → encoder hung {hang:?}, {window} pushes \
         in {took:?} (on time {on_time:?}): {starved:.3} → settled {settled:.3} → resumed {resumed:.3}"
    );
    assert!(band.contains(&healthy), "real pace {healthy}, band {band:?}");
    // Every tick of the hung window was pushed on the grid, so a push count would read the rate.
    assert!(
        took < on_time + Duration::from_millis(250),
        "the pushes kept their pace: {took:?} for {on_time:?}"
    );
    assert!(starved < floor, "starved {starved}, floor {floor}");
    assert!(band.contains(&resumed), "resumed {resumed}, band {band:?}");
}

#[test]
fn stats_egress_and_bitrate_follow_bytes_sent() {
    let _s = serial();
    let dir = Tmp::new("egress");
    let (mut p, rx, _f, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..90, FrameChoice::Slate(SlateKind::Waiting));
    drain_and_wait(&mut p, &rx);
    let snap = p.stats().snapshot();
    let size = std::fs::metadata(dir.file("a.flv")).expect("file").len();
    assert_eq!(snap.egress_bytes, Some(size), "egress is the file's final size");
    let kbps = f64::from(snap.bitrate_kbps.expect("a complete window"));
    let want = f64::from(P720.video_kbps);
    assert!(
        (kbps - want).abs() <= want * 0.5,
        "bitrate {kbps} kbps against {want} ± 50 %"
    );
}

/// The encoder's speed over 2 s of real-pace pushes from `from` (PR #28 review item 5). One 1 s window
/// moves by a whole frame (1/30 = 0.033) when a frame lands on either side of its edge, the size of
/// the 0.95 floor's margin, so one window flaked. Two consecutive windows, read exactly 1 s apart
/// on the tick grid (the 2nd and 3rd seconds, after a whole window of real-time output), halve it.
fn paced_speed_over_two_windows(p: &mut GstProgramme, from: u64) -> f64 {
    let stats = p.stats();
    let fps = u64::from(P720.fps);
    let mut read = Vec::new();
    pace_with(
        p,
        from..from + 3 * fps + 2,
        |_| FrameChoice::Slate(SlateKind::Waiting),
        |i| {
            if i == from + 2 * fps || i == from + 3 * fps {
                read.push(stats.snapshot().encoder_speed.expect("a window"));
            }
        },
    );
    assert_eq!(read.len(), 2, "{read:?}");
    eprintln!("encoder speed windows from tick {from}: {read:?}");
    (read[0] + read[1]) / 2.0
}

#[test]
fn stats_encoder_speed_tracks_real_time_and_drops_when_starved() {
    let _s = serial();
    let (mut p, _rx, _f, _l) = programme();
    let band = ENCODER_SPEED_REALTIME_MIN..=1.1;
    let first = paced_speed_over_two_windows(&mut p, 0);
    thread::sleep(Duration::from_millis(1_500));
    let starved = p.stats().snapshot().encoder_speed.expect("a window");
    let resumed = paced_speed_over_two_windows(&mut p, 92);
    eprintln!("encoder speed: {first:.3} → starved {starved:.3} → resumed {resumed:.3}");
    assert!(band.contains(&first), "real time {first}");
    assert!(starved < 0.5, "starved {starved}");
    assert!(band.contains(&resumed), "resumed {resumed}");
}

/// The ring's cap is the playout's (C4: `PLAYOUT_CAP_MS`, 500 ms; the plan's 400 ms amended).
#[test]
fn audio_ring_is_bounded_at_the_playout_cap_and_pads_silence() {
    let f = FrameStore::default();
    let second: Vec<u8> = (0..192_000u32).map(|i| (i % 251 + 1) as u8).collect();
    f.push_audio(&second);
    assert_eq!(f.audio_level(), AUDIO_RING_BYTES);
    let got = f.take_audio(192_000);
    assert_eq!(got.len(), 192_000);
    let pad = 192_000 - AUDIO_RING_BYTES;
    let cap_bytes = PLAYOUT_CAP_MS as usize * 48 * 4;
    assert_eq!(
        pad,
        192_000 - cap_bytes,
        "the second less the cap, 48 kHz stereo S16LE"
    );
    assert!(got[..pad].iter().all(|&b| b == 0), "the oldest part is silence");
    assert_eq!(
        &got[pad..],
        &second[192_000 - AUDIO_RING_BYTES..],
        "the newest cap's worth survives"
    );
    assert_eq!(f.audio_level(), 0);
    // An empty ring gives n bytes of padding; a push that is not whole frames is refused.
    assert_eq!(f.take_audio(6_400), vec![0u8; 6_400]);
    f.push_audio(&[1, 2, 3]);
    assert_eq!((f.audio_level(), f.audio_refused()), (0, 1));
}

/// The PR #35 review's m1: `clear_audio` empties the ring and says how much it dropped (the input's
/// abandon, loss and rebuild call it), and `audio_taken` counts the ring's real bytes handed out,
/// never the padding, so a test can tell what of a camera's audio went on air. The sequence: an
/// empty ring, a second clear, and a take after a clear.
#[test]
fn clear_audio_empties_the_ring_and_audio_taken_counts_real_bytes_only() {
    let f = FrameStore::default();
    assert_eq!(f.clear_audio(), 0, "an empty ring drops nothing");
    assert_eq!(f.audio_taken(), 0);
    f.push_audio(&[7u8; 4_000]);
    assert_eq!(f.take_audio(1_000).len(), 1_000);
    assert_eq!(f.audio_taken(), 1_000);
    assert_eq!(f.clear_audio(), 3_000, "the rest of the ring");
    assert_eq!(f.audio_level(), 0);
    assert_eq!(f.clear_audio(), 0, "a second clear drops nothing");
    let pad = f.take_audio(6_400);
    assert_eq!(pad, vec![0u8; 6_400], "after a clear only padding comes out");
    assert_eq!(f.audio_taken(), 1_000, "padding is not counted");
    f.push_audio(&[9u8; 400]);
    let mixed = f.take_audio(1_000);
    assert_eq!(&mixed[600..], &[9u8; 400][..], "the real bytes come last");
    assert_eq!(f.audio_taken(), 1_400, "only the 400 real bytes of a padded take");
}

// ---------------------------------------------------------------------------------------------
// Amendment 2026-10-15: the textless slate

#[test]
fn slate_is_textless_house_backdrop_and_logo_for_every_kind() {
    let _s = serial();
    gst::init().expect("gstreamer");
    let mut modes = 0;
    // Both modes (ADR 0005): the logo box is 480 × 168 at 720p and 720 × 252 at 1080p.
    for profile in [OutputProfile::P720, OutputProfile::P1080] {
        let mut kinds = 0;
        let (w, h) = (profile.width, profile.height);
        let slate = Slate::render(w, h).expect("slate");
        let first = slate.slate_for(SlateKind::ALL[0]);
        for kind in SlateKind::ALL {
            let s = slate.slate_for(kind);
            assert!(Arc::ptr_eq(&s, &first), "{kind:?} shares the one buffer");
            let pic = Pic::of(&s);
            assert_eq!((pic.w, pic.h), (w, h));
            for (x, y) in [(10, 10), (w - 10, h - 10)] {
                let px = pic.at(x, y);
                assert!(close(px, HOUSE_NIGHT, 4), "{kind:?} {w}x{h} ({x},{y}) = {px:?}");
            }
            let share = pic.logo_share();
            assert!(
                share >= 0.10,
                "{kind:?} {w}x{h}: logo covers {share:.3} of its box"
            );
            kinds += 1;
        }
        assert!(
            !slate.factories().contains(&"textoverlay"),
            "{:?}",
            slate.factories()
        );
        assert!(slate.factories().contains(&"pngdec"), "{:?}", slate.factories());
        eprintln!("slate kinds checked: {kinds} ({w}x{h})");
        assert_eq!(
            SLATE_BACKDROP_RGB, HOUSE_NIGHT,
            "the adapter's backdrop is the house colour"
        );
        assert_eq!(kinds, SlateKind::ALL.len());
        assert!(kinds > 0);
        modes += 1;
    }
    assert_eq!(modes, 2);
    assert_eq!(logo_box(1_280, 720), (400, 276, 480, 168));
    assert_eq!(logo_box(1_920, 1_080), (600, 414, 720, 252));
}

#[test]
fn vendored_logo_matches_its_pinned_sha256() {
    let got = hex::encode(Sha256::digest(LOGO_PNG));
    assert_eq!(got, LOGO_SHA256);
}

#[test]
fn an_opaque_overlay_covers_the_slate_and_hiding_it_reveals_the_slate() {
    let _s = serial();
    let dir = Tmp::new("cover");
    let (mut p, rx, frames, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    let (w, h) = OVERLAY_SIZE;
    frames.set_overlay(solid(gst_video::VideoFormat::Bgra, w, h, [0, 255, 0, 255]));
    pace(&mut p, 0..15, FrameChoice::Slate(SlateKind::Reconnecting));
    p.hide_overlay();
    pace(&mut p, 15..30, FrameChoice::Slate(SlateKind::Reconnecting));
    drain_and_wait(&mut p, &rx);
    let d = decode(&dir.file("a.flv"));
    assert_eq!(d.frames.len(), 30);
    let covered = &d.frames[14];
    assert!(
        is_green(covered.centre) && is_green(covered.corner_tl),
        "{covered:?}"
    );
    assert!(
        covered.logo_share > 0.9,
        "the overlay covers the logo: {covered:?}"
    );
    let revealed = &d.frames[29];
    assert!(close(revealed.corner_tl, HOUSE_NIGHT, ON_AIR_TOL), "{revealed:?}");
    assert!(close(revealed.corner_br, HOUSE_NIGHT, ON_AIR_TOL), "{revealed:?}");
    assert!(revealed.logo_share >= 0.10, "the logo is back: {revealed:?}");
}

// ---------------------------------------------------------------------------------------------
// Amendment 2026-10-18: no rung clears the overlay; the owed checks

#[test]
fn the_bottom_rung_keeps_the_overlay_on_air() {
    let _s = serial();
    let dir = Tmp::new("bottom-rung");
    let (mut p, rx, frames, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    frames.set_overlay(overlay_blue_corner());
    p.step_down(Rung::OverlayHalfRate);
    pace(&mut p, 0..60, FrameChoice::Slate(SlateKind::Waiting));
    assert_eq!(p.overlay_pushes(), 30);
    // `Full` (never emitted by the domain; the match is total) restores every-tick pushes.
    p.step_down(Rung::Full);
    pace(&mut p, 60..90, FrameChoice::Slate(SlateKind::Waiting));
    assert_eq!(p.overlay_pushes(), 60);
    drain_and_wait(&mut p, &rx);
    let d = decode(&dir.file("a.flv"));
    assert_eq!(d.frames.len(), 90);
    let (mut even, mut odd) = (0, 0);
    for (i, f) in d.frames.iter().enumerate() {
        assert!(is_blue(f.spot), "frame {i}: {:?}", f.spot);
        if i % 2 == 0 {
            even += 1;
        } else {
            odd += 1;
        }
    }
    eprintln!("blue frames checked: even {even}, odd {odd}");
    assert!(even > 0 && odd > 0);
}

#[test]
fn hide_then_set_overlay_composites_again() {
    let _s = serial();
    let dir = Tmp::new("hide-set");
    let (mut p, rx, frames, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    frames.set_overlay(overlay_blue_corner());
    p.hide_overlay();
    pace(&mut p, 0..15, FrameChoice::Slate(SlateKind::Waiting));
    frames.set_overlay(overlay_blue_corner());
    pace(&mut p, 15..30, FrameChoice::Slate(SlateKind::Waiting));
    drain_and_wait(&mut p, &rx);
    let d = decode(&dir.file("a.flv"));
    assert_eq!(d.frames.len(), 30);
    assert!(!is_blue(d.frames[14].spot), "hidden: {:?}", d.frames[14].spot);
    assert!(
        is_blue(d.frames[29].spot),
        "set again, on air: {:?}",
        d.frames[29].spot
    );
}

fn progress_of(events: &[Event], want: u64) -> Vec<u64> {
    events
        .iter()
        .filter_map(|e| match e {
            Event::OutputProgress { gen, total_bytes, .. } if *gen == want => Some(*total_bytes),
            _ => None,
        })
        .collect()
}

#[test]
fn progress_total_grows_while_pushing_and_freezes_when_the_sink_stops() {
    let _s = serial();
    let dir = Tmp::new("grow-freeze");
    let (mut p, rx, _f, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    let mut while_pushing = Vec::new();
    pace_with(
        &mut p,
        0..90,
        |_| FrameChoice::Slate(SlateKind::Waiting),
        |_| {
            while let Ok(Inbound::Event(e)) = rx.try_recv() {
                while_pushing.push(e);
            }
        },
    );
    let grown = progress_of(&while_pushing, 1);
    let first = grown.iter().position(|&t| t > 0).expect("bytes were sent");
    let grown = &grown[first..];
    eprintln!("progress events read while pushing: {}", grown.len());
    assert!(grown.len() >= 8, "{grown:?}");
    for w in grown.windows(2) {
        assert!(
            w[1] > w[0],
            "the total grows on every poll while pushing: {grown:?}"
        );
    }
    let drained = drain_and_wait(&mut p, &rx);
    let last = *progress_of(&drained, 1).last().expect("the final sample");
    let after = progress_of(&events_for(&rx, Duration::from_millis(1_000)), 1);
    eprintln!("progress events read after the drain: {}", after.len());
    assert!(!after.is_empty(), "the poll keeps reading after the drain");
    assert!(
        after.iter().all(|&t| t <= last),
        "after {after:?} past the last {last}"
    );
    assert_eq!(last, std::fs::metadata(dir.file("a.flv")).expect("file").len());
}

/// Owed 15 (a): the poll is at most 250 ms (the choice, `_FINDINGS.md` F-011).
#[test]
fn the_progress_poll_is_owed_15s_choice() {
    // Read at run time, so the check is a test that reds rather than a constant folded away.
    let poll = std::hint::black_box(OUTPUT_PROGRESS_POLL_MS);
    assert!(poll <= OWED_15_POLL_MAX_MS, "{poll} ms");
    assert!(poll > 0);
}

/// rtmp2sink's `timeout` in its unit, whole seconds, from the domain's constant (Owed 87).
fn sink_timeout_s() -> u32 {
    assert!(OUTPUT_SINK_TIMEOUT_MS.is_multiple_of(1_000), "whole seconds");
    u32::try_from(OUTPUT_SINK_TIMEOUT_MS / 1_000).expect("fits the property")
}

/// An RTMP target nothing listens on: the sink is built and fails its connect; its properties stay
/// readable until the next swap.
fn unreachable_rtmp(key: &str) -> TargetSpec {
    TargetSpec {
        url: "rtmp://127.0.0.1:9/live".to_owned(),
        stream_key: key.to_owned(),
    }
}

/// Owed 87 (the owner, 2026-10-24, "A"): the premise of "set, not inherited". The domain's constant
/// is today's rtmp2sink default, read from the element GStreamer installed (1.24.2,
/// `gstrtmp2locationhandler.c:35` `DEFAULT_TIMEOUT 5`). A red here means GStreamer's default moved:
/// the product still sets the constant, but Owed 87's "equal to today's default" needs a new look.
#[test]
fn the_sink_timeout_is_rtmp2sinks_own_default() {
    gst::init().expect("gstreamer");
    let sink = gst::ElementFactory::make("rtmp2sink").build().expect("rtmp2sink");
    let spec = sink.find_property("timeout").expect("rtmp2sink has `timeout`");
    assert_eq!(spec.value_type(), u32::static_type(), "a uint, in seconds");
    assert_eq!(sink.property::<u32>("timeout"), sink_timeout_s());
}

/// Owed 87: every RTMP sink is built with `timeout` = the domain's constant, read back from the
/// element itself, on the first build and on a swap; a `file://` sink has no such property.
#[test]
fn every_rtmp_sink_gets_the_domains_timeout() {
    let _s = serial();
    let dir = Tmp::new("sink-timeout");
    let (mut p, _rx, _f, _l) = programme();
    let mut checked = 0;
    for (gen, key) in [(1, "SECRETKEY-timeout-a"), (2, "SECRETKEY-timeout-b")] {
        p.swap_sink(gen, &unreachable_rtmp(key));
        assert_eq!(p.output_sink_timeout_s(), Some(sink_timeout_s()), "gen {gen}");
        checked += 1;
    }
    p.swap_sink(3, &file_target(&dir.file("c.flv")));
    assert_eq!(p.output_sink_timeout_s(), None, "a filesink has no timeout");
    eprintln!("rtmp sinks checked: {checked}");
    assert_eq!(checked, 2);
}

/// Owed 87: the timeout the adapter sets is what the sink gets, so a value other than rtmp2sink's
/// default reads back as itself (the production value equals the default, so only this tells the
/// set from no set). It holds for every sink built after it, never for one built before.
#[test]
fn a_sink_timeout_other_than_the_default_reaches_every_later_sink() {
    let _s = serial();
    let (mut p, _rx, _f, _l) = programme();
    let other = sink_timeout_s() + 2;
    p.swap_sink(1, &unreachable_rtmp("SECRETKEY-timeout-c"));
    p.set_output_sink_timeout_s(other);
    assert_eq!(
        p.output_sink_timeout_s(),
        Some(sink_timeout_s()),
        "a sink built before keeps its timeout"
    );
    let mut checked = 0;
    for (gen, key) in [(2, "SECRETKEY-timeout-d"), (3, "SECRETKEY-timeout-e")] {
        p.swap_sink(gen, &unreachable_rtmp(key));
        assert_eq!(p.output_sink_timeout_s(), Some(other), "gen {gen}");
        checked += 1;
    }
    eprintln!("overridden sinks checked: {checked}");
    assert_eq!(checked, 2);
}

/// C08 amendment 2026-10-19: the adapter drops within 500 ms while what it owns is hung.
#[test]
fn drop_returns_within_500_ms_while_the_pipelines_hang() {
    let _s = serial();
    let dir = Tmp::new("drop");
    let (mut p, _rx, _f, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..15, FrameChoice::Slate(SlateKind::Waiting));
    let hold = Duration::from_secs(3);
    assert!(p.hang_sink(hold), "the sink hook");
    assert!(p.hang_encoder(hold), "the encoder hook");
    // Two more ticks reach both probes, which then sleep holding their streaming locks.
    pace(&mut p, 15..20, FrameChoice::Slate(SlateKind::Waiting));
    thread::sleep(Duration::from_millis(300));
    let teardowns = p.teardown_counter();
    let t = Instant::now();
    drop(p);
    let took = t.elapsed();
    let in_flight = teardowns.load(Ordering::SeqCst);
    eprintln!("drop took {took:?}; teardowns in flight after it: {in_flight}");
    assert!(took < Duration::from_millis(500), "{took:?}");
    assert_eq!(
        in_flight, 2,
        "both teardowns are still held by the hang, so the hang was real"
    );
    let end = Instant::now() + Duration::from_secs(10);
    while teardowns.load(Ordering::SeqCst) > 0 && Instant::now() < end {
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        teardowns.load(Ordering::SeqCst),
        0,
        "the reapers finish once the hang ends"
    );
}

// ---------------------------------------------------------------------------------------------
// The audio section (F-004 verdict): real audio through, noise when there is none

/// A 440 Hz tone at −20 dBFS peak, one tick of it, continuing from sample `n`.
fn tone(n: &mut u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(TICK_AUDIO_BYTES);
    for _ in 0..TICK_AUDIO_BYTES / 4 {
        let v = (0.1
            * f64::from(i16::MAX)
            * (2.0 * std::f64::consts::PI * 440.0 * *n as f64 / 48_000.0).sin()) as i16;
        *n += 1;
        let [lo, hi] = v.to_le_bytes();
        out.extend_from_slice(&[lo, hi, lo, hi]);
    }
    out
}

fn report_audio(case: &str, d: &Decoded) -> (f64, f64, f64) {
    let fps = d.aac_frames as f64 / (d.audio_ms() as f64 / 1_000.0);
    let (kbps, zeros) = (d.aac_kbps(), d.zero_share());
    eprintln!(
        "audio {case}: {} AAC frames, {fps:.2} frames/s, {kbps:.1} kbps, zero-byte share {:.3}",
        d.aac_frames, zeros
    );
    (fps, kbps, zeros)
}

#[test]
fn camera_lost_audio_is_coded_noise_never_digital_silence() {
    let _s = serial();
    let dir = Tmp::new("noise");
    let (mut p, rx, _f, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    // No input audio at all: the slate, and nothing in the ring.
    pace(&mut p, 0..90, FrameChoice::Slate(SlateKind::Reconnecting));
    drain_and_wait(&mut p, &rx);
    let d = decode(&dir.file("a.flv"));
    let (fps, kbps, zeros) = report_audio("camera lost", &d);
    assert!(d.aac_frames > 100, "{}", d.aac_frames);
    assert!((fps - AAC_FRAMES_PER_S).abs() < 1.5, "{fps}");
    assert!((kbps - AAC_KBPS).abs() < AAC_KBPS * 0.2, "{kbps}");
    assert!(
        zeros <= 0.5,
        "zero-byte share {zeros:.3}: digital silence (F-004)"
    );
    let quiet = d.rms_dbfs(200, 2_800);
    assert!((-80.0..=-45.0).contains(&quiet), "filler level {quiet:.1} dBFS");
}

#[test]
fn live_audio_passes_through_and_an_underrun_neither_silences_nor_gaps_the_track() {
    let _s = serial();
    let dir = Tmp::new("live-audio");
    let (mut p, rx, frames, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    let mut n = 0u64;
    // 1 s of input audio, 1 s of none (the camera lost), 1 s of audio again.
    pace_with(
        &mut p,
        0..90,
        |_| FrameChoice::Slate(SlateKind::Waiting),
        |i| {
            if !(30..60).contains(&i) {
                frames.push_audio(&tone(&mut n));
            }
        },
    );
    drain_and_wait(&mut p, &rx);
    let d = decode(&dir.file("a.flv"));
    // The zero-byte share is reported, not held: a steady tone is coded with CBR fill bytes, which
    // are zeros (F-004's measure tells silence from noise, not a tone from either). The levels below
    // are the check.
    report_audio("live, underrun, live", &d);
    assert!(
        d.audio_ms().abs_diff(3_000) <= AV_TOLERANCE_MS,
        "no gap in the track: {} ms",
        d.audio_ms()
    );
    let (live1, lost, live2) = (
        d.rms_dbfs(200, 900),
        d.rms_dbfs(1_200, 1_900),
        d.rms_dbfs(2_200, 2_900),
    );
    eprintln!("segment levels: {live1:.1} / {lost:.1} / {live2:.1} dBFS");
    // A −20 dBFS-peak sine has an RMS of −23 dBFS.
    assert!((-27.0..=-19.0).contains(&live1), "{live1}");
    assert!((-80.0..=-45.0).contains(&lost), "{lost}");
    assert!((-27.0..=-19.0).contains(&live2), "{live2}");
}

// ---------------------------------------------------------------------------------------------
// Sequences: a second call, an empty input, after an interruption, the other mode

/// Three sinks in turn. Each of the first two lives the forced keyframe's floor (F-036), so each
/// sink's keyframe is due at its own swap; the last lives a GOP and more, and its file shows the GOP
/// restarting at the keyframe its swap forced: keyframes at its first frame and a GOP on.
#[test]
fn swapping_twice_starts_each_sink_at_a_keyframe_and_drains_once() {
    let _s = serial();
    let dir = Tmp::new("swap-twice");
    let (mut p, rx, _f, lines) = programme();
    let mut drained = 0;
    let each = floor_ticks();
    for (gen, name) in [(1u64, "a.flv"), (2, "b.flv"), (3, "c.flv")] {
        let target = file_target(&dir.file(name));
        if gen == 1 {
            swap_before_the_first_frame(&mut p, &lines, gen, &target);
        } else {
            swap_when_asked(&mut p, &lines, gen, &target);
        }
        let from = (gen - 1) * each;
        let to = from
            + if gen == 3 {
                gop_ticks() + OWED84_MARGIN_TICKS
            } else {
                each
            };
        pace(&mut p, from..to, FrameChoice::Slate(SlateKind::Waiting));
    }
    let mut seen = drain_and_wait(&mut p, &rx);
    seen.extend(events_for(&rx, Duration::from_millis(700)));
    drained += seen.iter().filter(|e| **e == Event::OutputDrained).count();
    assert_eq!(drained, 1, "{seen:?}");
    assert!(
        !seen.iter().any(|e| matches!(e, Event::OutputError { .. })),
        "{seen:?}"
    );
    let mut sinks = 0;
    for name in ["a.flv", "b.flv", "c.flv"] {
        let d = decode(&dir.file(name));
        assert!(
            d.frames.len() as u64 + OWED84_MARGIN_TICKS >= each,
            "{name}: {}",
            d.frames.len()
        );
        assert!(d.first_is_key(), "{name}");
        if name == "c.flv" {
            assert_eq!(
                key_indexes(&d),
                vec![0, gop_ticks()],
                "{name}: the GOP from its keyframe"
            );
        }
        assert_eq!(d.video_pts_ms.first(), Some(&0), "{name}");
        let first_audio = *d.audio_pts_ms.first().expect("audio");
        assert!(
            first_audio <= 1_024 * 1_000 / 48_000,
            "{name}: audio starts at {first_audio} ms"
        );
        sinks += 1;
    }
    eprintln!("sinks checked: {sinks}");
    assert_eq!(sinks, 3);
    assert_eq!(p.builds(), 1);
}

#[test]
fn a_swap_during_the_drain_still_drains_once() {
    let _s = serial();
    let dir = Tmp::new("swap-drain");
    let (mut p, rx, _f, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..30, FrameChoice::Slate(SlateKind::Waiting));
    p.drain();
    p.swap_sink(2, &file_target(&dir.file("b.flv")));
    let mut seen = events_until(&rx, Duration::from_secs(15), |e| *e == Event::OutputDrained);
    seen.extend(events_for(&rx, Duration::from_millis(1_000)));
    let drained = seen.iter().filter(|e| **e == Event::OutputDrained).count();
    assert_eq!(drained, 1, "{seen:?}");
    assert!(
        !seen.iter().any(|e| matches!(e, Event::OutputError { .. })),
        "{seen:?}"
    );
    // A push after the drain is refused, never a panic.
    p.push(30, FrameChoice::Slate(SlateKind::Waiting));
    assert_eq!(p.refused_pushes(), 1);
}

/// A first sink that comes late sends nothing from before its swap (Owed 84; owner, 2026-10-24,
/// option A), and the keyframe asked for at its swap never cuts a GOP under Cloudflare's floor
/// (F-036). A file can write at once, so its reconnect point is the swap. The ring then holds the
/// encoder's own keyframe at the GOP (a slate), which must not start the file; the swap comes `early`
/// ticks before the floor after that keyframe, so the keyframe asked for is held to the floor, and
/// the file starts there. Three mutants show: the ring replayed (the file starts at the GOP's
/// keyframe, on a slate), the floor gone (it starts at the swap, `early` frames too many), the request
/// dropped (the encoder's own next keyframe comes after the last tick paced: an empty file).
#[test]
fn a_late_first_sink_starts_at_the_keyframe_after_its_swap_not_inside_the_ring() {
    let _s = serial();
    let dir = Tmp::new("late-sink");
    let (mut p, rx, frames, _l) = programme();
    frames.set_camera(solid(gst_video::VideoFormat::I420, 1_280, 720, [255, 0, 0, 255]));
    let gop = gop_ticks();
    // Half a second: inside the ring's 2 s after the GOP's keyframe, and more than
    // `OWED84_KEY_WAIT_TICKS` between the swap and the floor.
    let early = 15;
    let swap = gop + floor_ticks() - early;
    let due = key_due(swap, last_natural_key(swap));
    assert_eq!(due, swap + early, "setup: the request is held to the floor");
    // Up to the encoder's own next keyframe, never pushing it.
    let paced = 2 * gop - swap;
    pace(
        &mut p,
        0..swap - OWED84_MARGIN_TICKS,
        FrameChoice::Slate(SlateKind::Waiting),
    );
    pace(&mut p, swap - OWED84_MARGIN_TICKS..swap, FrameChoice::Emit);
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    pace(&mut p, swap..swap + paced, FrameChoice::Emit);
    drain_and_wait(&mut p, &rx);
    let len = file_len(&dir.file("a.flv"));
    assert!(
        len > 0,
        "late sink: nothing reached the file ({len} B): the request was dropped"
    );
    let d = decode(&dir.file("a.flv"));
    assert!(d.first_is_key(), "keys {:?}", &d.keys[..d.keys.len().min(5)]);
    assert_eq!(d.video_pts_ms.first(), Some(&0));
    let n = d.frames.len() as u64;
    let owed = swap + paced - due;
    eprintln!("late sink: swap at {swap}, keyframe due at {due}; {n} frames for the {owed} paced from it");
    assert!(
        n <= owed,
        "{n} frames for the {owed} paced from the floor: it started before the floor"
    );
    assert!(
        n + OWED84_KEY_WAIT_TICKS >= owed,
        "{n} frames: the keyframe came more than {OWED84_KEY_WAIT_TICKS} ticks after the floor"
    );
    let checked = assert_all_camera("late sink", &d);
    eprintln!("late sink: frames checked {checked}");
    assert_eq!(checked as u64, n);
    assert!(checked > 0);
}

/// F-036 (the C1 review's I1): the request carries the running time from which a keyframe serves the
/// swap (just after the newest video in the ring), so a keyframe the encoder has made at or after it
/// serves the swap, and none is forced a floor later. The encoder's next output is held
/// (`hang_encoder`) and the GOP's own keyframe pushed into it: GstVideoEncoder has noted that keyframe
/// but the ring has not got it when the sink can send, so the reconnect point comes before it. The
/// stream starts there, and the file's next keyframe is the GOP's, a GOP on. One mutant shows: an
/// unstamped request (any running time) is held to the floor after that keyframe and then forced, so
/// the file's second keyframe comes a floor in and cuts a GOP of the floor.
#[test]
fn a_keyframe_made_after_the_reconnect_point_serves_the_swap_and_none_is_forced_a_floor_on() {
    let _s = serial();
    let dir = Tmp::new("stamped-ask");
    let (mut p, rx, _f, lines) = programme();
    let stats = p.stats();
    let gop = gop_ticks();
    let slate = FrameChoice::Slate(SlateKind::Waiting);
    let hold = Duration::from_secs(2);
    pace(&mut p, 0..gop, slate);
    thread::sleep(OWED84_ENCODED_SETTLE);
    assert_eq!(
        stats.snapshot().encoded_frames,
        Some(gop),
        "setup: the GOP's frames reached the ring"
    );
    assert!(p.hang_encoder(hold), "the encoder hook");
    let hung = Instant::now();
    p.push(gop, slate);
    thread::sleep(OWED84_ENCODED_SETTLE);
    swap_when_asked(&mut p, &lines, 1, &file_target(&dir.file("a.flv")));
    let asked = hung.elapsed();
    eprintln!("stamped ask: taken {asked:?} into the encoder's {hold:?} hold");
    assert_eq!(
        stats.snapshot().encoded_frames,
        Some(gop),
        "setup: the GOP's keyframe reached the ring before the ask ({asked:?})"
    );
    assert!(
        asked + OWED84_ENCODED_SETTLE < hold,
        "setup: the ask came {asked:?} into the {hold:?} hold"
    );
    thread::sleep(hold.saturating_sub(hung.elapsed()) + OWED84_ENCODED_SETTLE);
    assert_eq!(
        stats.snapshot().encoded_frames,
        Some(gop + 1),
        "setup: the hold ended"
    );
    let end = 2 * gop + OWED84_MARGIN_TICKS;
    pace(&mut p, gop + 1..end, slate);
    drain_and_wait(&mut p, &rx);
    let d = decode(&dir.file("a.flv"));
    assert!(d.first_is_key(), "keys {:?}", &d.keys[..d.keys.len().min(5)]);
    let n = d.frames.len() as u64;
    let owed = end - gop;
    eprintln!(
        "stamped ask: {n} frames for the {owed} from the GOP's keyframe; keyframes at {:?}",
        key_indexes(&d)
    );
    assert!(
        n <= owed && n + OWED84_KEY_WAIT_TICKS >= owed,
        "{n} frames for the {owed} from the GOP's keyframe"
    );
    assert_eq!(
        key_indexes(&d),
        vec![0, gop],
        "the GOP's own keyframes, none forced (a forced one comes at {})",
        floor_ticks()
    );
}

/// F-036 (the C1 review's I3): a swap whose ask reaches the encoder one or two frames after the GOP's
/// own keyframe went into the ring. The keyframe asked for is held to the floor after it, so the file
/// starts at `key_due`, its keyframes are a GOP apart, and no interval in it is under the floor. This
/// holds because x264 has no frame delay (the encoder's comment in `gst_programme.rs`): with frame
/// threads (`sliced-threads=false`) the GOP's keyframe is still inside x264 when the ask comes, the
/// request is not throttled, and the file starts at the GOP's keyframe with a GOP of one or two
/// frames after it (predicted; the mutant runs only where GStreamer does). Both modes.
#[test]
fn a_keyframe_asked_frames_after_the_gops_own_waits_for_the_floor() {
    let _s = serial();
    let (mut cases, mut intervals) = (0, 0);
    for profile in [P720, OutputProfile::P1080] {
        let gop = u64::from(profile.gop_frames());
        let floor = u64::from(profile.forced_keyframe_min_frames());
        for after in [1, 2] {
            let case = format!("{}p, asked {after} after the GOP's keyframe", profile.height);
            let dir = Tmp::new("ask-after-key");
            let (mut p, rx, _f, lines) = programme_with(&profile);
            let slate = FrameChoice::Slate(SlateKind::Waiting);
            let swap = gop + after;
            let due = key_due(swap, last_natural_key(swap));
            assert_eq!(due, gop + floor, "{case}: setup: held to the floor");
            pace(&mut p, 0..swap, slate);
            thread::sleep(OWED84_ENCODED_SETTLE);
            swap_when_asked(&mut p, &lines, 1, &file_target(&dir.file("a.flv")));
            let end = due + gop + OWED84_MARGIN_TICKS;
            pace(&mut p, swap..end, slate);
            drain_and_wait(&mut p, &rx);
            let d = decode(&dir.file("a.flv"));
            assert!(d.first_is_key(), "{case}");
            let n = d.frames.len() as u64;
            let owed = end - due;
            eprintln!(
                "{case}: {n} frames for the {owed} from the floor; keyframes at {:?}",
                key_indexes(&d)
            );
            assert!(
                n <= owed,
                "{case}: {n} frames for the {owed} from the floor: it started before the floor"
            );
            assert!(
                n + OWED84_KEY_WAIT_TICKS >= owed,
                "{case}: {n} frames: the keyframe came more than {OWED84_KEY_WAIT_TICKS} ticks late"
            );
            assert_eq!(key_indexes(&d), vec![0, gop], "{case}");
            let key_ms: Vec<u64> = d.parsed.iter().filter(|f| f.1).map(|f| f.0).collect();
            for w in key_ms.windows(2) {
                assert!(
                    w[1] - w[0] >= FORCED_KEYFRAME_MIN_INTERVAL_MS,
                    "{case}: a {} ms GOP",
                    w[1] - w[0]
                );
                intervals += 1;
            }
            cases += 1;
        }
    }
    eprintln!("asked after the GOP's keyframe: cases {cases}, key intervals checked {intervals}");
    assert_eq!(cases, 4);
    assert!(intervals >= cases, "intervals checked: {intervals}");
}

#[test]
fn a_slate_then_back_to_camera_and_back_again() {
    let _s = serial();
    let dir = Tmp::new("slate-camera");
    let (mut p, rx, frames, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    frames.set_camera(solid(gst_video::VideoFormat::I420, 1_280, 720, [255, 0, 0, 255]));
    pace_with(
        &mut p,
        0..60,
        |i| {
            if (20..40).contains(&i) {
                FrameChoice::Emit
            } else {
                FrameChoice::Slate(SlateKind::Reconnecting)
            }
        },
        |_| {},
    );
    drain_and_wait(&mut p, &rx);
    let d = decode(&dir.file("a.flv"));
    assert_eq!(d.frames.len(), 60);
    let mut checked = 0;
    for (i, f) in d.frames.iter().enumerate() {
        if (20..40).contains(&i) {
            assert!(is_red(f.corner_tl) && is_red(f.centre), "frame {i}: camera {f:?}");
        } else {
            assert!(
                close(f.corner_tl, HOUSE_NIGHT, ON_AIR_TOL),
                "frame {i}: slate {f:?}"
            );
        }
        checked += 1;
    }
    eprintln!("frames checked: {checked}");
    assert_eq!(checked, 60);
}

#[test]
fn emit_with_no_camera_frame_pushes_the_slate_and_counts_it() {
    let _s = serial();
    let dir = Tmp::new("no-camera");
    let (mut p, rx, _f, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..10, FrameChoice::Emit);
    drain_and_wait(&mut p, &rx);
    assert_eq!(p.camera_missing(), 10);
    let d = decode(&dir.file("a.flv"));
    assert_eq!(d.frames.len(), 10);
    assert!(close(d.frames[9].corner_tl, HOUSE_NIGHT, ON_AIR_TOL));
}

#[test]
fn a_second_start_is_refused_and_nothing_is_rebuilt() {
    let _s = serial();
    let (mut p, _rx, _f, _l) = programme();
    assert_eq!(p.start(&P720), Err("programme already started".to_owned()));
    assert_eq!(
        p.start(&OutputProfile::P1080),
        Err("programme already started".to_owned())
    );
    assert_eq!(p.builds(), 1);
}

#[test]
fn a_profile_whose_ticks_do_not_divide_the_audio_is_refused() {
    let _s = serial();
    gst::init().expect("gstreamer");
    for fps in [0, 7] {
        let (tx, _rx) = crossbeam_channel::bounded(16);
        let mut p = GstProgramme::new(FrameStore::default(), tx);
        let profile = OutputProfile { fps, ..P720 };
        assert!(p.start(&profile).is_err(), "fps {fps}");
        assert_eq!(p.builds(), 0);
    }
}

#[test]
fn calls_before_start_are_refused_without_panic() {
    let _s = serial();
    gst::init().expect("gstreamer");
    let (tx, rx) = crossbeam_channel::bounded(16);
    let mut p = GstProgramme::new(FrameStore::default(), tx);
    p.push(0, FrameChoice::Emit);
    assert_eq!(p.refused_pushes(), 1);
    p.swap_sink(4, &file_target(Path::new("/nonexistent/x.flv")));
    assert_eq!(
        events_until(&rx, Duration::from_secs(1), |_| true),
        vec![Event::OutputError { gen: 4 }]
    );
    p.drain();
    p.drain();
    let seen = events_for(&rx, Duration::from_millis(300));
    assert_eq!(
        seen,
        vec![Event::OutputDrained],
        "drained once, with nothing to drain"
    );
}

#[test]
fn a_drain_with_no_sink_reports_drained_once() {
    let _s = serial();
    let (mut p, rx, _f, _l) = programme();
    pace(&mut p, 0..15, FrameChoice::Slate(SlateKind::Waiting));
    p.drain();
    p.drain();
    let mut seen = events_until(&rx, Duration::from_secs(10), |e| *e == Event::OutputDrained);
    seen.extend(events_for(&rx, Duration::from_millis(500)));
    assert_eq!(
        seen.iter().filter(|e| **e == Event::OutputDrained).count(),
        1,
        "{seen:?}"
    );
}

#[test]
fn an_index_that_does_not_advance_is_refused() {
    let _s = serial();
    let (mut p, _rx, _f, _l) = programme();
    p.push(5, FrameChoice::Slate(SlateKind::Waiting));
    p.push(5, FrameChoice::Slate(SlateKind::Waiting));
    p.push(4, FrameChoice::Slate(SlateKind::Waiting));
    p.push(9, FrameChoice::Slate(SlateKind::Waiting));
    assert_eq!(p.refused_pushes(), 2);
    assert_eq!(p.overlay_pushes(), 2);
}

#[test]
fn a_target_with_an_unknown_scheme_or_no_key_is_an_output_error() {
    let _s = serial();
    let (mut p, rx, _f, lines) = programme();
    p.swap_sink(
        1,
        &TargetSpec {
            url: "srt://127.0.0.1:9".to_owned(),
            stream_key: "SECRETKEY-srt-0001".to_owned(),
        },
    );
    p.swap_sink(
        2,
        &TargetSpec {
            url: "rtmps://127.0.0.1:9/live/".to_owned(),
            stream_key: String::new(),
        },
    );
    let seen = events_for(&rx, Duration::from_millis(500));
    let errors: Vec<&Event> = seen
        .iter()
        .filter(|e| matches!(e, Event::OutputError { .. }))
        .collect();
    assert_eq!(
        errors,
        vec![&Event::OutputError { gen: 1 }, &Event::OutputError { gen: 2 }]
    );
    // Both are refused before any sink is built, each with its own reason.
    let lines = lines.lock().unwrap().clone();
    for (gen, reason) in [
        (1, "output target scheme is not rtmp, rtmps or file"),
        (2, "output target has no stream key"),
    ] {
        let found = lines
            .iter()
            .any(|l| l.starts_with(&format!("swap_sink gen={gen}: {reason}")));
        assert!(found, "gen {gen}: {reason:?} not in {lines:?}");
    }
    assert!(
        !lines.iter().any(|l| l.contains(": sink ")),
        "a sink was built: {lines:?}"
    );
}

#[test]
fn the_1080p_programme_keeps_its_own_caps() {
    let _s = serial();
    let dir = Tmp::new("p1080");
    let profile = OutputProfile::P1080;
    let (mut p, rx, frames, _l) = programme_with(&profile);
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    frames.set_overlay(overlay_blue_corner());
    frames.set_camera(solid(gst_video::VideoFormat::I420, 1_280, 720, [255, 0, 0, 255]));
    pace_with(
        &mut p,
        0..30,
        |i| {
            if i < 15 {
                FrameChoice::Slate(SlateKind::Final)
            } else {
                FrameChoice::Emit
            }
        },
        |_| {},
    );
    drain_and_wait(&mut p, &rx);
    let d = decode(&dir.file("a.flv"));
    assert_eq!(d.frames.len(), 30);
    assert!(d.frames.iter().all(|f| (f.w, f.h) == (1_920, 1_080)));
    assert!(
        close(d.frames[14].corner_br, HOUSE_NIGHT, ON_AIR_TOL),
        "{:?}",
        d.frames[14]
    );
    assert!(is_blue(d.frames[14].spot) && is_blue(d.frames[29].spot));
    assert!(is_red(d.frames[29].centre), "{:?}", d.frames[29]);
}

// ---------------------------------------------------------------------------------------------
// The adapter's own guards (each mutated once: the PR lists them)

#[test]
fn a_programme_error_is_fatal_once_and_its_text_is_scrubbed() {
    let _s = serial();
    let dir = Tmp::new("fatal");
    let (mut p, rx, _f, lines) = programme();
    let key = "SECRETKEY-fatal-0001";
    p.swap_sink(
        1,
        &TargetSpec {
            url: format!("file://{}", dir.file("a.flv").display()),
            stream_key: key.to_owned(),
        },
    );
    for _ in 0..2 {
        assert!(p.plant_programme_error(&format!("the element said {key}")));
    }
    let seen = events_for(&rx, Duration::from_millis(800));
    let fatals: Vec<&Event> = seen.iter().filter(|e| matches!(e, Event::Fatal { .. })).collect();
    assert_eq!(fatals.len(), 1, "{seen:?}");
    let Event::Fatal { detail: Some(detail) } = fatals[0] else {
        panic!("a fatal error with no detail: {fatals:?}");
    };
    assert!(detail.as_str().contains("planted programme error"), "{detail:?}");
    assert!(
        !detail.as_str().contains(key) && detail.as_str().contains("****"),
        "{detail:?}"
    );
    let lines = lines.lock().unwrap().clone();
    let logged = lines.iter().filter(|l| l.starts_with("programme error")).count();
    eprintln!("programme error lines: {logged}");
    assert_eq!(logged, 2, "{lines:?}");
    assert!(lines.iter().all(|l| !l.contains(key)), "{lines:?}");
}

#[test]
fn an_encoder_that_stalls_reports_encoder_overrun_and_the_programme_carries_on() {
    let _s = serial();
    let dir = Tmp::new("overrun");
    let (mut p, rx, _f, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..15, FrameChoice::Slate(SlateKind::Waiting));
    assert!(p.hang_encoder(Duration::from_millis(2_000)), "the encoder hook");
    pace(&mut p, 15..120, FrameChoice::Slate(SlateKind::Waiting));
    let seen = drain_and_wait(&mut p, &rx);
    let overruns = seen.iter().filter(|e| **e == Event::EncoderOverrun).count();
    eprintln!("encoder overruns read: {overruns}");
    assert!(overruns >= 1, "{seen:?}");
    let d = decode(&dir.file("a.flv"));
    eprintln!("frames after a 2 s encoder stall: {} of 120", d.frames.len());
    // The leaky queue dropped what the stalled encoder could not take; the programme went on.
    assert!(
        d.frames.len() < 120,
        "nothing was dropped, so the stall was not real"
    );
    assert!(d.frames.len() >= 60, "{}", d.frames.len());
    assert_eq!(p.builds(), 1);
}

/// The bound a held sink leaves on the output's two appsrcs: the feeder's own bound plus the one
/// entry it may push past it.
fn queue_bound() -> u64 {
    2 * OUTPUT_QUEUE_BYTES
}

/// The entries 2 s of encoded output holds: 30 video frames a second at 30 fps and 48 000 / 1 024
/// AAC frames a second, with a tenth of a second of slack.
fn ring_bound() -> usize {
    let per_second = f64::from(P720.fps) + 48_000.0 / 1_024.0;
    (per_second * (ENCODED_RING_SPAN.as_secs_f64() + 0.1)).ceil() as usize
}

#[test]
fn a_held_sink_backs_up_into_the_bounded_ring_not_the_output() {
    let _s = serial();
    let dir = Tmp::new("held");
    let (mut p, _rx, _f, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..15, FrameChoice::Slate(SlateKind::Waiting));
    assert!(p.hold_sink_writes(), "the hook found the sink");
    let (mut queued, mut ring, mut samples) = (0u64, 0usize, 0);
    for second in 0..5u64 {
        let from = 15 + second * 30;
        pace(&mut p, from..from + 30, FrameChoice::Slate(SlateKind::Waiting));
        let (v, a) = p.output_queued_bytes().expect("an output");
        queued = queued.max(v.max(a));
        ring = ring.max(p.ring_entries());
        samples += 1;
        assert!(
            v <= queue_bound() && a <= queue_bound(),
            "second {second}: queued {v} / {a}"
        );
        assert!(
            p.ring_entries() <= ring_bound().min(ENCODED_RING_MAX_ENTRIES),
            "{}",
            p.ring_entries()
        );
    }
    eprintln!(
        "held-sink samples: {samples}; most queued {queued} B (bound {}), ring at most {ring} (bound {})",
        queue_bound(),
        ring_bound()
    );
    assert_eq!(samples, 5);
    assert!(
        queued >= OUTPUT_QUEUE_BYTES,
        "the output never filled to the feeder's bound"
    );
    assert!(ring * 10 >= ring_bound() * 8, "the ring never filled: {ring}");
}

/// Owed 94 (the owner, 2026-10-24, "B"), the seam through its real producer and consumer. The hook
/// that holds the sink's writes (the hang shape the backstop exists for) leaves the sink's total flat
/// while both encoders' totals keep growing on every reading; and those very readings, folded through
/// the domain's `OutputLink` at the instants they arrived, back the sink off as `EgressStalled` at
/// `output_egress_flat_ms` after its last growth and not 1 ms before. The clock is time (P-6), so
/// the readings need not run the whole threshold: two seconds of the hang show its shape.
#[test]
fn a_held_sink_reads_flat_while_the_encoders_deliver_and_the_link_backs_it_off() {
    let _s = serial();
    let dir = Tmp::new("held-backstop");
    let (mut p, rx, _f, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    let t0 = Instant::now();
    let mut readings: Vec<(Ms, u64, Encoded)> = Vec::new();
    let take = |readings: &mut Vec<(Ms, u64, Encoded)>| {
        while let Ok(m) = rx.try_recv() {
            if let Inbound::Event(Event::OutputProgress {
                gen: 1,
                total_bytes,
                encoded,
            }) = m
            {
                readings.push((t0.elapsed().as_millis() as Ms, total_bytes, encoded));
            }
        }
    };
    let fps = u64::from(P720.fps);
    pace_with(
        &mut p,
        0..fps,
        |_| FrameChoice::Slate(SlateKind::Waiting),
        |_| take(&mut readings),
    );
    assert!(p.hold_sink_writes(), "the hook found the sink");
    let held_at = t0.elapsed().as_millis() as Ms;
    pace_with(
        &mut p,
        fps..3 * fps,
        |_| FrameChoice::Slate(SlateKind::Waiting),
        |_| take(&mut readings),
    );
    take(&mut readings);
    // The adapter's half: the sink's total stops at the hold, and the encoders keep delivering.
    let high = readings.iter().map(|r| r.1).max().expect("readings");
    let last = readings.iter().find(|r| r.1 == high).expect("the high").0;
    let after: Vec<_> = readings.iter().filter(|r| r.0 > last).collect();
    let grew = after
        .windows(2)
        .filter(|w| w[1].2.video > w[0].2.video && w[1].2.audio > w[0].2.audio)
        .count();
    eprintln!(
        "held-sink readings: {} ({} after the last growth at {last} ms, hold at {held_at} ms; {grew} \
         pairs with both encoders grown)",
        readings.len(),
        after.len()
    );
    assert!(after.len() >= 5, "{} readings after the last growth", after.len());
    assert!(
        after.iter().all(|r| r.1 == high),
        "the total moved after its high: {after:?}"
    );
    assert_eq!(
        grew,
        after.len() - 1,
        "an encoder stopped behind the held sink: {after:?}"
    );
    // The domain's half, on the same readings.
    let t = Thresholds::default();
    let mut link = OutputLink::new();
    assert!(link.on_tick(0, &t));
    for &(at, total, encoded) in &readings {
        link.on_progress(at, 1, total, &t);
        link.on_delivery(at, 1, encoded);
        assert!(!link.on_tick(at, &t), "built at {at} ms");
    }
    let flat = t.output_egress_flat_ms;
    assert!(!link.on_tick(last + flat - 1, &t), "1 ms early");
    assert!(
        matches!(link.state(), OutputState::Proving { .. }),
        "{:?}",
        link.state()
    );
    link.on_tick(last + flat, &t);
    let OutputState::Backoff { attempt: 1, until } = link.state() else {
        panic!("the backstop did not back off: {:?}", link.state());
    };
    assert!(link.on_tick(until, &t));
    assert_eq!(link.swap_reason(), SwapReason::EgressStalled);
}

/// Owed 94 (the PR #73 review's P-5): the backstop's swap away from a sink whose teardown hangs
/// (F-001: `set_state(NULL)` held by a write sleeping on its streaming lock) returns at once, and the
/// new sink's bytes flow while the old teardown is still held by the reaper.
#[test]
fn a_swap_away_from_a_hung_sink_does_not_wait_for_its_teardown() {
    let _s = serial();
    let dir = Tmp::new("swap-hung");
    let (mut p, rx, _f, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..15, FrameChoice::Slate(SlateKind::Waiting));
    assert!(p.hang_sink(Duration::from_secs(3)), "the sink hook");
    pace(&mut p, 15..20, FrameChoice::Slate(SlateKind::Waiting));
    thread::sleep(Duration::from_millis(300));
    let teardowns = p.teardown_counter();
    let t = Instant::now();
    p.swap_sink(2, &file_target(&dir.file("b.flv")));
    let took = t.elapsed();
    let in_flight = teardowns.load(Ordering::SeqCst);
    // Sink 2's keyframe is held to the floor after frame 0's (F-036); 45 ticks on from it.
    pace(
        &mut p,
        20..key_due(20, 0) + 45,
        FrameChoice::Slate(SlateKind::Waiting),
    );
    let grown = progress_of(&events_for(&rx, Duration::from_millis(600)), 2);
    eprintln!(
        "swap took {took:?}; teardowns in flight after it: {in_flight}; gen-2 totals read: {}",
        grown.len()
    );
    assert!(took < Duration::from_millis(500), "{took:?}");
    assert_eq!(
        in_flight, 1,
        "the old sink's teardown is still held, so the hang was real"
    );
    assert!(
        grown.iter().any(|&b| b > 0),
        "the new sink sent nothing: {grown:?}"
    );
}

#[test]
fn a_sink_that_stalls_past_the_ring_resumes_at_a_keyframe() {
    let _s = serial();
    let dir = Tmp::new("stall-recover");
    let (mut p, rx, _f, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..30, FrameChoice::Slate(SlateKind::Waiting));
    // 5 s: the output fills (about 1 s), then the ring's 2 s fill and drop their oldest.
    assert!(p.hang_sink(Duration::from_secs(5)), "the hook found the sink");
    pace(&mut p, 30..300, FrameChoice::Slate(SlateKind::Waiting));
    drain_and_wait(&mut p, &rx);
    let d = decode(&dir.file("a.flv"));
    let step = (1_000.0 / f64::from(P720.fps)).round() as u64;
    let mut gaps = 0;
    for w in d.parsed.windows(2) {
        let ((before, _), (after, key)) = (w[0], w[1]);
        if after > before + step + 1 {
            gaps += 1;
            assert!(
                key,
                "the frame at {after} ms after a gap from {before} ms is not a keyframe"
            );
        }
    }
    eprintln!("gaps checked: {gaps}; frames parsed: {}", d.parsed.len());
    assert!(gaps >= 1, "the stall never outlasted the ring");
    assert!(d.first_is_key());
}

#[test]
fn a_camera_frame_that_cannot_be_fitted_shows_the_slate_and_is_logged_once() {
    let _s = serial();
    let dir = Tmp::new("unfit");
    let (mut p, rx, frames, lines) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    // Caps that promise a 640 × 360 I420 frame, and a buffer far too small to be one.
    let caps = gst_video::VideoInfo::builder(gst_video::VideoFormat::I420, 640, 360)
        .build()
        .expect("info")
        .to_caps()
        .expect("caps");
    let short = gst::Sample::builder()
        .buffer(&gst::Buffer::from_mut_slice(vec![0u8; 16]))
        .caps(&caps)
        .build();
    frames.set_camera(short);
    pace(&mut p, 0..10, FrameChoice::Emit);
    // A good frame afterwards is fitted as usual.
    frames.set_camera(solid(gst_video::VideoFormat::I420, 640, 360, [255, 0, 0, 255]));
    pace(&mut p, 10..20, FrameChoice::Emit);
    drain_and_wait(&mut p, &rx);
    assert_eq!(p.camera_missing(), 10);
    let lines = lines.lock().unwrap().clone();
    let logged = lines.iter().filter(|l| l.contains("cannot be fitted")).count();
    eprintln!("unfit camera ticks: 10, lines logged: {logged}");
    assert_eq!(logged, 1, "{lines:?}");
    let d = decode(&dir.file("a.flv"));
    assert_eq!(d.frames.len(), 20);
    assert!(
        close(d.frames[9].corner_tl, HOUSE_NIGHT, ON_AIR_TOL),
        "{:?}",
        d.frames[9]
    );
    assert!(is_red(d.frames[19].centre), "{:?}", d.frames[19]);
}

#[test]
fn an_output_error_is_reported_once_and_its_progress_stops() {
    let _s = serial();
    let dir = Tmp::new("output-error");
    let (mut p, rx, _f, _l) = programme();
    p.swap_sink(5, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..15, FrameChoice::Slate(SlateKind::Waiting));
    for _ in 0..2 {
        assert!(p.plant_output_error(), "the hook found the output");
    }
    let seen = events_until(&rx, Duration::from_secs(2), |e| {
        matches!(e, Event::OutputError { .. })
    });
    assert_eq!(seen.last(), Some(&Event::OutputError { gen: 5 }));
    pace(&mut p, 15..45, FrameChoice::Slate(SlateKind::Waiting));
    let after = events_for(&rx, Duration::from_millis(500));
    let errors = after
        .iter()
        .filter(|e| matches!(e, Event::OutputError { .. }))
        .count();
    let progress = progress_of(&after, 5).len();
    eprintln!("after the error: {errors} more errors, {progress} progress events");
    assert_eq!((errors, progress), (0, 0), "{after:?}");
}

#[test]
fn a_drain_waits_for_the_sink_to_take_everything() {
    let _s = serial();
    let dir = Tmp::new("drain-held");
    let (mut p, rx, _f, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..30, FrameChoice::Slate(SlateKind::Waiting));
    assert!(p.hold_sink_writes(), "the hook found the sink");
    p.drain();
    // The encoders reach EOS at once; the sink, holding its writes, never does. Drained is the
    // sink's to say (the domain's drain_timeout_ms is the bound on waiting for it).
    let seen = events_for(&rx, Duration::from_millis(1_500));
    let progress = progress_of(&seen, 1).len();
    eprintln!(
        "events while the held sink drains: {}, progress among them: {progress}",
        seen.len()
    );
    assert!(progress > 0, "the watch was running");
    assert!(!seen.contains(&Event::OutputDrained), "{seen:?}");
}

// ---------------------------------------------------------------------------------------------
// PR #28 review, round 1

/// Review item 1: a swap that lands just before the retired output's next poll. The retired watch's
/// last reading lands after the new sink has started counting; it must change nothing. Egress never
/// exceeds what is on disk (each sink's total is a file's size at some earlier instant), and once the
/// new sink has one complete window its bitrate is a real rate, not the 0 a stale higher total left.
#[test]
fn egress_and_bitrate_survive_a_swap_just_before_the_old_poll() {
    let _s = serial();
    let dir = Tmp::new("swap-before-poll");
    let (mut p, _rx, _f, lines) = programme();
    let stats = p.stats();
    let step = period(&P720);
    let poll = Duration::from_millis(OUTPUT_PROGRESS_POLL_MS);
    // F-036: a swap's keyframe is held to the floor after the encoder's last one, and this test needs
    // each new sink sending at once. The first sink is swapped in before the first frame, so nothing
    // is asked and frame 0 is its keyframe (F-036, fact 8); it lives past the floor after the GOP's
    // keyframe and short of the next GOP's. Each later one lives past the floor and short of a GOP.
    // So every swap's keyframe is due at its swap, and each swap still lands 25 ms before a poll.
    // "Past" by three frames: a swap's keyframe can come a frame after its swap, so a sink that
    // lived the floor and 25 ms (`floor / poll + 1` polls at the 2 200 ms floor) would see its
    // successor's keyframe held a frame.
    let lead = Duration::from_millis(25);
    let (gop_ms, floor_ms, poll_ms) = (
        PROGRAMME_GOP_MS,
        FORCED_KEYFRAME_MIN_INTERVAL_MS,
        OUTPUT_PROGRESS_POLL_MS,
    );
    let slack_ms = 3 * step.as_millis() as u64;
    let first_polls = (gop_ms + floor_ms) / poll_ms + 2;
    let each_polls = floor_ms / poll_ms + 2;
    let ms = |polls: u64| polls * poll_ms - lead.as_millis() as u64;
    assert!(
        (gop_ms + floor_ms + slack_ms..2 * gop_ms).contains(&ms(first_polls))
            && (floor_ms + slack_ms..gop_ms).contains(&ms(each_polls)),
        "setup: {} and {} ms",
        ms(first_polls),
        ms(each_polls)
    );
    swap_before_the_first_frame(&mut p, &lines, 1, &file_target(&dir.file("g1.flv")));
    let mut built = Instant::now();
    let t0 = Instant::now();
    let mut i = 0u64;
    let push_until = |p: &mut GstProgramme, until: Instant, i: &mut u64| {
        while Instant::now() < until {
            let due = t0 + step * (*i as u32);
            if let Some(w) = due.checked_duration_since(Instant::now()) {
                thread::sleep(w.min(until.saturating_duration_since(Instant::now())));
            }
            if Instant::now() >= due {
                p.push(*i, FrameChoice::Slate(SlateKind::Waiting));
                *i += 1;
            }
        }
    };
    let on_disk = |n: u64| -> u64 {
        (1..=n)
            .map(|g| {
                std::fs::metadata(dir.file(&format!("g{g}.flv")))
                    .map(|m| m.len())
                    .unwrap_or(0)
            })
            .sum()
    };
    // The first sink lives over 5 s, so a stale reading of it is large.
    push_until(&mut p, built + poll * first_polls as u32 - lead, &mut i);
    let mut checked = 0;
    for gen in 2..=7u64 {
        p.swap_sink(gen, &file_target(&dir.file(&format!("g{gen}.flv"))));
        built = Instant::now();
        // 120 ms on, the retired watch's poll has landed (it was due 25 ms after the swap).
        push_until(&mut p, built + Duration::from_millis(120), &mut i);
        let egress = stats.snapshot().egress_bytes.unwrap_or(0);
        let disk = on_disk(gen);
        // 1.6 s on, the new sink's first window (from its first poll at 250 ms) is complete.
        push_until(&mut p, built + Duration::from_millis(1_600), &mut i);
        let later = stats.snapshot();
        let disk_later = on_disk(gen);
        eprintln!(
            "gen {gen}: +120 ms egress {egress} on disk {disk}; +1.6 s egress {:?} on disk {disk_later}, bitrate {:?}",
            later.egress_bytes, later.bitrate_kbps
        );
        assert!(
            egress <= disk,
            "gen {gen}: egress {egress} over the {disk} bytes on disk"
        );
        assert!(
            later.egress_bytes.unwrap_or(0) <= disk_later,
            "gen {gen}: {later:?} over {disk_later}"
        );
        // A healthy 720p sink sends at least a quarter of its video rate.
        let floor = P720.video_kbps / 4;
        assert!(
            later.bitrate_kbps.is_some_and(|b| b >= floor),
            "gen {gen}: bitrate {:?}, floor {floor}",
            later.bitrate_kbps
        );
        checked += 1;
        // The next swap lands 25 ms before one of this sink's watch's polls.
        push_until(&mut p, built + poll * each_polls as u32 - lead, &mut i);
    }
    eprintln!("swaps timed before a poll checked: {checked}");
    assert_eq!(checked, 6);
}

/// Review item 2 (ruling: `try_send` and a drop counter): the driver's thread is the inbound
/// channel's only reader, so a call on it that waits for room never returns. A refused swap (before
/// `start`, or a scheme the adapter refuses) and a drain with nothing started each try once and
/// count the drop. The lost `OutputError` costs nothing lasting: the domain's link times the connect
/// out and swaps again.
#[test]
fn refused_calls_never_block_on_a_full_inbound_channel() {
    let _s = serial();
    gst::init().expect("gstreamer");
    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(Inbound::Event(Event::Tick)).expect("fill the channel");
    let lines: Lines = Arc::default();
    let (done_tx, done_rx) = crossbeam_channel::bounded(1);
    let worker = thread::spawn({
        let lines = Arc::clone(&lines);
        move || {
            let log = move |l: &str| {
                lines
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(l.to_owned())
            };
            let log: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(log);
            let refused = TargetSpec {
                url: "srt://127.0.0.1:9".to_owned(),
                stream_key: "SECRETKEY-full-channel".to_owned(),
            };
            // Never started: a swap and a drain.
            let mut idle = GstProgramme::new(FrameStore::default(), tx.clone()).with_log(Arc::clone(&log));
            idle.swap_sink(1, &file_target(Path::new("/tmp/never.flv")));
            idle.drain();
            // Started: a scheme the adapter refuses.
            let mut started = GstProgramme::new(FrameStore::default(), tx).with_log(log);
            started.start(&P720).expect("start");
            started.swap_sink(2, &refused);
            let _ = done_tx.send((idle.events_dropped(), started.events_dropped()));
        }
    });
    let returned = done_rx.recv_timeout(Duration::from_secs(3));
    // Unblock a blocked worker either way, so the test process can end.
    let mut seen = Vec::new();
    while let Ok(m) = rx.recv_timeout(Duration::from_millis(200)) {
        if let Inbound::Event(e) = m {
            seen.push(e);
        }
    }
    let _ = worker.join();
    let (idle, started) = returned.expect("the calls returned within 3 s with the channel full");
    let lines = lines.lock().unwrap_or_else(PoisonError::into_inner).clone();
    let logged = lines.iter().filter(|l| l.contains("not delivered")).count();
    eprintln!("dropped: idle {idle}, started {started}; lines naming the drop {logged}; delivered {seen:?}");
    assert_eq!((idle, started), (2, 1));
    assert_eq!(logged, 3, "{lines:?}");
    assert_eq!(seen, vec![Event::Tick], "only what was there before");
    assert!(lines.iter().all(|l| !l.contains("SECRETKEY")), "{lines:?}");
}

/// Review item 3: a sink that dies mid-stream and is not swapped (the domain is in its back-off, or
/// in the hold), then the session drains. Nothing more can reach the dead sink, so the drain ends
/// when the encoders do, well inside the domain's `drain_timeout_ms`.
#[test]
fn a_failed_sink_left_in_place_still_drains() {
    let _s = serial();
    let dir = Tmp::new("failed-in-place");
    let (mut p, rx, _f, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..15, FrameChoice::Slate(SlateKind::Waiting));
    assert!(p.fail_sink_writes());
    pace(&mut p, 15..30, FrameChoice::Slate(SlateKind::Waiting));
    events_until(&rx, Duration::from_secs(2), |e| {
        *e == Event::OutputError { gen: 1 }
    });
    let t = Instant::now();
    p.drain();
    let seen = events_for(&rx, Duration::from_millis(2_000));
    let drained = seen.iter().filter(|e| **e == Event::OutputDrained).count();
    let bound = Thresholds::default().drain_timeout_ms;
    eprintln!(
        "drained events {drained} within 2 s of the drain (took {:?}); the domain waits {bound} ms",
        t.elapsed()
    );
    assert_eq!(drained, 1, "{seen:?}");
}

/// Review item 3, the other order: the sink stops taking writes during the drain (the encoders have
/// reached EOS, and the sink never will), then errors, as rtmp2sink does when its write times out.
/// The error ends the drain: nothing more can reach that sink.
#[test]
fn a_sink_that_fails_during_the_drain_ends_it() {
    let _s = serial();
    let dir = Tmp::new("fail-in-drain");
    let (mut p, rx, _f, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..30, FrameChoice::Slate(SlateKind::Waiting));
    assert!(p.hold_sink_writes());
    p.drain();
    let before = events_for(&rx, Duration::from_millis(1_000));
    assert!(!before.contains(&Event::OutputDrained), "{before:?}");
    assert!(p.plant_output_error());
    let after = events_for(&rx, Duration::from_millis(1_000));
    let drained = after.iter().filter(|e| **e == Event::OutputDrained).count();
    eprintln!("held drain, then the sink's error: {after:?}");
    assert!(after.contains(&Event::OutputError { gen: 1 }), "{after:?}");
    assert_eq!(drained, 1, "{after:?}");
}

/// The ring's gap rule, deterministically (the reviewer's M4 survived a quiet machine: whether the
/// stall test's natural overflow leaves a delta frame at the ring's front is timing). While the sink
/// hangs and the feeder holds off, the ring loses its oldest entries up to a delta frame; when the
/// sink resumes, the output's next picture after the gap is a keyframe, never that delta.
#[test]
fn after_the_ring_drops_entries_the_output_resumes_at_a_keyframe() {
    let _s = serial();
    let dir = Tmp::new("ring-gap");
    let (mut p, rx, _f, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..30, FrameChoice::Slate(SlateKind::Waiting));
    assert!(p.hang_sink(Duration::from_secs(4)), "the hook found the sink");
    // The output's queue fills (`OUTPUT_QUEUE_BYTES`, about 1.4 s) and the feeder holds off; the
    // ring then holds about a second.
    pace(&mut p, 30..105, FrameChoice::Slate(SlateKind::Waiting));
    let held = p.ring_entries();
    let dropped = p.drop_ring_to_delta();
    pace(&mut p, 105..240, FrameChoice::Slate(SlateKind::Waiting));
    drain_and_wait(&mut p, &rx);
    let d = decode(&dir.file("a.flv"));
    let step = (1_000.0 / f64::from(P720.fps)).round() as u64;
    let gaps: Vec<(u64, u64, bool)> = d
        .parsed
        .windows(2)
        .filter(|w| w[1].0 > w[0].0 + step + 1)
        .map(|w| (w[0].0, w[1].0, w[1].1))
        .collect();
    eprintln!("ring held {held}, dropped {dropped} up to a delta; gaps (before ms, after ms, key) {gaps:?}; frames parsed {}", d.parsed.len());
    assert!(dropped >= 1 && held > dropped, "held {held}, dropped {dropped}");
    assert!(!gaps.is_empty(), "no gap reached the file");
    assert!(gaps.iter().all(|g| g.2), "{gaps:?}");
    assert!(d.first_is_key());
}

/// Review item 4 (ruling: keep the rebase, witnessed on the appsrc side): flvmux starts its own
/// timestamps at 0, so the file cannot tell a rebased output from one that is not. What the feeder
/// sends into the output's appsrcs can: a second sink, built 2.2 s into the programme, starts its video
/// at exactly 0 and its audio within one AAC frame of it.
#[test]
fn each_output_rebases_its_first_timestamps_to_zero() {
    let _s = serial();
    let dir = Tmp::new("rebase");
    let (mut p, rx, _f, lines) = programme();
    // Swapped in before the first frame, so frame 0 is the last keyframe and the floor has passed at
    // the second swap (F-036).
    swap_before_the_first_frame(&mut p, &lines, 1, &file_target(&dir.file("a.flv")));
    let second = floor_ticks();
    pace(&mut p, 0..second, FrameChoice::Slate(SlateKind::Waiting));
    p.swap_sink(2, &file_target(&dir.file("b.flv")));
    pace(
        &mut p,
        second..second + 30,
        FrameChoice::Slate(SlateKind::Waiting),
    );
    let (video, audio) = p.output_first_pts().expect("an output");
    drain_and_wait(&mut p, &rx);
    eprintln!("second sink's first PTS: video {video:?}, audio {audio:?} (built at tick {second})");
    assert_eq!(video, Some(gst::ClockTime::ZERO));
    let aac_frame = gst::ClockTime::from_nseconds(1_024 * 1_000_000_000 / 48_000);
    assert!(audio.is_some_and(|a| a <= aac_frame), "{audio:?}");
}

/// Review item 6 (ruling: the fitter holds the input buffer it cached by): a camera's buffer pool
/// recycles a buffer as soon as nothing holds it, so the next frame can arrive in the very same
/// `GstBuffer` with new pixels. Keyed by address alone, the fitter served the old frame's conversion.
#[test]
fn a_recycled_camera_buffer_is_converted_again() {
    let _s = serial();
    let dir = Tmp::new("pool-recycle");
    let (mut p, rx, frames, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    let i420 = gst_video::VideoFormat::I420;
    // A shape the programme does not take (640×360), so every frame goes through the fitter.
    let info = gst_video::VideoInfo::builder(i420, 640, 360)
        .build()
        .expect("info");
    let caps = info.to_caps().expect("caps");
    let pool = gst::BufferPool::new();
    let mut config = pool.config();
    config.set_params(Some(&caps), info.size() as u32, 1, 2);
    pool.set_config(config).expect("pool config");
    pool.set_active(true).expect("pool active");
    let from_pool = |rgba: [u8; 4]| -> (gst::Sample, usize) {
        let pixels = solid(i420, 640, 360, rgba);
        let bytes = pixels
            .buffer()
            .expect("buffer")
            .map_readable()
            .expect("map")
            .to_vec();
        let mut b = pool.acquire_buffer(None).expect("a pool buffer");
        b.get_mut()
            .expect("writable")
            .copy_from_slice(0, &bytes)
            .expect("fill");
        let at = b.as_ptr() as usize;
        (gst::Sample::builder().buffer(&b).caps(&caps).build(), at)
    };
    // The programme's own shape skips the fitter; it releases the pool buffer from the store.
    let native = solid(i420, P720.width, P720.height, [0, 255, 0, 255]);
    let colours = [[255u8, 0, 0, 255], [0, 0, 255, 255]];
    let mut addresses = Vec::new();
    let mut want = Vec::new();
    let step = period(&P720);
    let t0 = Instant::now();
    for i in 0..20u64 {
        if let Some(w) = (t0 + step * i as u32).checked_duration_since(Instant::now()) {
            thread::sleep(w);
        }
        frames.set_camera(native.clone());
        let c = colours[(i % 2) as usize];
        let (sample, at) = from_pool(c);
        addresses.push(at);
        frames.set_camera(sample);
        want.push(i % 2);
        p.push(i, FrameChoice::Emit);
    }
    drain_and_wait(&mut p, &rx);
    pool.set_active(false).expect("pool inactive");
    let d = decode(&dir.file("a.flv"));
    assert_eq!(d.frames.len(), 20);
    let wrong: Vec<(usize, u64)> = d
        .frames
        .iter()
        .enumerate()
        .filter(|(i, f)| {
            if want[*i] == 0 {
                !is_red(f.centre)
            } else {
                !is_blue(f.centre)
            }
        })
        .map(|(i, _)| (i, want[i]))
        .collect();
    let reused = addresses.windows(2).filter(|w| w[0] == w[1]).count();
    eprintln!(
        "pool frames checked: {}, consecutive frames in a reused buffer: {reused}, wrong: {wrong:?}",
        d.frames.len()
    );
    assert!(wrong.is_empty(), "{wrong:?}");
}

/// Review item 7: progress the poll could not deliver is counted and said, once when the channel
/// fills and once, with the count, when it drains.
#[test]
fn progress_dropped_on_a_full_channel_is_counted_and_logged() {
    let _s = serial();
    let dir = Tmp::new("progress-dropped");
    gst::init().expect("gstreamer");
    let (tx, rx) = crossbeam_channel::bounded(1);
    tx.send(Inbound::Event(Event::Tick)).expect("fill the channel");
    let lines: Lines = Arc::default();
    let sink = Arc::clone(&lines);
    let mut p = GstProgramme::new(FrameStore::default(), tx).with_log(Arc::new(move |l: &str| {
        sink.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(l.to_owned());
    }));
    p.start(&P720).expect("start");
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..30, FrameChoice::Slate(SlateKind::Waiting));
    let dropped = p.progress_dropped();
    // Room again: the next poll is delivered, and says how many were not.
    let _ = rx.recv();
    let first = events_until(&rx, Duration::from_secs(2), |e| {
        matches!(e, Event::OutputProgress { .. })
    });
    // The count is logged just after that event is delivered, on the watch's thread: wait for it.
    let end = Instant::now() + Duration::from_secs(1);
    let lines_now = loop {
        let now = lines.lock().unwrap_or_else(PoisonError::into_inner).clone();
        if now.iter().any(|l| l.contains("progress events were dropped")) || Instant::now() > end {
            break now;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let full = lines_now
        .iter()
        .filter(|l| l.contains("inbound channel is full"))
        .count();
    let summed: Vec<&String> = lines_now
        .iter()
        .filter(|l| l.contains("progress events were dropped"))
        .collect();
    eprintln!("dropped {dropped} over 1 s; lines: full {full}, summed {summed:?}; first after {first:?}");
    assert!(dropped >= 3, "{dropped}");
    assert_eq!(full, 1, "{lines_now:?}");
    assert_eq!(summed.len(), 1, "{lines_now:?}");
    // The line's count is the streak's: at least what was dropped before the channel drained, and
    // no more than the counter now.
    let said: u64 = summed[0]
        .split(": ")
        .nth(1)
        .and_then(|t| t.split(' ').next())
        .and_then(|n| n.parse().ok())
        .expect("a count in the line");
    assert!(
        (dropped..=p.progress_dropped()).contains(&said),
        "{said} against {dropped}..={}",
        p.progress_dropped()
    );
    drain_and_wait(&mut p, &rx);
}

/// Review item 8 (brief Step 6: "push_buffer errors are logged, redacted and counted"): a layer whose
/// appsrc refuses buffers counts every refusal and says so once a run: once when it starts failing,
/// and once more only after it has recovered and fails again.
#[test]
fn a_refused_layer_push_is_counted_and_logged_once_a_run() {
    let _s = serial();
    let dir = Tmp::new("push-errors");
    let (mut p, _rx, _f, lines) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..10, FrameChoice::Slate(SlateKind::Waiting));
    assert_eq!(p.push_errors(), 0);
    let said = |lines: &Lines| -> Vec<String> {
        let l = lines.lock().unwrap_or_else(PoisonError::into_inner);
        l.iter().filter(|l| l.contains("push failed")).cloned().collect()
    };
    assert!(p.stop_base_layer());
    pace(&mut p, 10..20, FrameChoice::Slate(SlateKind::Waiting));
    let first_run = (p.push_errors(), said(&lines).len());
    assert!(p.restart_base_layer());
    pace(&mut p, 20..30, FrameChoice::Slate(SlateKind::Waiting));
    let recovered = (p.push_errors(), said(&lines).len());
    assert!(p.stop_base_layer());
    pace(&mut p, 30..35, FrameChoice::Slate(SlateKind::Waiting));
    let second_run = (p.push_errors(), said(&lines));
    eprintln!("(errors, lines): first run {first_run:?}, recovered {recovered:?}, second run {second_run:?}");
    assert_eq!(first_run, (10, 1));
    assert_eq!(recovered, (10, 1), "the restarted layer pushes again");
    assert_eq!(second_run.0, 15);
    assert_eq!(second_run.1.len(), 2, "{:?}", second_run.1);
    assert!(
        second_run.1.iter().all(|l| l.contains("base")),
        "{:?}",
        second_run.1
    );
}

/// Owed 40's bound: the ring holds the playout's cap of audio (C4: 500 ms, was 400), so a 1 s gap
/// fills its last 500 ms (15 periods at 30 fps) and keeps the rest as one hole, never a burst of a
/// whole second of filler.
#[test]
fn a_long_gap_fills_only_the_rings_span_of_audio() {
    let _s = serial();
    let dir = Tmp::new("long-gap");
    let (mut p, rx, _f, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    pace(&mut p, 0..30, FrameChoice::Slate(SlateKind::Waiting));
    thread::sleep(Duration::from_secs(1));
    pace(&mut p, 60..90, FrameChoice::Slate(SlateKind::Waiting));
    drain_and_wait(&mut p, &rx);
    let d = decode(&dir.file("a.flv"));
    let jumps: Vec<u64> = d
        .audio_pts_ms
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|&dt| dt > 22)
        .collect();
    // 30 periods missed, 15 filled (`AUDIO_RING_BYTES` over one period): a 15-period hole, 500 ms,
    // seen as one jump of the hole plus one AAC frame.
    let period_ms = 1_000 / u64::from(P720.fps);
    let fill = (AUDIO_RING_BYTES / (48_000 / P720.fps as usize * 4)) as u64;
    let hole_ms = (30 - fill) * 1_000 / u64::from(P720.fps);
    eprintln!("audio frames checked {}, jumps {jumps:?}; filled {fill} periods, hole {hole_ms} ms ({period_ms} ms a period)", d.audio_pts_ms.len());
    assert_eq!(
        fill,
        PLAYOUT_CAP_MS * u64::from(P720.fps) / 1_000,
        "the cap's periods"
    );
    assert_eq!(jumps.len(), 1, "{jumps:?}");
    assert!(
        jumps[0].abs_diff(hole_ms + 21) <= 22,
        "{jumps:?} against {hole_ms} + one frame"
    );
}

/// Owed 40 (owner, 2026-10-19: "1 ok"): a skipped tick leaves no hole in the audio. The compositor
/// repeats the last picture across the gap; the audio of every missed period is pushed with the next
/// tick, from the ring and then the filler, so the track has no jump and its full length.
#[test]
fn skipped_ticks_leave_no_hole_in_the_audio() {
    let _s = serial();
    let dir = Tmp::new("skipped-ticks");
    let (mut p, rx, _f, _l) = programme();
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    let step = period(&P720);
    let t0 = Instant::now();
    let mut skipped = 0;
    for i in 0..150u64 {
        if let Some(w) = (t0 + step * i as u32).checked_duration_since(Instant::now()) {
            thread::sleep(w);
        }
        if i % 10 == 9 {
            skipped += 1;
            continue;
        }
        p.push(i, FrameChoice::Slate(SlateKind::Waiting));
    }
    drain_and_wait(&mut p, &rx);
    let d = decode(&dir.file("a.flv"));
    // One AAC frame is 21.3 ms; a jump past 22 ms is a hole.
    let jumps: Vec<u64> = d
        .audio_pts_ms
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|&dt| dt > 22)
        .collect();
    // Index 148 is the last pushed, so the audio covers 149 periods.
    let want_ms = 149 * 1_000 / u64::from(P720.fps);
    eprintln!(
        "ticks skipped {skipped}; video frames {}; audio frames checked {}, jumps {jumps:?}; audio {} ms against {want_ms}",
        d.frames.len(),
        d.audio_pts_ms.len(),
        d.audio_ms()
    );
    assert_eq!(skipped, 15, "9, 19, … 149");
    assert!(d.audio_pts_ms.len() > 200, "{}", d.audio_pts_ms.len());
    assert!(jumps.is_empty(), "{jumps:?}");
    assert!(
        d.audio_ms().abs_diff(want_ms) <= AV_TOLERANCE_MS,
        "audio {} ms",
        d.audio_ms()
    );
}

// ---------------------------------------------------------------------------------------------
// Owed 84: a sink sends nothing that was queued before it could send (owner, 2026-10-24, option A:
// when the destination recovers after an outage, the queued stale frames are dropped and the stream
// restarts at the next keyframe; viewers never get a burst of old frames). F-023 measured the burst
// in C14 cases 4 and 12: a sink whose connect outlived the fault delivered about 4 s of backlog at the
// clear, and MediaMTX's recorder discarded about 7.9 s. The RTMP legs run the real rtmp2sink against
// `FakeRtmp`, which holds its publish reply for as long as a leg likes.
//
// Each leg shows the programme's slate until OWED84_MARGIN_TICKS before the reconnect point and the
// camera (red) from then on, so a frame from before that margin is told by its picture.

/// Camera ticks before the reconnect point. A frame the programme had not yet encoded when the sink
/// could send is not stale (the encoder's own pipeline: the appsrcs hold at most 3 ticks, and
/// zerolatency x264 adds none), so the slate stops 10 ticks (333 ms) before the reconnect.
const OWED84_MARGIN_TICKS: u64 = 10;
/// The keyframe asked for at the reconnect point comes within this many ticks (6, 200 ms). The
/// encoder's own next keyframe could be a whole GOP away (F-036's 3 s, `OutputProfile::gop_frames`),
/// so a sink that waited for it would lose up to 90 ticks.
const OWED84_KEY_WAIT_TICKS: u64 = 6;

// F-036 (the C1 re-review's m2): the GOP less the forced keyframe's floor leaves these tests their
// margin and key wait, in both profiles, mirroring `config`'s bound. The compiler checks it, frames
// counted as `OutputProfile`'s `frames_in` counts them (whole frames, rounded down), so raising the
// floor, shortening the GOP or raising either tick constant fails the build of this file on any host
// (clippy compiles it without GStreamer); `owed84_margins_fit_between_the_floor_and_the_gop` checks
// the same through the profiles' own methods where the file runs.
const _: () = {
    let profiles = [OutputProfile::P720, OutputProfile::P1080];
    let mut i = 0;
    while i < profiles.len() {
        let fps = profiles[i].fps as u64;
        let gop = fps * PROGRAMME_GOP_MS / 1_000;
        let floor = fps * FORCED_KEYFRAME_MIN_INTERVAL_MS / 1_000;
        assert!(gop >= floor + OWED84_MARGIN_TICKS + OWED84_KEY_WAIT_TICKS);
        i += 1;
    }
};

#[test]
fn owed84_margins_fit_between_the_floor_and_the_gop() {
    let mut checked = 0;
    for profile in [P720, OutputProfile::P1080] {
        let (gop, floor) = (
            u64::from(profile.gop_frames()),
            u64::from(profile.forced_keyframe_min_frames()),
        );
        assert!(
            gop >= floor + OWED84_MARGIN_TICKS + OWED84_KEY_WAIT_TICKS,
            "{}p: GOP {gop} less floor {floor} frames under {} + {}",
            profile.height,
            OWED84_MARGIN_TICKS,
            OWED84_KEY_WAIT_TICKS
        );
        checked += 1;
    }
    eprintln!("profiles checked: {checked}");
    assert_eq!(checked, 2);
}
/// The most a received message may run ahead of real time, measured from the first frame received.
/// A stale backlog runs at least a whole 2 s ring ahead (F-023: about 4 s); this covers the AAC
/// encoder's lead on the video (about 60 ms) and the mux's interleave, and stays a quarter of a ring.
const OWED84_BURST_SLACK_MS: u128 = 500;

/// F-036: the forced keyframe's floor in ticks (`FORCED_KEYFRAME_MIN_INTERVAL_MS` in the 720p
/// profile's frames). The floor tests run 720p alone: both profiles are 30 fps (ADR 0005; `config`
/// pins both at 66 frames), and the encoder holds the floor in running time, not in frames.
fn floor_ticks() -> u64 {
    u64::from(P720.forced_keyframe_min_frames())
}

/// F-036: the tick the keyframe asked for at `reconnect` is made on: at once, unless the encoder's
/// last keyframe, `last_key`, is less than the floor before it; then at that floor.
fn key_due(reconnect: u64, last_key: u64) -> u64 {
    reconnect.max(last_key + floor_ticks())
}

/// The encoder's last keyframe at or before `tick`, in a programme no swap has forced one in: the
/// GOP's own.
fn last_natural_key(tick: u64) -> u64 {
    tick / gop_ticks() * gop_ticks()
}

/// Swaps in sink `gen` and waits for its feeder's line that the encoder took the keyframe request
/// (Owed 84), which it logs after the ask (the C1 review's m4), so the ask has reached the encoder
/// before the next push. F-036's floor makes the order count: an ask that reaches the encoder after
/// a keyframe has gone into the ring is held to the floor after it, which moves every keyframe after.
fn swap_when_asked(p: &mut GstProgramme, lines: &Lines, gen: u64, target: &TargetSpec) {
    p.swap_sink(gen, target);
    assert!(
        wait_for_line(
            lines,
            &format!("output gen={gen}: the keyframe request was taken"),
            Duration::from_secs(3)
        ),
        "gen {gen}: the keyframe request was never taken"
    );
}

/// Swaps in sink `gen` before the programme's first frame and waits for its feeder's line that it
/// asked for no keyframe (CI on PR #109): the ring holds no video, so the encoder's first frame, a
/// keyframe, starts the stream, and the encoder's last keyframe is frame 0 for every later swap's
/// `key_due`. A request there would be matched on frame 0 at a running time 1 000 h out and hold
/// off every later forced keyframe (`gst_output::Feeder::run`): each later sink would wait for the
/// GOP's own.
fn swap_before_the_first_frame(p: &mut GstProgramme, lines: &Lines, gen: u64, target: &TargetSpec) {
    p.swap_sink(gen, target);
    assert!(
        wait_for_line(
            lines,
            &format!("output gen={gen}: no keyframe is asked"),
            Duration::from_secs(3)
        ),
        "gen {gen}: a keyframe was asked before the first frame"
    );
}

fn rtmp_target(server: &FakeRtmp) -> TargetSpec {
    TargetSpec {
        url: server.url(),
        stream_key: "OWED84-fake-stream-key".to_owned(),
    }
}

/// Every decoded frame shows the camera (red), none the slate; returns how many were checked.
fn assert_all_camera(case: &str, d: &Decoded) -> usize {
    let mut checked = 0;
    for (i, f) in d.frames.iter().enumerate() {
        assert!(
            is_red(f.centre) && !close(f.corner_tl, HOUSE_NIGHT, ON_AIR_TOL),
            "{case}: frame {i} (PTS {} ms) is from before the reconnect: {f:?}",
            d.video_pts_ms.get(i).copied().unwrap_or(u64::MAX)
        );
        checked += 1;
    }
    checked
}

/// What `server` received for one publish, checked against Owed 84 and RTMP's timing rules: the
/// first video frame is a keyframe at PTS 0, nothing is from before the reconnect margin, no message
/// runs ahead of real time (no burst), timestamps never go back (each kind, and in arrival order),
/// and the audio starts with the picture, never ahead of it. `paced` is the ticks pushed after the
/// reconnect point, and `wait` the ticks from it to the keyframe's due tick (`key_due`, F-036): a
/// keyframe held to the floor starts the stream there, and nothing from before it is sent. Returns
/// the frames checked.
fn assert_fresh_start(case: &str, server: &FakeRtmp, dir: &Tmp, paced: u64, wait: u64) -> usize {
    let n = server.settle(Duration::from_millis(400), Duration::from_secs(8));
    let path = dir.file(&format!("{case}.flv"));
    let tags = server.write_flv(&path);
    let rec = server.record();
    eprintln!(
        "{case}: accepted {}, published {}, messages {n}, ended {:?}",
        rec.accepted, rec.published, rec.ended
    );
    assert_eq!(rec.published, 1, "{case}: one publish, one reconnect checked");
    assert_eq!(tags, rec.media.len());
    let frames: Vec<&rtmp::Media> = rec
        .media
        .iter()
        .filter(|m| m.is_avc_frame() || m.is_aac_frame())
        .collect();
    let first_video = frames
        .iter()
        .find(|m| m.is_avc_frame())
        .expect("a video frame was received");
    assert!(
        first_video.is_keyframe(),
        "{case}: the first video frame sent is not a keyframe"
    );
    let first_audio = frames
        .iter()
        .find(|m| m.is_aac_frame())
        .expect("an audio frame was received");
    assert!(
        first_audio.ts_ms >= first_video.ts_ms
            && u64::from(first_audio.ts_ms - first_video.ts_ms) <= 1_024 * 1_000 / 48_000,
        "{case}: audio starts at {} ms, the picture at {} ms",
        first_audio.ts_ms,
        first_video.ts_ms
    );
    let (anchor_ts, anchor_at) = (frames[0].ts_ms, frames[0].at);
    let (mut ahead_most, mut back) = (i128::MIN, 0);
    for (i, m) in frames.iter().enumerate() {
        let ahead =
            i128::from(m.ts_ms - anchor_ts.min(m.ts_ms)) - m.at.duration_since(anchor_at).as_millis() as i128;
        ahead_most = ahead_most.max(ahead);
        assert!(
            ahead <= OWED84_BURST_SLACK_MS as i128,
            "{case}: message {i} (ts {} ms) arrived {ahead} ms ahead of real time: a burst",
            m.ts_ms
        );
        if i > 0 && m.ts_ms < frames[i - 1].ts_ms {
            back += 1;
        }
        let prev_same = frames[..i].iter().rev().find(|p| p.kind == m.kind);
        assert!(
            prev_same.is_none_or(|p| m.ts_ms >= p.ts_ms),
            "{case}: message {i} goes back in time within its kind"
        );
    }
    assert_eq!(back, 0, "{case}: {back} messages arrived out of timestamp order");
    drop(rec);
    let d = decode(&path);
    assert!(
        d.first_is_key(),
        "{case}: decoded keys {:?}",
        &d.keys[..d.keys.len().min(5)]
    );
    assert_eq!(d.video_pts_ms.first(), Some(&0), "{case}");
    let checked = assert_all_camera(case, &d);
    eprintln!(
        "{case}: frames checked {checked} for {paced} paced after the reconnect; most ahead of real time {ahead_most} ms"
    );
    let owed = paced - wait;
    // A frame pushed before the reconnect may be encoded after it, unless the keyframe was held.
    let slack = if wait == 0 { OWED84_MARGIN_TICKS } else { 0 };
    eprintln!("{case}: the keyframe due {wait} ticks after the reconnect, {owed} frames owed");
    assert!(
        checked as u64 + OWED84_KEY_WAIT_TICKS >= owed,
        "{case}: {checked} frames for {owed} ticks: the keyframe came late"
    );
    assert!(
        checked as u64 <= owed + slack,
        "{case}: {checked} frames for {owed} ticks"
    );
    checked
}

/// Paces `range` in steps of 10 ticks and checks, after each step, that the output holds nothing
/// (its sink cannot send, so the ring stays the only queue). Returns the samples taken.
fn pace_with_nothing_queued(p: &mut GstProgramme, range: std::ops::Range<u64>, choice: FrameChoice) -> u32 {
    let mut samples = 0;
    let mut from = range.start;
    while from < range.end {
        let to = (from + 10).min(range.end);
        pace(p, from..to, choice);
        let queued = p.output_queued_bytes().expect("an output");
        assert_eq!(
            queued,
            (0, 0),
            "tick {to}: the output queued {queued:?} B in front of a sink that cannot send"
        );
        samples += 1;
        from = to;
    }
    samples
}

/// C14 cases 4 and 12's red phase: the destination comes back while the sink's connect is still
/// pending. The destination holds its publish answer for 2.3 s (in C14 the pending part was the TCP
/// connect; the sink cannot send in either), with the ring full of older programme.
#[test]
fn a_sink_whose_connect_is_pending_when_the_destination_returns_sends_nothing_queued() {
    let _s = serial();
    let dir = Tmp::new("owed84-pending");
    let (mut p, rx, frames, _l) = programme();
    frames.set_camera(solid(gst_video::VideoFormat::I420, 1_280, 720, [255, 0, 0, 255]));
    let server = FakeRtmp::held();
    let slate = FrameChoice::Slate(SlateKind::Reconnecting);
    pace(&mut p, 0..60, slate);
    p.swap_sink(1, &rtmp_target(&server));
    let samples = pace_with_nothing_queued(&mut p, 60..120, slate);
    {
        let rec = server.record();
        assert_eq!(rec.accepted, 1, "the sink connected to the destination");
        assert_eq!(rec.published, 0, "and its publish is still held");
    }
    pace(&mut p, 120..120 + OWED84_MARGIN_TICKS, FrameChoice::Emit);
    let clear = 120 + OWED84_MARGIN_TICKS;
    server.release();
    pace(&mut p, clear..clear + 90, FrameChoice::Emit);
    let seen = drain_and_wait(&mut p, &rx);
    assert!(
        !seen.iter().any(|e| matches!(e, Event::OutputError { .. })),
        "{seen:?}"
    );
    eprintln!("pending: queue samples while held {samples}");
    assert_eq!(samples, 6);
    let wait = key_due(clear, last_natural_key(clear)) - clear;
    assert!(
        wait > OWED84_KEY_WAIT_TICKS,
        "setup: the keyframe is held, {wait}"
    );
    assert_fresh_start("pending", &server, &dir, 90, wait);
}

/// C14's green phase: the pending connect fails first (in C14 at rtmp2sink's 5 s socket timeout;
/// here the destination closes it, so the leg does not wait out a timer), and the next swap's sink
/// connects at once. Neither generation sends anything from before its own connect.
#[test]
fn after_a_connect_that_failed_the_next_sink_sends_nothing_queued() {
    let _s = serial();
    let dir = Tmp::new("owed84-failed");
    let (mut p, rx, frames, _l) = programme();
    frames.set_camera(solid(gst_video::VideoFormat::I420, 1_280, 720, [255, 0, 0, 255]));
    let first = FakeRtmp::held();
    let slate = FrameChoice::Slate(SlateKind::Reconnecting);
    pace(&mut p, 0..60, slate);
    p.swap_sink(1, &rtmp_target(&first));
    let mut samples = pace_with_nothing_queued(&mut p, 60..120, slate);
    first.refuse();
    // Up to 2 s for rtmp2sink to report the refused connect.
    let mut tick = 120;
    let errored = loop {
        samples += pace_with_nothing_queued(&mut p, tick..tick + 10, slate);
        tick += 10;
        let error = rx
            .try_iter()
            .any(|m| matches!(m, Inbound::Event(Event::OutputError { gen: 1 })));
        if error || tick >= 180 {
            break error;
        }
    };
    assert!(errored, "rtmp2sink never reported the refused connect");
    {
        let rec = first.record();
        assert_eq!((rec.accepted, rec.published, rec.media.len()), (1, 0, 0));
    }
    // Never on the GOP's own keyframe (F-036): there the connect and that keyframe race, and the
    // stream would start at it or at the floor after it as the race goes.
    let swap = if (tick + OWED84_MARGIN_TICKS).is_multiple_of(gop_ticks()) {
        tick + 2 * OWED84_MARGIN_TICKS
    } else {
        tick + OWED84_MARGIN_TICKS
    };
    pace(&mut p, tick..swap, FrameChoice::Emit);
    let second = FakeRtmp::prompt();
    p.swap_sink(2, &rtmp_target(&second));
    pace(&mut p, swap..swap + 90, FrameChoice::Emit);
    drain_and_wait(&mut p, &rx);
    eprintln!("failed: gen 1 errored by tick {tick}; queue samples while it connected {samples}");
    assert!(
        samples >= 7,
        "the first connect was pending only {samples} samples"
    );
    // Sink 1 never could send, so it asked for nothing: the last keyframe is the GOP's.
    let wait = key_due(swap, last_natural_key(swap)) - swap;
    assert_fresh_start("failed", &second, &dir, 90, wait);
}

/// The drain ends with the encoders while the sink is still connecting: nothing is owed to a sink
/// that never connected, and rtmp2sink would give the connect up only at its 5 s socket timeout.
#[test]
fn a_drain_while_the_sink_is_still_connecting_ends_without_waiting_for_it() {
    let _s = serial();
    let server = FakeRtmp::held();
    let (mut p, rx, _f, _l) = programme();
    let slate = FrameChoice::Slate(SlateKind::Reconnecting);
    pace(&mut p, 0..30, slate);
    p.swap_sink(1, &rtmp_target(&server));
    pace(&mut p, 30..40, slate);
    let t = Instant::now();
    let mut seen = drain_and_wait(&mut p, &rx);
    let took = t.elapsed();
    seen.extend(events_for(&rx, Duration::from_millis(500)));
    eprintln!("drain while connecting: OutputDrained after {took:?}");
    assert!(took < Duration::from_millis(1_500), "{took:?}");
    assert_eq!(
        seen.iter().filter(|e| **e == Event::OutputDrained).count(),
        1,
        "{seen:?}"
    );
    assert!(
        !seen.iter().any(|e| matches!(e, Event::OutputError { .. })),
        "{seen:?}"
    );
    assert_eq!(server.record().published, 0);
}

// Owed 112 (C3b, F-038; chaos runs 45480676592 `cef-hung` and 45498019823 `dest-stall-rule`): the
// boot's first publish lands after the encoder's first frame. Owed 84 dropped that frame at the gate
// and asked for a keyframe, which the encoder holds to F-036's floor after frame 0, so the stream
// started up to 2.2 s late (head outages of 813 and 816 ms). A backlog of the encoder's first frames
// up to `FIRST_GOP_CATCH_UP_MAX_MS`, and under `OUTPUT_QUEUE_BYTES` a track, now starts the stream at
// the encoder's first keyframe, and nothing is asked.

/// The feeder's line when sink 1's gate opens, either way.
const GATE_OPEN_LINE: &str = "output gen=1: the sink can send after";
/// The same line's ending when it starts from the encoder's first keyframe (Owed 112).
const FIRST_GOP_TAKEN: &str = "so it starts at the encoder's first keyframe";
/// The feeder's line when the encoder's first frames are within the time bound but over the output
/// queue (Owed 112, the C3b review's m2).
const FIRST_GOP_OVER_QUEUE: &str = "output gen=1: the encoder's first frames hold";

/// What one Owed 112 leg's destination received, and what its feeder logged.
struct LatePublish {
    d: Decoded,
    lines: Lines,
    /// The most a received message ran ahead of real time, from the first one received (F-023).
    ahead_most_ms: i128,
    /// The first AAC message's timestamp, as the destination received it.
    first_audio_ts_ms: Option<u32>,
}

/// One Owed 112 leg: sink 1 is swapped in before the programme's first frame, and its publish is
/// held until `late` camera frames are in the ring, then accepted; `total` ticks are paced in all.
fn publish_late(case: &str, profile: &OutputProfile, late: u64, total: u64, dir: &Tmp) -> LatePublish {
    let server = FakeRtmp::held();
    let (mut p, rx, frames, lines) = programme_with(profile);
    frames.set_camera(solid(
        gst_video::VideoFormat::I420,
        profile.width,
        profile.height,
        [255, 0, 0, 255],
    ));
    let stats = p.stats();
    p.swap_sink(1, &rtmp_target(&server));
    pace(&mut p, 0..late, FrameChoice::Emit);
    thread::sleep(OWED84_ENCODED_SETTLE);
    assert_eq!(
        stats.snapshot().encoded_frames,
        Some(late),
        "{case}: setup: the late frames reached the ring before the publish"
    );
    assert_eq!(server.record().published, 0, "{case}: setup: the publish is held");
    server.release();
    assert!(
        wait_for_line(&lines, GATE_OPEN_LINE, Duration::from_secs(3)),
        "{case}: the gate never opened"
    );
    pace(&mut p, late..total, FrameChoice::Emit);
    let seen = drain_and_wait(&mut p, &rx);
    assert!(
        !seen.iter().any(|e| matches!(e, Event::OutputError { .. })),
        "{case}: {seen:?}"
    );
    server.settle(Duration::from_millis(400), Duration::from_secs(8));
    let path = dir.file(&format!("{case}.flv"));
    let tags = server.write_flv(&path);
    let rec = server.record();
    assert_eq!(rec.published, 1, "{case}: one publish");
    assert_eq!(tags, rec.media.len(), "{case}");
    let media: Vec<&rtmp::Media> = rec
        .media
        .iter()
        .filter(|m| m.is_avc_frame() || m.is_aac_frame())
        .collect();
    let first = media.first().expect("a frame was received");
    let (anchor_ts, anchor_at) = (first.ts_ms, first.at);
    let ahead_most_ms = media
        .iter()
        .map(|m| {
            i128::from(m.ts_ms - anchor_ts.min(m.ts_ms)) - m.at.duration_since(anchor_at).as_millis() as i128
        })
        .max()
        .unwrap_or(i128::MIN);
    let first_audio_ts_ms = media.iter().find(|m| m.is_aac_frame()).map(|m| m.ts_ms);
    drop(rec);
    for l in lines.lock().unwrap_or_else(PoisonError::into_inner).iter() {
        if l.contains("output gen=1: the") {
            eprintln!("{case}: {l}");
        }
    }
    LatePublish {
        d: decode(&path),
        lines,
        ahead_most_ms,
        first_audio_ts_ms,
    }
}

/// Checks a leg that started at the encoder's first keyframe, `late` frames queued: the feeder said
/// so and asked for no keyframe; the destination got every frame paced, the first a keyframe at PTS
/// 0, the camera in each; the audio starts with it, the AAC frame at 0 sent (the C3b review's m5:
/// within one AAC frame of PTS 0, as `ninety_slate_ticks_give_ninety_frames_at_constant_spacing` reads it);
/// its keyframes are the GOP's own (none forced a floor in); and nothing ran further ahead of real
/// time than the backlog taken (the `late - 1` frame periods from the first keyframe to the newest;
/// CI read 1 122 ms for an 1 100 ms one) and Owed 84's slack (`OWED84_BURST_SLACK_MS`), so no frame
/// from before the encoder's first was sent. Returns the frames checked.
fn assert_first_gop_start(case: &str, leg: &LatePublish, late: u64, total: u64, gop: u64) -> usize {
    let d = &leg.d;
    let keys = key_indexes(d);
    eprintln!(
        "{case}: {} frames for the {total} paced; keyframes at {keys:?}; most ahead of real time {} ms; \
         first AAC at {:?} ms (decoded {:?})",
        d.frames.len(),
        leg.ahead_most_ms,
        leg.first_audio_ts_ms,
        d.audio_pts_ms.first()
    );
    assert_eq!(
        logged(&leg.lines, FIRST_GOP_TAKEN),
        1,
        "{case}: the feeder did not start at the first keyframe"
    );
    assert_eq!(
        logged(&leg.lines, "output gen=1: the keyframe request was"),
        0,
        "{case}: a keyframe was asked"
    );
    assert!(
        d.first_is_key(),
        "{case}: keys {:?}",
        &d.keys[..d.keys.len().min(5)]
    );
    assert_eq!(d.video_pts_ms.first(), Some(&0), "{case}");
    // The audio starts with the picture, at or after it (no PTS before 0: the rebase clamps none
    // away) and within one AAC frame (1 024 samples at 48 kHz, 21 ms), so the frame at 0 was sent.
    let aac_frame_ms = 1_024 * 1_000 / 48_000;
    let wire = leg.first_audio_ts_ms.expect("an AAC frame was received");
    assert!(
        u64::from(wire) <= aac_frame_ms,
        "{case}: the first AAC frame on the wire is at {wire} ms: the one at 0 was not sent"
    );
    let decoded = *d.audio_pts_ms.first().expect("audio");
    assert!(
        decoded != u64::MAX && decoded <= aac_frame_ms,
        "{case}: the first decoded AAC PTS is {decoded} ms: the one at 0 was not sent"
    );
    assert_eq!(
        d.frames.len() as u64,
        total,
        "{case}: the stream starts at frame 0"
    );
    assert_eq!(keys, vec![0, gop], "{case}: the GOP's own keyframes, none forced");
    let backlog_ms = (late - 1) * 1_000 / u64::from(P720.fps);
    assert!(
        leg.ahead_most_ms <= i128::from(backlog_ms) + OWED84_BURST_SLACK_MS as i128,
        "{case}: ran {} ms ahead of real time for a {backlog_ms} ms backlog",
        leg.ahead_most_ms
    );
    let checked = assert_all_camera(case, d);
    assert_eq!(checked as u64, total, "{case}");
    checked
}

/// Checks a leg whose backlog was dropped as Owed 84 drops it: nothing taken, the keyframe asked
/// for, and the stream started at it, held to the floor after frame 0 (`key_due`). `over_queue`:
/// whether the bytes guard was the one that refused (its line), rather than the time bound.
fn assert_first_gop_dropped(case: &str, leg: &LatePublish, late: u64, total: u64, over_queue: bool) {
    let d = &leg.d;
    let due = key_due(late, last_natural_key(late));
    let owed = total - due;
    let n = d.frames.len() as u64;
    eprintln!(
        "{case}: {late} late, keyframe due at {due}; {n} frames for the {owed} paced from it; keyframes at {:?}",
        key_indexes(d)
    );
    assert_eq!(
        logged(&leg.lines, FIRST_GOP_TAKEN),
        0,
        "{case}: the backlog was taken"
    );
    assert_eq!(
        logged(&leg.lines, FIRST_GOP_OVER_QUEUE),
        usize::from(over_queue),
        "{case}: which guard refused"
    );
    assert_eq!(
        logged(&leg.lines, "output gen=1: the keyframe request was taken"),
        1,
        "{case}: no keyframe was asked"
    );
    assert!(
        d.first_is_key(),
        "{case}: keys {:?}",
        &d.keys[..d.keys.len().min(5)]
    );
    assert_eq!(d.video_pts_ms.first(), Some(&0), "{case}");
    assert!(
        n <= owed,
        "{case}: {n} frames for the {owed} from the floor: it started before the floor"
    );
    assert!(
        n + OWED84_KEY_WAIT_TICKS >= owed,
        "{case}: {n} frames: the keyframe came more than {OWED84_KEY_WAIT_TICKS} ticks after the floor"
    );
    let checked = assert_all_camera(case, d);
    eprintln!("{case}: frames checked {checked}");
    assert_eq!(checked as u64, n);
    assert!(checked > 0);
}

/// The bytes `late` frames of a profile take at its CBR rate (x264's CBR with filler, F-005): the
/// legs below are set clear of `OUTPUT_QUEUE_BYTES` by it, so which guard refuses is not a coin toss.
fn cbr_bytes(profile: &OutputProfile, late: u64) -> u64 {
    late * u64::from(profile.video_kbps) * 1_000 / 8 / u64::from(profile.fps)
}

/// Owed 112's witness: the publish lands one, two or three frames after the encoder's first (the
/// chaos runs' race was one, 37 to 38 ms), within the first GOP. The stream starts at frame 0, the
/// encoder's first keyframe, with no head outage: every frame paced reaches the destination. Before
/// the fix the gate dropped those frames and the keyframe it asked for came at F-036's floor after
/// frame 0, 66 frames in. Both modes (one leg at 1080p: both are 30 fps, ADR 0005).
#[test]
fn a_publish_that_lands_frames_after_the_first_starts_at_the_first_keyframe() {
    let _s = serial();
    let mut legs = 0;
    for (profile, late) in [(P720, 1), (P720, 2), (P720, 3), (OutputProfile::P1080, 1)] {
        let case = format!("owed112-{}p-{late}-late", profile.height);
        let dir = Tmp::new(&case);
        let gop = u64::from(profile.gop_frames());
        let total = gop + OWED84_MARGIN_TICKS;
        let leg = publish_late(&case, &profile, late, total, &dir);
        let checked = assert_first_gop_start(&case, &leg, late, total, gop);
        eprintln!("{case}: frames checked {checked}");
        legs += 1;
    }
    eprintln!("Owed 112 legs checked: {legs}");
    assert_eq!(legs, 4);
}

/// Owed 112's two bounds. Within both (720p, 25 frames: 800 ms, about 417 KB at 4 Mbps) the backlog
/// is taken. Past the time bound (720p, 32 frames: `FIRST_GOP_CATCH_UP_MAX_MS` from `config` and a
/// frame), it is dropped by the time rule, which is read first (no bytes line). Inside
/// the time bound and past `OUTPUT_QUEUE_BYTES` (1080p, 30 frames: 967 ms, about 750 KB at 6 Mbps)
/// it is dropped by the bytes rule (the C3b review's m2). The time legs are 720p because only that
/// rate stays under the queue for the whole bound; the bytes leg is 1080p because only its rate
/// passes the queue inside it (the C3b review's m7). A dropped leg starts at the keyframe it asks
/// for, which the encoder holds to the floor after frame 0 (`key_due`).
#[test]
fn a_first_gop_backlog_within_both_bounds_is_taken_and_past_either_is_dropped() {
    let _s = serial();
    let p1080 = OutputProfile::P1080;
    let gop = gop_ticks();
    let total = gop + OWED84_MARGIN_TICKS;
    let bound = FIRST_GOP_CATCH_UP_MAX_MS * u64::from(P720.fps) / 1_000;
    let (inside, past_time, past_bytes) = (25, bound + 2, bound);
    // Each leg's premise, from `config` and the profiles, never from the rule.
    assert!(
        past_time + OWED84_KEY_WAIT_TICKS < floor_ticks(),
        "setup: a dropped backlog waits for the floor"
    );
    assert!(
        (inside - 1) * 1_000 / u64::from(P720.fps) <= FIRST_GOP_CATCH_UP_MAX_MS
            && cbr_bytes(&P720, inside) * 5 / 4 < OUTPUT_QUEUE_BYTES,
        "setup: the inside leg is within both bounds with a quarter to spare"
    );
    assert!(
        (past_time - 1) * 1_000 / u64::from(P720.fps) > FIRST_GOP_CATCH_UP_MAX_MS,
        "setup: the time leg is past the time bound"
    );
    assert!(
        (past_bytes - 1) * 1_000 / u64::from(p1080.fps) <= FIRST_GOP_CATCH_UP_MAX_MS
            && cbr_bytes(&p1080, past_bytes) * 4 / 5 >= OUTPUT_QUEUE_BYTES,
        "setup: the bytes leg is within the time bound and a fifth past the queue"
    );

    let case = "owed112-inside-both";
    let dir = Tmp::new(case);
    let leg = publish_late(case, &P720, inside, total, &dir);
    assert_first_gop_start(case, &leg, inside, total, gop);

    let case = "owed112-past-the-time-bound";
    let dir = Tmp::new(case);
    let leg = publish_late(case, &P720, past_time, total, &dir);
    assert_first_gop_dropped(case, &leg, past_time, total, false);

    let case = "owed112-past-the-output-queue";
    let dir = Tmp::new(case);
    let leg = publish_late(case, &p1080, past_bytes, total, &dir);
    assert_first_gop_dropped(case, &leg, past_bytes, total, true);
}

/// The file's size, 0 if it does not exist (filesink creates it when its pipeline starts).
fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
}

/// Whether the captured log holds a line containing `needle` within `timeout`.
fn wait_for_line(lines: &Lines, needle: &str, timeout: Duration) -> bool {
    let end = Instant::now() + timeout;
    loop {
        if lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|l| l.contains(needle))
        {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn logged(lines: &Lines, needle: &str) -> usize {
    lines
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|l| l.contains(needle))
        .count()
}

/// A session end must not wait out `drain_timeout_ms` (7 s) for an output that has sent nothing:
/// rtmp2sink and filesink have nothing owed, and flvmux 1.24.2 forwards no EOS before its first
/// buffer (PR #57 review I-1). This bound is the one the drain at a pending connect keeps.
const OWED84_EMPTY_DRAIN_MAX: Duration = Duration::from_millis(1_500);
/// A pause after the last paced tick that outlasts its encode (zerolatency x264 and the AAC
/// encoder take milliseconds), so no frame is encoded after the gate opens.
const OWED84_ENCODED_SETTLE: Duration = Duration::from_millis(200);

/// Drains, and checks the drain ended within `OWED84_EMPTY_DRAIN_MAX` with exactly one
/// `OutputDrained` and no `OutputError` (a further 500 ms is read for a second one). Returns how long
/// it took.
fn assert_empty_output_drains_at_once(case: &str, p: &mut GstProgramme, rx: &Receiver<Inbound>) -> Duration {
    let t = Instant::now();
    p.drain();
    let mut seen = events_until(rx, OWED84_EMPTY_DRAIN_MAX, |e| *e == Event::OutputDrained);
    let took = t.elapsed();
    seen.extend(events_for(rx, Duration::from_millis(500)));
    eprintln!("{case}: OutputDrained after {took:?}");
    assert_eq!(
        seen.iter().filter(|e| **e == Event::OutputDrained).count(),
        1,
        "{case}: {seen:?}"
    );
    assert!(
        !seen.iter().any(|e| matches!(e, Event::OutputError { .. })),
        "{case}: {seen:?}"
    );
    took
}

/// I-1 (a): the destination accepts the publish in the instant the session ends. Whichever the
/// feeder sees first, the open gate or the encoders' EOS, the drain ends with the encoders: at the
/// gate (`reconnect_point`) or with nothing sent after it (`run`). The leg prints which.
#[test]
fn a_publish_accepted_during_the_drain_ends_it_with_the_encoders() {
    let _s = serial();
    let server = FakeRtmp::held();
    let (mut p, rx, _f, lines) = programme();
    let slate = FrameChoice::Slate(SlateKind::Reconnecting);
    pace(&mut p, 0..30, slate);
    p.swap_sink(1, &rtmp_target(&server));
    pace(&mut p, 30..60, slate);
    // Every paced frame is encoded before the publish is accepted, so each one is from before it.
    thread::sleep(OWED84_ENCODED_SETTLE);
    server.release();
    assert_empty_output_drains_at_once("accepted during the drain", &mut p, &rx);
    let opened = logged(&lines, "output gen=1: the sink can send");
    eprintln!(
        "accepted during the drain: the gate read open first: {}",
        opened == 1
    );
    let rec = server.record();
    assert_eq!(rec.accepted, 1, "the sink connected to the destination");
    assert!(
        !rec.media.iter().any(|m| m.is_avc_frame() || m.is_aac_frame()),
        "a frame from before the publish was sent"
    );
}

/// I-1 (b): the publish is accepted and the keyframe asked for, and the session ends before the
/// encoder gets another frame to make it from. Nothing is sent, and the drain does not wait for it.
#[test]
fn a_drain_before_the_asked_keyframe_ends_without_waiting_for_it() {
    let _s = serial();
    let server = FakeRtmp::held();
    let (mut p, rx, _f, lines) = programme();
    let slate = FrameChoice::Slate(SlateKind::Reconnecting);
    pace(&mut p, 0..30, slate);
    p.swap_sink(1, &rtmp_target(&server));
    pace(&mut p, 30..60, slate);
    thread::sleep(OWED84_ENCODED_SETTLE);
    server.release();
    assert!(
        wait_for_line(&lines, "output gen=1: the sink can send", Duration::from_secs(3)),
        "the gate never opened"
    );
    assert_empty_output_drains_at_once("drain before the key", &mut p, &rx);
    let rec = server.record();
    assert_eq!((rec.accepted, rec.published), (1, 1), "the publish was accepted");
    assert!(
        !rec.media.iter().any(|m| m.is_avc_frame() || m.is_aac_frame()),
        "a frame from before the publish was sent"
    );
}

/// I-1 (c) and m-6: an encoder that never takes the keyframe request. It is asked again at each
/// frame; the swap comes just after the GOP's second keyframe and the 30 paced end before its
/// third (`OutputProfile::gop_frames`, F-036), so nothing is sent, and the drain ends with the
/// encoders.
#[test]
fn a_keyframe_request_never_taken_sends_nothing_and_the_drain_ends_with_the_encoders() {
    let _s = serial();
    let dir = Tmp::new("owed84-not-taken");
    let (mut p, rx, _f, lines) = programme();
    let swap = gop_ticks() + OWED84_MARGIN_TICKS;
    assert!(
        swap + 30 + OWED84_KEY_WAIT_TICKS < 2 * gop_ticks(),
        "setup: no keyframe of the encoder's own"
    );
    pace(&mut p, 0..swap, FrameChoice::Slate(SlateKind::Waiting));
    p.refuse_key_requests(u32::MAX);
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    pace(&mut p, swap..swap + 30, FrameChoice::Slate(SlateKind::Waiting));
    assert_empty_output_drains_at_once("not taken", &mut p, &rx);
    let asked = u32::MAX - p.key_refusals_left();
    eprintln!("not taken: requests refused {asked}");
    // One at the reconnect point, then one per ring entry while nothing is sent (30 ticks of video,
    // and their audio).
    assert!(asked > 30, "the request was asked {asked} times");
    assert_eq!(
        logged(&lines, "output gen=1: the keyframe request was not taken"),
        1,
        "logged once, not per try"
    );
    assert_eq!(file_len(&dir.file("a.flv")), 0, "nothing was sent");
}

/// m-6: a request the encoder refuses once is asked again at the next frame, so the stream starts
/// within `OWED84_KEY_WAIT_TICKS` of the swap, not at the encoder's own next keyframe (the GOP's
/// third, 14 ticks after this swap: F-036's 3 s GOP and 2.2 s floor). The swap comes more than
/// `FORCED_KEYFRAME_MIN_INTERVAL_MS` after the GOP's second keyframe, so the retried request is due
/// at once.
#[test]
fn a_keyframe_request_refused_once_is_asked_again_at_the_next_frame() {
    let _s = serial();
    let dir = Tmp::new("owed84-refused-once");
    let (mut p, rx, frames, lines) = programme();
    frames.set_camera(solid(gst_video::VideoFormat::I420, 1_280, 720, [255, 0, 0, 255]));
    let camera = gop_ticks() + u64::from(P720.forced_keyframe_min_frames());
    let swap = camera + OWED84_MARGIN_TICKS;
    assert!(
        2 * gop_ticks() > swap + OWED84_KEY_WAIT_TICKS,
        "setup: the encoder's own keyframe comes later than the retry's"
    );
    pace(&mut p, 0..camera, FrameChoice::Slate(SlateKind::Waiting));
    pace(&mut p, camera..swap, FrameChoice::Emit);
    p.refuse_key_requests(1);
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    let paced = 45;
    pace(&mut p, swap..swap + paced, FrameChoice::Emit);
    drain_and_wait(&mut p, &rx);
    assert_eq!(p.key_refusals_left(), 0, "the first request was refused");
    assert_eq!(
        logged(&lines, "output gen=1: the keyframe request was taken at try 2"),
        1,
        "the refused request was not asked again"
    );
    let len = file_len(&dir.file("a.flv"));
    assert!(len > 0, "refused once: nothing reached the file ({len} B)");
    let d = decode(&dir.file("a.flv"));
    assert!(d.first_is_key(), "keys {:?}", &d.keys[..d.keys.len().min(5)]);
    assert_eq!(d.video_pts_ms.first(), Some(&0));
    let n = d.frames.len() as u64;
    assert!(
        n + OWED84_KEY_WAIT_TICKS >= paced,
        "{n} frames for the {paced} paced since the swap: the keyframe came late"
    );
    assert!(
        n <= paced + OWED84_MARGIN_TICKS,
        "{n} frames: the ring was replayed"
    );
    let checked = assert_all_camera("refused once", &d);
    eprintln!("refused once: frames checked {checked}");
    assert_eq!(checked as u64, n);
}

/// The gate reads rtmp2sink's `stats` contract and the empty drain flvmux's EOS rule, both read in
/// the 1.24.2 source (F-027). check.yml installs the runner's GStreamer from Ubuntu 24.04's apt, as
/// the image does (docker/Dockerfile `FROM ubuntu:24.04`): a bump on either side reds here first.
/// This test runs on the runner only; image.yml runs only the graphics suite in the image.
#[test]
fn the_gate_reads_gstreamer_1_24() {
    gst::init().expect("gstreamer");
    let want = "1.24.";
    let (major, minor, micro, _) = gst::version();
    let core = format!("{major}.{minor}.{micro}");
    let mut checked = 0;
    for name in ["rtmp2sink", "flvmux"] {
        let plugin = gst::ElementFactory::find(name)
            .and_then(|f| f.plugin())
            .unwrap_or_else(|| panic!("no {name}"));
        let v = plugin.version();
        assert!(
            v.starts_with(want),
            "{name} is from GStreamer {v}, not {want}x: re-read its stats and EOS rules (F-027)"
        );
        checked += 1;
    }
    assert!(
        format!("{core}.").starts_with(want),
        "GStreamer core {core}, not {want}x: re-read F-027"
    );
    assert_eq!(checked, 2);
}

// ---------------------------------------------------------------------------------------------
// R4 task SC2: the frame tap (spec 2026-10-27 §3.6.3). The size, the quality and the byte bound are
// `domain::config`'s, pinned there to the spec and to S9.

use adapters::frame_tap::GstFrameTap;
use app::ports::FrameTap;
use domain::config::{FRAME_GRAB_SIZE, FRAME_GRAB_WAIT_MS, FRAME_UPLOAD_MAX_BYTES};

/// A programme in `profile` with a frame tap, asked for before `start()` as the binary asks for it.
fn programme_with_tap(profile: &OutputProfile) -> (GstProgramme, GstFrameTap, Receiver<Inbound>, FrameStore) {
    gst::init().expect("gstreamer");
    let frames = FrameStore::default();
    let (tx, rx) = crossbeam_channel::bounded(8_192);
    let mut p = GstProgramme::new(frames.clone(), tx).with_log(Arc::new(|_: &str| {}));
    let tap = p.frame_tap().expect("a tap before start");
    p.start(profile).expect("start");
    (p, tap, rx, frames)
}

/// A JPEG decoded by GStreamer's own `jpegdec`, to RGB: its size and a pixel reader. `jpegdec` reads
/// every YCbCr JPEG as JFIF's full-range BT.601 (ITU-T T.871 §7), as every decoder does, so a black
/// that decodes to 16 is a limited-range frame encoded as-is.
struct Jpeg {
    w: u32,
    h: u32,
    stride: usize,
    rgb: Vec<u8>,
}

impl Jpeg {
    fn decode(bytes: &[u8]) -> Jpeg {
        let pipeline = gst::parse::launch(
            "appsrc name=src caps=image/jpeg ! jpegdec ! videoconvert ! video/x-raw,format=RGB ! \
             appsink name=out sync=false",
        )
        .expect("jpeg pipeline")
        .downcast::<gst::Pipeline>()
        .expect("pipeline");
        let src = pipeline
            .by_name("src")
            .expect("src")
            .downcast::<gst_app::AppSrc>()
            .expect("appsrc");
        let out = pipeline
            .by_name("out")
            .expect("out")
            .downcast::<gst_app::AppSink>()
            .expect("appsink");
        pipeline.set_state(gst::State::Playing).expect("playing");
        src.push_buffer(gst::Buffer::from_slice(bytes.to_vec()))
            .expect("push");
        src.end_of_stream().expect("eos");
        let sample = out
            .try_pull_sample(gst::ClockTime::from_seconds(5))
            .expect("the JPEG decodes");
        let info = gst_video::VideoInfo::from_caps(sample.caps().expect("caps")).expect("info");
        let frame =
            gst_video::VideoFrame::from_buffer_readable(sample.buffer_owned().expect("buffer"), &info)
                .expect("frame");
        let j = Jpeg {
            w: info.width(),
            h: info.height(),
            stride: frame.plane_stride()[0] as usize,
            rgb: frame.plane_data(0).expect("plane").to_vec(),
        };
        pipeline.set_state(gst::State::Null).expect("null");
        j
    }

    fn at(&self, x: u32, y: u32) -> Rgb {
        let o = y as usize * self.stride + x as usize * 3;
        [self.rgb[o], self.rgb[o + 1], self.rgb[o + 2]]
    }
}

/// The SOF0 marker's height and width, read from the JPEG's own header (ITU T.81 B.2.2).
fn sof0_size(jpeg: &[u8]) -> Option<(u32, u32)> {
    let at = jpeg.windows(2).position(|w| w == [0xFF, 0xC0])?;
    let h = u32::from(u16::from_be_bytes([jpeg[at + 5], jpeg[at + 6]]));
    let w = u32::from(u16::from_be_bytes([jpeg[at + 7], jpeg[at + 8]]));
    Some((w, h))
}

/// One grab on another thread while the programme runs at pace, in both modes (1080p is the
/// production default, AGENTS.md §2): a 640 × 360 JPEG of the composed frame (the green camera, its
/// black bottom-left corner, and the overlay's blue corner on top), within web's bound, with JFIF's
/// black, and the programme lost no frame to it.
#[test]
fn the_frame_tap_grabs_the_composed_programme_as_a_640x360_jpeg() {
    let _s = serial();
    let mut modes = 0;
    for profile in [OutputProfile::P720, OutputProfile::P1080] {
        let (w, h) = (profile.width, profile.height);
        let dir = Tmp::new(&format!("tap-{h}"));
        let (mut p, mut tap, rx, frames) = programme_with_tap(&profile);
        p.swap_sink(1, &file_target(&dir.file("a.flv")));
        // Green, with black in the left quarter of the bottom half.
        frames.set_camera(picture(gst_video::VideoFormat::Bgra, w, h, move |x, y| {
            if x < w / 4 && y >= h / 2 {
                [0, 0, 0, 255]
            } else {
                [0, 255, 0, 255]
            }
        }));
        frames.set_overlay(overlay_blue_corner());
        pace(&mut p, 0..10, FrameChoice::Emit);
        let grab = thread::spawn(move || {
            let got = tap.grab();
            (got, tap)
        });
        pace(&mut p, 10..40, FrameChoice::Emit);
        let (got, mut tap) = grab.join().expect("the grab thread");
        let jpeg = got.expect("a frame");
        assert_eq!(&jpeg[..3], &[0xFF, 0xD8, 0xFF], "{h}p: SOI");
        assert_eq!(&jpeg[jpeg.len() - 2..], &[0xFF, 0xD9], "{h}p: EOI");
        assert!(jpeg.len() <= FRAME_UPLOAD_MAX_BYTES, "{h}p: {} bytes", jpeg.len());
        assert_eq!(sof0_size(&jpeg), Some(FRAME_GRAB_SIZE), "{h}p: the header's size");
        let pic = Jpeg::decode(&jpeg);
        assert_eq!((pic.w, pic.h), FRAME_GRAB_SIZE);
        let centre = pic.at(pic.w / 2, pic.h / 2);
        assert!(is_green(centre), "{h}p: {centre:?}");
        // The overlay's 100 × 100 of 1920 × 1080 is 33 × 33 at 640 × 360.
        assert!(is_blue(pic.at(12, 12)), "{h}p: {:?}", pic.at(12, 12));
        // m6 (SC2 polish): black decodes to 0 in JFIF's full range; a limited-range frame encoded
        // as-is decodes it to 16 (BT.709's foot room), so the bound sits halfway.
        let black = pic.at(pic.w / 8, pic.h * 3 / 4);
        assert!(black.iter().all(|&c| c <= 8), "{h}p: black reads {black:?}");
        // A second grab, with the encoder kept: another frame.
        let again = thread::spawn(move || tap.grab());
        pace(&mut p, 40..70, FrameChoice::Emit);
        let second = again.join().expect("the second grab").expect("a second frame");
        assert_eq!(sof0_size(&second), Some(FRAME_GRAB_SIZE));
        drain_and_wait(&mut p, &rx);
        let d = decode(&dir.file("a.flv"));
        assert_eq!(d.frames.len(), 70, "{h}p: the programme kept every frame");
        modes += 1;
    }
    assert_eq!(modes, 2);
}

/// A grab with no programme frame coming (nothing pushed) fails within its bound, and the next
/// frame the programme makes after it is not held by the tap: the programme runs on untouched.
#[test]
fn a_grab_with_no_programme_frame_fails_within_its_bound() {
    let _s = serial();
    let dir = Tmp::new("tap-none");
    // 720p only: the wait for a frame does not depend on the mode.
    let (mut p, mut tap, rx, _frames) = programme_with_tap(&P720);
    p.swap_sink(1, &file_target(&dir.file("a.flv")));
    let began = Instant::now();
    let got = tap.grab();
    let took = began.elapsed();
    assert_eq!(
        got,
        Err(format!("no programme frame within {FRAME_GRAB_WAIT_MS} ms"))
    );
    assert!(took >= Duration::from_millis(FRAME_GRAB_WAIT_MS), "{took:?}");
    assert!(
        took < Duration::from_millis(FRAME_GRAB_WAIT_MS + 1_000),
        "{took:?}"
    );
    pace(&mut p, 0..30, FrameChoice::Slate(SlateKind::Waiting));
    drain_and_wait(&mut p, &rx);
    assert_eq!(decode(&dir.file("a.flv")).frames.len(), 30);
    drop(tap);
}

/// The tap is asked for before `start()` (its probe goes in as the pipeline is built, never
/// rebuilt), and once.
#[test]
fn the_frame_tap_is_refused_after_start_and_a_second_time() {
    let _s = serial();
    gst::init().expect("gstreamer");
    let (tx, _rx) = crossbeam_channel::bounded(64);
    let mut p = GstProgramme::new(FrameStore::default(), tx);
    assert!(p.frame_tap().is_some());
    assert!(p.frame_tap().is_none(), "a second tap");
    let (mut q, _rx2, _f, _l) = programme();
    assert!(q.frame_tap().is_none(), "after start");
}
