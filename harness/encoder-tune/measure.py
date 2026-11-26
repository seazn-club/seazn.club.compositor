#!/usr/bin/env python3
"""C5 encoder-tune measurement (compositor-c5-brief.md, scan finding 6; F-033's method).

MEASUREMENT ONLY: nothing here is the shipped encoder. The legs are what the brief names; the product's
x264enc settings (`crates/adapters/src/gst_programme.rs`, `OutputProfile::P1080`, `config.rs`) are
restated in `COMMON` below, each with its source, and `legs()` varies only what a leg varies.

Subcommands (run by `.github/workflows/encoder-tune.yml`; GStreamer's Python bindings and an ffmpeg
with libvmaf are needed by `quality` and `paced`, never by `table` or the self-test):

  clip      --url U --out F.yuv --frames N      the first N frames of a Xiph y4m as raw I420
  quality   --clip NAME=F.yuv ... --out F.json  x264enc offline per leg, then libvmaf
  paced     --clip F.yuv --out F.json           30 fps real-time feed: CPU, added latency, forced keys
  table     --quality F.json --paced F.json     the markdown tables
  selftest                                      the pure helpers, no GStreamer
"""

import argparse
import json
import math
import os
import re
import statistics
import subprocess
import sys
import tempfile
import threading
import time

W, H, FPS = 1920, 1080, 30
FRAME_BYTES = W * H * 3 // 2

# The product's settings, restated with their sources. Never derived from the code under test by
# import: this script is a measurement, and each number is the declaration it cites.
COMMON = {
    "speed-preset": "medium",  # gst_programme.rs X264_SPEED_PRESET (owner ruling 2026-10-27, F-033)
    "bitrate": 6000,  # config.rs OutputProfile::P1080.video_kbps
    "key-int-max": 90,  # config.rs PROGRAMME_GOP_MS 3000 at 30 fps (F-036)
    "vbv-buf-capacity": 1000,  # config.rs X264_VBV_MS
    # gst_programme.rs X264_OPTIONS + X264_NO_SCENE_CUT
    "option-string": "nal-hrd=cbr:filler=1:scenecut=0",
    # config.rs FORCED_KEYFRAME_MIN_INTERVAL_MS = CF floor 2000 + margin 200, in ns
    "min-force-key-unit-interval": 2_200_000_000,
}
CF_GOP_FLOOR_MS = 2000  # config.rs CF_KEYFRAME_INTERVAL_MIN_MS (Cloudflare: GOP between 2 and 10 s)
PRODUCT_FLOOR_MS = 2200  # config.rs FORCED_KEYFRAME_MIN_INTERVAL_MS
# C5 Phase B (the shipped guard, `gst_output::own_key_in_flight`): the adapter asks for no keyframe while
# the GOP's own is due within the encoder's delay of the newest video it has taken, counted in FRAMES
# since the last keyframe taken (x264 counts frames; the review's I3).
GOP_FRAMES = 90  # gop_frames() at 30 fps: config.rs PROGRAMME_GOP_MS 3000
DELAY_FRAMES = 24  # config.rs X264_DELAY_FRAMES = X264_RC_LOOKAHEAD_FRAMES 15 + X264_THREADS 9
DELAY_MS = 800  # config.rs X264_DELAY_MS: those frames at 30 fps


