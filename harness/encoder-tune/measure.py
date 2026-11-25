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
            "name": "as L1, filler=0",
            "props": {**today, "option-string": "nal-hrd=cbr:filler=0:scenecut=0"},
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
    from gi.repository import Gst, GstVideo  # noqa: F401

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
    lavfi = (
        "[0:v]setpts=PTS-STARTPTS,format=yuv420p[d];[1:v]setpts=PTS-STARTPTS[r];"
        f"[d][r]libvmaf=log_fmt=json:log_path={out_json}:n_threads={os.cpu_count()}"
    )
    run(
        [
            "ffmpeg", "-hide_banner", "-loglevel", "error", "-y",
            "-f", "h264", "-framerate", str(FPS), "-i", h264,
            "-f", "rawvideo", "-pix_fmt", "yuv420p", "-s", f"{W}x{H}", "-framerate", str(FPS), "-i", ref,
            "-lavfi", lavfi, "-f", "null", "-",
        ]
    )
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
    warm = 5 * FPS
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
            if warm <= n < total - 3 * FPS:
                lat.append((wall - r.sched[n]) * 1000)
        out["latency_ms"] = stats_ms(lat)
        out["access_units"] = len(outs)
        out["keys"] = sum(1 for o in outs if o[2])
        out["checked_frames"] = len(lat)
        if not lat:
            raise SystemExit(f"{out['leg']}: no latency sample, nothing was measured")
    r.close()
    return out


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
    while n < deadline_frames:
        if n == n_req:
            ev = GstVideo.video_event_new_upstream_force_key_unit(req_rt, True, 0)
            t_send = time.monotonic()
            if not r.parse.get_static_pad("src").send_event(ev):
                r.close()
                return {"offset_s": offset_s, "forced_key": None, "event_refused": True}
        r.push(n)
        n += 1
        if t_send is not None:
            keys = [o for o in r.snapshot() if o[2] and o[1] is not None and o[1] >= req_rt]
            if keys:
                forced = min(keys, key=lambda o: o[1])
                break
    row = {"offset_s": offset_s}
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
                cells.append(f"{d:.0f} ({min(g):.0f})" + ("" if len(got) == len(rs) else f" {len(got)}/{len(rs)}"))
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
    # legs: five of the brief plus R0, one profile each, the product's floor and GOP carried
    ls = legs()
    assert [lg["id"] for lg in ls] == ["L1", "L2", "L3", "L4", "L5", "R0"]
    assert COMMON["key-int-max"] == 3 * FPS and COMMON["min-force-key-unit-interval"] == PRODUCT_FLOOR_MS * 1_000_000
    p = {lg["id"]: lg["props"] for lg in ls}
    assert p["L1"]["tune"] == "zerolatency" and "tune" not in p["L2"]
    assert (p["L2"]["rc-lookahead"], p["L3"]["rc-lookahead"], p["L4"]["bframes"]) == (15, 20, 2)
    assert p["L5"]["option-string"].count("filler=0") == 1 and "filler=1" in COMMON["option-string"]
    assert 'option-string="nal-hrd=cbr:filler=0:scenecut=0"' in enc_props(ls[4])
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
    pc.add_argument("--offsets", default="0.1,0.5,1.0,1.5,2.0,2.3,2.6")
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