def legs(vcpus=6):
    """The brief's five legs, in its order, then R0 (F-033's own x264 settings, for the method check).

    x264's automatic thread count is cpus * 3/2 for frame threads and cpus for sliced threads (x264
    `x264_validate_parameters`); the production guest is performance-6x, so 9 and 6. The `threads`
    property is set to those so a 4-vCPU runner encodes what a 6-vCPU guest would. `probe-threads` in
    the workflow prints the options each leg really got.
    """
    frame_threads, sliced_threads = vcpus * 3 // 2, vcpus
    today = {"tune": "zerolatency", "bframes": 0, "threads": sliced_threads, "profile": "main"}
    nt = {"bframes": 0, "rc-lookahead": 15, "sliced-threads": "false", "threads": frame_threads, "profile": "high"}
    return [
        {"id": "L1", "name": "today: zerolatency, Main, filler=1", "props": today},
        {"id": "L2", "name": "no tune, bframes=0, lookahead 15, frame threads, High", "props": nt},
        {"id": "L3", "name": "as L2, lookahead 20", "props": {**nt, "rc-lookahead": 20}},
        {"id": "L4", "name": "as L2 + bframes=2", "props": {**nt, "bframes": 2}},
        {
            "id": "L5",
            "name": "as L1, nal-hrd=vbr (HRD signalled, no filler)",
            "props": {**today, "option-string": "nal-hrd=vbr:scenecut=0"},
        },
        {
            # The brief's literal leg 5. x264 documents `--nal-hrd cbr` as implying filler, and the first
            # full run's SEI read `filler=1` under it: kept to prove it is leg 1, so L5 is the real leg.
            "id": "L5a",
            "name": "as L1, filler=0 under nal-hrd=cbr (the brief's literal leg)",
            "props": {**today, "option-string": "nal-hrd=cbr:filler=0:scenecut=0"},
            "quality_only": True,
        },
        {
            "id": "X1",
            "name": "extra (not a brief leg): as L2, sync-lookahead=0",
            "props": {**nt, "sync-lookahead": 0},
        },
        {
            # C5 Phase B: the shipped encoder (L2 with X1's sync-lookahead=0 and threads 9) asked for its
            # forced keys the way the product now asks: not while the GOP's own is in the delay.
            "id": "P1",
            "name": "the product (C5): as X1, forced keys asked behind the in-flight guard",
            "props": {**nt, "sync-lookahead": 0},
            "guard": True,
            "paced_only": True,
        },
        {
            "id": "R0",
            "name": "F-033's settings (keyint 60, scene cut on), as L1",
            "props": {**today, "key-int-max": 60, "option-string": "nal-hrd=cbr:filler=1"},
            "quality_only": True,
        },
    ]


def enc_props(leg):
    """x264enc's property list for a leg (the profile is a caps filter, not a property)."""
    merged = {**COMMON, **{k: v for k, v in leg["props"].items() if k != "profile"}}
    out = []
    for k, v in merged.items():
        out.append(f'{k}="{v}"' if k == "option-string" else f"{k}={v}")
    return " ".join(out)


# ---------------------------------------------------------------- pure helpers (selftest covers them)


def percentile(values, p):
    """Nearest rank (p in 0..100) of a non-empty list."""
    if not values:
        raise ValueError("percentile of nothing")
    s = sorted(values)
    k = max(0, min(len(s) - 1, math.ceil(p / 100.0 * len(s)) - 1))
    return s[k]


def nal_stats(data):
    """(total bytes, filler-NAL bytes) of an Annex B stream: NAL type 12 is x264's CBR filler."""
    starts = []
    i = 0
    n = len(data)
    while True:
        j = data.find(b"\x00\x00\x01", i)
        if j < 0 or j + 3 >= n:
            break
        starts.append(j + 3)
        i = j + 3
    filler = 0
    for idx, s in enumerate(starts):
        end = (starts[idx + 1] - 3) if idx + 1 < len(starts) else n
        if data[s] & 0x1F == 12:
            filler += end - s
    return n, filler


def x264_options(data):
    """The options string x264 writes into the stream's SEI, or None."""
    m = re.search(rb"x264 - core \d+[^\x00]*?options: ([^\x00]*)", data[:400_000])
    return m.group(0).decode("ascii", "replace") if m else None


def parse_vmaf(path):
    with open(path, encoding="utf-8") as f:
        j = json.load(f)
    frames = [fr["metrics"]["vmaf"] for fr in j["frames"]]
    return {
        "frames": len(frames),
        "mean": j["pooled_metrics"]["vmaf"]["mean"],
        "p1": percentile(frames, 1),
        "p5": percentile(frames, 5),
        "min": min(frames),
    }


# ---------------------------------------------------------------------------------- GStreamer helpers


def gst():
    import gi

    gi.require_version("Gst", "1.0")
    gi.require_version("GstApp", "1.0")
    gi.require_version("GstVideo", "1.0")
    from gi.repository import Gst, GstApp, GstVideo  # noqa: F401  (GstApp adds appsrc and appsink methods)

    Gst.init(None)
    return Gst, GstVideo


def run_to_eos(Gst, desc, timeout_s=900):
    pipe = Gst.parse_launch(desc)
    pipe.set_state(Gst.State.PLAYING)
    msg = pipe.get_bus().timed_pop_filtered(
        int(timeout_s * Gst.SECOND), Gst.MessageType.EOS | Gst.MessageType.ERROR
    )
    pipe.set_state(Gst.State.NULL)
    if msg is None:
        raise RuntimeError("pipeline timed out")
    if msg.type == Gst.MessageType.ERROR:
        err, dbg = msg.parse_error()
        raise RuntimeError(f"pipeline error: {err} ({dbg})")


def run(cmd, **kw):
    print("+", " ".join(cmd), flush=True)
    return subprocess.run(cmd, check=True, **kw)


# --------------------------------------------------------------------------------------------- clip


def cmd_clip(a):
    run(
        [
            "ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", a.url,
            "-frames:v", str(a.frames), "-pix_fmt", "yuv420p", "-f", "rawvideo", a.out,
        ]
    )
    size = os.path.getsize(a.out)
    if size != a.frames * FRAME_BYTES:
        raise SystemExit(f"clip {a.out}: {size} bytes, expected {a.frames} x {FRAME_BYTES}")
    print(f"clip {a.out}: {a.frames} frames, {size} bytes")


def cmd_synth(a):
    """Smoke only: a synthetic clip, so the plumbing runs in seconds."""
    run(
        [
            "ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i",
            f"testsrc2=size={W}x{H}:rate={FPS}", "-frames:v", str(a.frames), "-pix_fmt", "yuv420p",
            "-f", "rawvideo", a.out,
        ]
    )


# ------------------------------------------------------------------------------------------ quality


def encode_offline(Gst, leg, clip, frames, out_h264):
    prof = leg["props"]["profile"]
    desc = (
        f"filesrc location={clip} blocksize={FRAME_BYTES} ! "
        f"rawvideoparse width={W} height={H} format=i420 framerate={FPS}/1 colorimetry=bt709 ! "
        f"x264enc {enc_props(leg)} ! video/x-h264,profile={prof},stream-format=byte-stream ! "
        f"h264parse ! filesink location={out_h264}"
    )
    print("encode:", desc, flush=True)
    t0 = time.monotonic()
    c0 = time.process_time()
    run_to_eos(Gst, desc)
    return time.monotonic() - t0, time.process_time() - c0


def vmaf(ref, h264, frames, out_json):
    """libvmaf's own CLI (v3.2.0, built by the workflow from the pinned tag: Ubuntu's ffmpeg has no
    libvmaf) on the raw frames: the decoded stream against the uncompressed source."""
    dist = out_json + ".dist.yuv"
    run(
        [
            "ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-f", "h264", "-framerate", str(FPS),
            "-i", h264, "-pix_fmt", "yuv420p", "-f", "rawvideo", dist,
        ]
    )
    got = os.path.getsize(dist)
    if got != frames * FRAME_BYTES:
        raise SystemExit(f"decoded {got} bytes, the clip is {frames} frames of {FRAME_BYTES}")
    run(
        [
            os.environ.get("VMAF_BIN", "vmaf"), "--reference", ref, "--distorted", dist,
            "--width", str(W), "--height", str(H), "--pixel_format", "420", "--bitdepth", "8",
            "--model", "version=vmaf_v0.6.1", "--output", out_json, "--json",
            "--threads", str(os.cpu_count()),
        ]
    )
    os.remove(dist)
    r = parse_vmaf(out_json)
    if r["frames"] != frames:
        raise SystemExit(f"libvmaf scored {r['frames']} frames, the clip has {frames}")
    return r


def cmd_quality(a):
    Gst, _ = gst()
    clips = dict(c.split("=", 1) for c in a.clip)
    results = []
    checked = 0
    for leg in legs():
        if leg.get("paced_only"):
            continue
        for cname, cpath in clips.items():
            frames = os.path.getsize(cpath) // FRAME_BYTES
            with tempfile.TemporaryDirectory() as d:
                h264 = os.path.join(d, "enc.h264")
                wall, cpu = encode_offline(Gst, leg, cpath, frames, h264)
                with open(h264, "rb") as f:
                    data = f.read()
                total, filler = nal_stats(data)
                seconds = frames / FPS
                v = vmaf(cpath, h264, frames, os.path.join(d, "vmaf.json"))
            checked += v["frames"]
            row = {
                "leg": leg["id"], "clip": cname, **v,
                "kbps": total * 8 / seconds / 1000,
                "filler_share": filler / total if total else None,
                "encode_wall_s": wall, "encode_cpu_s": cpu,
                "x264_options": x264_options(data),
            }
            print(json.dumps(row), flush=True)
            results.append(row)
    if checked == 0:
        raise SystemExit("quality: zero frames scored")
    print(f"quality: scored {checked} frames over {len(results)} legs x clips")
    with open(a.out, "w", encoding="utf-8") as f:
        json.dump({"legs": legs(), "rows": results}, f, indent=1)


# ------------------------------------------------------------------------------------------- paced


class Feed:
    """The pre-loaded clip, played forward and back so the picture never jumps at the loop point."""

    def __init__(self, path):
        with open(path, "rb") as f:
            raw = f.read()
        self.n = len(raw) // FRAME_BYTES
        self.frames = [raw[i * FRAME_BYTES:(i + 1) * FRAME_BYTES] for i in range(self.n)]

    def at(self, k):
        period = max(1, 2 * (self.n - 1))
        i = k % period
        return self.frames[i if i < self.n else period - i]


def pipeline_desc(leg):
    if leg is None:  # the baseline: the feed's own cost, no encoder
        tail = "fakesink sync=false"
    else:
        tail = (
            f"x264enc name=enc {enc_props(leg)} ! video/x-h264,profile={leg['props']['profile']} ! "
            "h264parse name=parse config-interval=-1 ! "
            "video/x-h264,stream-format=avc,alignment=au ! "
            "appsink name=sink emit-signals=true sync=false max-buffers=0 drop=false"
        )
    return (
        f"appsrc name=src is-live=true format=time block=true max-bytes={FRAME_BYTES * 4} "
        f"caps=video/x-raw,format=I420,width={W},height={H},framerate={FPS}/1,colorimetry=bt709 ! {tail}"
    )


class Run:
    """One pipeline, fed at 30 fps wall time; records every output access unit."""

    def __init__(self, Gst, GstVideo, leg, feed):
        self.Gst, self.GstVideo, self.leg, self.feed = Gst, GstVideo, leg, feed
        self.pipe = Gst.parse_launch(pipeline_desc(leg))
        self.src = self.pipe.get_by_name("src")
        self.outs = []  # (wall, running_time_ns, key)
        self.lock = threading.Lock()
        self.sched = []
        self.push_lag_ms = []
        self.eos = threading.Event()
        sink = self.pipe.get_by_name("sink")
        if sink is not None:
            sink.connect("new-sample", self._on_sample)
        self.parse = self.pipe.get_by_name("parse")

    def _on_sample(self, sink):
        Gst = self.Gst
        s = sink.emit("pull-sample")
        b = s.get_buffer()
        rt = s.get_segment().to_running_time(Gst.Format.TIME, b.pts)
        if rt == Gst.CLOCK_TIME_NONE:
            rt = None  # outside its segment: counted and refused by the reader, never guessed
        key = not b.has_flags(Gst.BufferFlags.DELTA_UNIT)
        with self.lock:
            self.outs.append((time.monotonic(), rt, key))
        return Gst.FlowReturn.OK

    def start(self):
        Gst = self.Gst
        self.pipe.set_state(Gst.State.PLAYING)
        self.pipe.get_state(5 * Gst.SECOND)
        self.t0 = time.monotonic() + 0.2

    def latency_query_ms(self):
        Gst = self.Gst
        q = Gst.Query.new_latency()
        if self.pipe.query(q):
            live, mn, mx = q.parse_latency()
            return {"live": live, "min_ms": mn / 1e6, "max_ms": (mx / 1e6) if mx != Gst.CLOCK_TIME_NONE else None}
        return None

    def push(self, n):
        """Push frame n at its slot; returns the wall instant it was due."""
        Gst = self.Gst
        due = self.t0 + n / FPS
        wait = due - time.monotonic()
        if wait > 0:
            time.sleep(wait)
        buf = Gst.Buffer.new_wrapped(self.feed.at(n))
        buf.pts = Gst.util_uint64_scale(n, Gst.SECOND, FPS)
        buf.duration = Gst.util_uint64_scale(1, Gst.SECOND, FPS)
        t_in = time.monotonic()
        res = self.src.push_buffer(buf)
        if res != Gst.FlowReturn.OK:
            raise RuntimeError(f"push_buffer {n}: {res}")
        self.sched.append(due)
        self.push_lag_ms.append((t_in - due) * 1000)
        return due

    def rt_of(self, n):
        return self.Gst.util_uint64_scale(n, self.Gst.SECOND, FPS)

    def close(self):
        self.pipe.set_state(self.Gst.State.NULL)

    def snapshot(self):
        with self.lock:
            return list(self.outs)


def stats_ms(v):
    if not v:
        return None
    return {
        "n": len(v), "mean": statistics.fmean(v), "p50": percentile(v, 50),
        "p95": percentile(v, 95), "max": max(v),
    }


def free_run(Gst, GstVideo, leg, feed, seconds):
    """Steady 30 fps, no forced keys: cores (this process) and the added encode latency."""
    r = Run(Gst, GstVideo, leg, feed)
    r.start()
    total = int(seconds * FPS)
    warm = min(5 * FPS, total // 4)
    tail = min(3 * FPS, total // 4)
    cpu0 = wall0 = None
    for n in range(total):
        r.push(n)
        if n == warm:
            cpu0, wall0 = time.process_time(), time.monotonic()
    cpu1, wall1 = time.process_time(), time.monotonic()
    out = {
        "leg": leg["id"] if leg else "_baseline",
        "cores": (cpu1 - cpu0) / (wall1 - wall0),
        "frames": total,
        "push_lag_ms": stats_ms(r.push_lag_ms[warm:]),
    }
    if leg is not None:
        out["gst_latency"] = r.latency_query_ms()
        time.sleep(3.0)  # let the lookahead's tail come out
        outs = r.snapshot()
        lat = []
        for wall, rt, key in outs:
            if rt is None:
                raise SystemExit(f"{out['leg']}: an access unit with no running time")
            n = round(rt * FPS / Gst.SECOND)
            if not 0 <= n < len(r.sched):
                raise SystemExit(f"{out['leg']}: running time {rt} is frame {n}, outside the {len(r.sched)} pushed")
            if warm <= n < total - tail:
                lat.append((wall - r.sched[n]) * 1000)
        out["latency_ms"] = stats_ms(lat)
        out["access_units"] = len(outs)
        out["keys"] = sum(1 for o in outs if o[2])
        out["checked_frames"] = len(lat)
        out["behind_real_time"] = max(r.push_lag_ms[warm:]) > 100 if len(r.push_lag_ms) > warm else None
        if not lat:
            raise SystemExit(f"{out['leg']}: no latency sample, nothing was measured")
    r.close()
    return out


def key_in_flight(outs, gop_frames=GOP_FRAMES, delay_frames=DELAY_FRAMES):
    """`gst_output::ask_key`'s refusal on the observed outputs (wall, running_time_ns, key), as the
    reason the product asks nothing, else None:

    - "first_frame": no video out yet (the ring has taken none, the product's `None` branch). The
      encoder's FIRST frame, a keyframe, starts the stream, so the sink waits for key 0, not for any
      later one (the review's I2: this used to be timed against the next natural key, 3.0 s on).
    - "in_flight": the GOP's own keyframe is due within the delay's frames: `gop_frames - since`
      frames on, `since` the frames out since the last keyframe out (0 at the keyframe), 1 to
      `delay_frames`. That keyframe serves the sink.
    """
    outs = [o for o in outs if o[1] is not None]
    if not outs:
        return "first_frame"
    outs.sort(key=lambda o: o[1])
    keys = [i for i, o in enumerate(outs) if o[2]]
    if not keys:
        return None
    due_in = gop_frames - (len(outs) - 1 - keys[-1])
    return "in_flight" if 0 < due_in <= delay_frames else None


def key_episode(Gst, GstVideo, leg, feed, offset_s):
    """One fresh encoder; a forced key asked `offset_s` after its first frame, the way
    `gst_programme.rs` asks (an upstream force-key-unit event from the parser's src pad, running time
    just after the newest frame, all headers)."""
    r = Run(Gst, GstVideo, leg, feed)
    r.start()
    n_req = max(1, round(offset_s * FPS))
    req_rt = r.rt_of(n_req)
    t_send = None
    deadline_frames = n_req + 7 * FPS
    n = 0
    forced = None
    skipped = None
    serves_from = req_rt
    while n < deadline_frames:
        refusal = key_in_flight(r.snapshot()) if n == n_req and leg.get("guard") else None
        if refusal:
            # The product asks nothing here (`gst_output::ask_key`): a key it already has serves. With
            # no video out yet that is the encoder's first frame (key 0, however early its running
            # time); in the delay it is the GOP's own, after the request's time.
            skipped = refusal
            if refusal == "first_frame":
                serves_from = 0
            t_send = time.monotonic()
        elif n == n_req:
            ev = GstVideo.video_event_new_upstream_force_key_unit(req_rt, True, 0)
            t_send = time.monotonic()
            if not r.parse.get_static_pad("src").send_event(ev):
                r.close()
                return {"offset_s": offset_s, "forced_key": None, "event_refused": True}
        r.push(n)
        n += 1
        if t_send is not None:
            keys = [o for o in r.snapshot() if o[2] and o[1] is not None and o[1] >= serves_from]
            if keys:
                forced = min(keys, key=lambda o: o[1])
                break
    row = {"offset_s": offset_s, "skipped": skipped}
    if forced is None:
        row["forced_key"] = None
    else:
        time.sleep(0.5)
        all_keys = sorted(o[1] for o in r.snapshot() if o[2] and o[1] is not None)
        prior = [k for k in all_keys if k < forced[1]]
        row.update(
            forced_key=True,
            delay_ms=(forced[0] - t_send) * 1000,
            held_ms=(forced[1] - req_rt) / 1e6,
            gop_ms=((forced[1] - prior[-1]) / 1e6) if prior else None,
            keys_rt_ms=[k / 1e6 for k in all_keys],
        )
    r.close()
    return row


def cmd_paced(a):
    Gst, GstVideo = gst()
    feed = Feed(a.clip)
    print(f"feed: {feed.n} frames")
    all_legs = [lg for lg in legs() if not lg.get("quality_only")]
    free = {}
    order = [None] + all_legs
    for rnd in range(a.rounds):
        seq = order if rnd % 2 == 0 else list(reversed(order))  # F-033: round 2 in reverse order
        for leg in seq:
            row = free_run(Gst, GstVideo, leg, feed, a.seconds)
            row["round"] = rnd + 1
            print(json.dumps(row), flush=True)
            free.setdefault(row["leg"], []).append(row)
    base = statistics.fmean(r["cores"] for r in free["_baseline"])
    episodes = {}
    offsets = [float(x) for x in a.offsets.split(",")]
    for leg in all_legs:
        rows = []
        for rep in range(a.reps):
            for off in offsets:
                row = key_episode(Gst, GstVideo, leg, feed, off)
                row["rep"] = rep + 1
                print(json.dumps({"leg": leg["id"], **row}), flush=True)
                rows.append(row)
        episodes[leg["id"]] = rows
    checked = sum(len(v) for v in episodes.values()) + sum(len(v) for v in free.values())
    if checked == 0:
        raise SystemExit("paced: nothing measured")
    print(f"paced: {checked} runs checked")
    with open(a.out, "w", encoding="utf-8") as f:
        json.dump(
            {"baseline_cores": base, "free": free, "episodes": episodes, "legs": legs(),
             "runner_cpus": os.cpu_count()},
            f, indent=1,
        )


# ------------------------------------------------------------------------------------------- table


def fmt(x, nd=1):
    return "n/a" if x is None else f"{x:.{nd}f}"


def cmd_table(a):
    out = []
    names = {lg["id"]: lg["name"] for lg in legs()}
    if a.quality:
        q = json.load(open(a.quality, encoding="utf-8"))
        clips = sorted({r["clip"] for r in q["rows"]})
        out.append("### Quality: VMAF mean (1st percentile, 5th percentile), x264enc offline, 1080p30 6000 kbps\n")
        out.append("| Leg | " + " | ".join(clips) + " | kbps (filler share) |")
        out.append("|---|" + "---|" * (len(clips) + 1))
        for lid in names:
            cells, kb = [], []
            for c in clips:
                rr = [r for r in q["rows"] if r["leg"] == lid and r["clip"] == c]
                if not rr:
                    cells.append("n/a")
                    continue
                r = rr[0]
                cells.append(f"{r['mean']:.2f} ({r['p1']:.1f}, {r['p5']:.1f})")
                kb.append(f"{r['kbps']:.0f} ({100 * (r['filler_share'] or 0):.1f} %)")
            out.append(f"| {lid} {names[lid]} | " + " | ".join(cells) + f" | {' / '.join(kb)} |")
        out.append("")
        out.append("x264 options each leg really got (the stream's own SEI):\n")
        seen = set()
        for r in q["rows"]:
            if r["leg"] not in seen:
                seen.add(r["leg"])
                out.append(f"- {r['leg']}: `{r['x264_options']}`")
        out.append("")
    if a.paced:
        p = json.load(open(a.paced, encoding="utf-8"))
        base = p["baseline_cores"]
        out.append(f"### Paced 30 fps run on a {p['runner_cpus']}-vCPU runner (the feed's own {base:.2f} cores subtracted)\n")
        out.append("| Leg | encoder cores (rounds) | latency p50 / p95 / max (ms) | gst latency query min (ms) |")
        out.append("|---|---|---|---|")
        for lid in names:
            rows = p["free"].get(lid)
            if not rows:
                continue
            cores = [r["cores"] - base for r in rows]
            lat = [r["latency_ms"] for r in rows]
            qmin = rows[0].get("gst_latency", {}) or {}
            out.append(
                f"| {lid} | {statistics.fmean(cores):.2f} ({', '.join(f'{c:.2f}' for c in cores)}) | "
                f"{fmt(statistics.fmean(x['p50'] for x in lat), 0)} / "
                f"{fmt(statistics.fmean(x['p95'] for x in lat), 0)} / {fmt(max(x['max'] for x in lat), 0)} | "
                f"{fmt(qmin.get('min_ms'), 0)} |"
            )
        out.append("")
        out.append(
            "### Forced keyframes: a request at each offset after the first frame "
            f"(product floor {PRODUCT_FLOOR_MS} ms; Cloudflare's {CF_GOP_FLOOR_MS} ms)\n"
        )
        offs = sorted({e["offset_s"] for rows in p["episodes"].values() for e in rows})
        out.append("| Leg | " + " | ".join(f"+{o:g} s: delay ms (GOP ms)" for o in offs) + " | shortest GOP ms | GOPs under 2000 |")
        out.append("|---|" + "---|" * (len(offs) + 2))
        for lid in names:
            rows = p["episodes"].get(lid)
            if not rows:
                continue
            cells, gops = [], []
            for o in offs:
                rs = [e for e in rows if e["offset_s"] == o]
                got = [e for e in rs if e.get("forced_key")]
                if not got:
                    cells.append("NO KEY")
                    continue
                d = statistics.fmean(e["delay_ms"] for e in got)
                g = [e["gop_ms"] for e in got if e["gop_ms"] is not None]
                gops += g
                sk = sum(1 for e in got if e.get("skipped"))
                first = sum(1 for e in got if e.get("skipped") == "first_frame")
                cells.append(
                    f"{d:.0f} ({fmt(min(g), 0) if g else '-'})"
                    + ("" if len(got) == len(rs) else f" {len(got)}/{len(rs)}")
                    + (f" [{sk} not asked" + (f", {first} served by key 0" if first else "") + "]" if sk else "")
                )
            under = sum(1 for g in gops if g < CF_GOP_FLOOR_MS)
            out.append(f"| {lid} | " + " | ".join(cells) + f" | {fmt(min(gops) if gops else None, 0)} | {under} of {len(gops)} |")
        out.append("")
    text = "\n".join(out)
    print(text)
    if a.out:
        with open(a.out, "w", encoding="utf-8") as f:
            f.write(text + "\n")


# ----------------------------------------------------------------------------------------- selftest


def cmd_selftest(_):
    # percentile: nearest rank over a known list (expected from the definition, not from the code)
    v = list(range(1, 101))
    assert percentile(v, 1) == 1 and percentile(v, 5) == 5 and percentile(v, 50) == 50 and percentile(v, 100) == 100
    assert percentile([7], 1) == 7
    try:
        percentile([], 50)
        raise AssertionError("empty percentile must refuse")
    except ValueError:
        pass
    # nal_stats: three NALs, the middle a 5-byte filler (type 12) after a 4-byte start code's 00 00 01
    stream = b"\x00\x00\x00\x01\x67aa" + b"\x00\x00\x01\x0c\xff\xff\xff\xff" + b"\x00\x00\x01\x65bb"
    total, filler = nal_stats(stream)
    assert total == len(stream) and filler == 5, (total, filler)
    assert nal_stats(b"") == (0, 0)
    # x264 SEI
    sei = b"\x00\x00\x01\x06x264 - core 164 r3108 - H.264/MPEG-4 AVC codec - options: cabac=1 ref=3\x00tail"
    assert x264_options(sei).endswith("cabac=1 ref=3")
    assert x264_options(b"nothing here") is None
    # legs: five of the brief, one extra, R0, one profile each, the product's floor and GOP carried
    ls = legs()
    assert [lg["id"] for lg in ls] == ["L1", "L2", "L3", "L4", "L5", "L5a", "X1", "P1", "R0"]
    assert COMMON["key-int-max"] == 3 * FPS and COMMON["min-force-key-unit-interval"] == PRODUCT_FLOOR_MS * 1_000_000
    p = {lg["id"]: lg["props"] for lg in ls}
    assert p["L1"]["tune"] == "zerolatency" and "tune" not in p["L2"]
    assert (p["L2"]["rc-lookahead"], p["L3"]["rc-lookahead"], p["L4"]["bframes"]) == (15, 20, 2)
    assert "nal-hrd=vbr" in p["L5"]["option-string"] and "filler" not in p["L5"]["option-string"]
    assert p["L5a"]["option-string"].count("filler=0") == 1 and "filler=1" in COMMON["option-string"]
    assert 'option-string="nal-hrd=cbr:filler=0:scenecut=0"' in enc_props(ls[5])
    # key_in_flight, from the rule in frames: keyframe 0 of a 90-frame GOP, a 24-frame delay (outputs as
    # (wall, running_time_ns, key)): in flight for the newest frames 66..89 and no other; nothing out
    # yet is the first-frame refusal (the product asks nothing before its first video).
    ns = lambda n: n * 1_000_000_000 // FPS
    def outs(newest, stamp=lambda i: ns(i)):
        return [(0.0, stamp(i), i == 0) for i in range(newest + 1)]
    yes = [n for n in range(0, 180) if key_in_flight(outs(n)) == "in_flight"]
    assert yes == list(range(66, 90)), yes
    assert key_in_flight([]) == "first_frame"
    assert key_in_flight([(0.0, ns(3), False)]) is None
    # Frames lost before the encoder move the stamps, not the count: 30 frames missing from the stamps
    # inside the GOP leave the same frames in flight (the review's I3; a time rule moves them).
    lost = lambda i: ns(i + 30)
    assert [n for n in range(0, 180) if key_in_flight(outs(n, lost)) == "in_flight"] == yes
    # A later keyframe resets the count (a forced key, which x264 counts from).
    reset = outs(100) + [(0.0, ns(101), True)] + [(0.0, ns(102 + i), False) for i in range(10)]
    assert key_in_flight(reset) is None
    print("selftest: ok")


def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    c = sub.add_parser("clip")
    c.add_argument("--url", required=True)
    c.add_argument("--out", required=True)
    c.add_argument("--frames", type=int, default=220)
    c.set_defaults(fn=cmd_clip)
    s = sub.add_parser("synth")
    s.add_argument("--out", required=True)
    s.add_argument("--frames", type=int, default=30)
    s.set_defaults(fn=cmd_synth)
    q = sub.add_parser("quality")
    q.add_argument("--clip", action="append", required=True, help="NAME=path.yuv")
    q.add_argument("--out", required=True)
    q.set_defaults(fn=cmd_quality)
    pc = sub.add_parser("paced")
    pc.add_argument("--clip", required=True)
    pc.add_argument("--out", required=True)
    pc.add_argument("--seconds", type=float, default=45)
    pc.add_argument("--rounds", type=int, default=2)
    pc.add_argument("--reps", type=int, default=2)
    pc.add_argument("--offsets", default="0.1,0.5,1.0,1.5,2.0,2.3,2.6,2.8,3.0,3.4")
    pc.set_defaults(fn=cmd_paced)
    t = sub.add_parser("table")
    t.add_argument("--quality")
    t.add_argument("--paced")
    t.add_argument("--out")
    t.set_defaults(fn=cmd_table)
    st = sub.add_parser("selftest")
    st.set_defaults(fn=cmd_selftest)
    a = ap.parse_args()
    a.fn(a)


if __name__ == "__main__":
    main()
