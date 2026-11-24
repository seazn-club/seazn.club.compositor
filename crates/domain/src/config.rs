//! THE number authority. Every tunable lives here, each with the source that set it.
//! "PDF" = Live Sports Streaming Compositor — Production Design & Specification (2026-10-03).
//! "Spec" = `docs/specs/2026-10-14-r2-compositor-design.md`; "plan" = its Global Constraints.
//! "C02 brief default" = a number the C02 brief chose where no PDF, spec, plan or spike number exists.
//! A spike measurement (F-id in `_FINDINGS.md`) overrides a PDF value and must be cited.
use crate::time::Ms;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Thresholds {
    /// PDF robustness state machine: "no frame 0.5 s" → Frozen.
    pub frozen_after_ms: Ms,
    /// PDF state machine "no frame 3 s" → Reconnecting slate; timing budget "slate ≤ 3.1 s". The
    /// owner's ruling, 2026-11-24 (C3, S5 item 3: "keep the frozen picture, 4.5 s slate"): 4.5 s. The
    /// phone publishes a 3 s GOP (C1, F-036), and with `avdec_h264`'s `output-corrupt=false` (Owed 48)
    /// a lost reference holds the last good picture until the next keyframe, up to one GOP; at 3 s the
    /// slate went up on every such freeze. One phone GOP and a 1.5 s margin, so a freeze that ends at
    /// the GOP's keyframe never slates. The budget moves with it (`slate_budget_ms`).
    pub slate_after_ms: Ms,
    /// PDF: "a return to live requires 1 s of continuous frames".
    pub resume_after_ms: Ms,
    /// PDF failure table: input back-off 1 s → 10 s, retried forever. The first wait.
    pub input_backoff_base_ms: Ms,
    /// The cap. PDF failure table: input back-off 1 s → 10 s. F-036 (stg 2026-11-24, session
    /// `5e5403ce`; the C2 brief's S7): 4 s. Cloudflare had the camera connected at 09:55:31.6Z, and the
    /// relay built gens 4 to 9 with the back-off's 8 and 10 s steps between its connects, so the
    /// reconnecting slate stayed on air about 50 s with the camera back. At 4 s a dead pull is retried
    /// at most a connect timeout and 4 s apart (13 s), not 19 s; the first three waits (1, 2, 4 s) and
    /// F-036's hold-end schedule are unchanged.
    pub input_backoff_cap_ms: Ms,
    /// Spec §4.2: a connect that neither succeeds nor fails within this is treated as lost. C10 times
    /// "connected" to the first decoded frame. F-002 kept 5 s with a thin margin (the slowest healthy
    /// first decoded frame 4.19 s, F-002; the slowest first AU 4.4 s, F-004 rerun 1). F-036 (stg,
    /// 2026-11-24; the C1 brief's rule): 9 s. A phone that republishes with its 3 s GOP behind the 1 s
    /// SRT latency can take over 5 s to a first decoded frame (derived: F-032's 4.2 s behind a 2 s
    /// GOP, plus the GOP's extra second, is 5.2 s; the brief's estimate is 6 to 8 s), and at 5 s the
    /// relay gave up six connects in a row while the camera was back.
    pub input_connect_timeout_ms: Ms,
    /// Spec §6 "reconnect forever": a Running input that delivers no frame for this long is treated as
    /// a Stalled loss and rebuilt. Set after the slate so the viewer sees the slate first (the slate
    /// moved to 4.5 s by the owner's ruling of 2026-11-24, and this stays: `stall_comes_after_the_slate`
    /// pins the gap at a tick at least, and the take-back (C3, S2) makes a stall that ends inside the
    /// back-off cost no rebuild). F-002
    /// confirmed it is needed: CF holds a silent, encrypted playback socket open indefinitely after the
    /// phone stops publishing.
    pub input_stall_after_ms: Ms,
    /// C3 (S2 item 2): while bytes still leave the camera's SRT source (`input_bytes_fresh_ms`) and no
    /// frame decodes, a Running input is waiting on its decoder, not on a dead socket, so the stall
    /// rule waits this long instead of `input_stall_after_ms`. Derived (`input_decode_wait_ms`): the
    /// stall and one phone GOP (`CAMERA_GOP_MS`). With `output-corrupt=false` (Owed 48) a lost
    /// reference withholds every frame to the next keyframe, and a lost keyframe to the one after
    /// it: up to two GOPs (6 s) from the last good frame, which this clears; a decoder wedged with
    /// bytes still arriving is rebuilt here, 3 s later than a silent one. A silent socket is never
    /// extended: Cloudflare's held broadcast carries no byte (F-032) and a dead pull none (F-002).
    pub input_decode_wait_ms: Ms,
    /// C3 (S2 item 2): bytes "still arrive" while the build's source byte count rose within this
    /// long. One SRT latency (`input_srt_latency_ms`, F-002's 1 000 ms): libsrt's live mode hands
    /// each packet on a latency after it was sent, so a sender that still publishes moves the count
    /// many times inside it (a 1 316 B payload per SRT packet; the phone's audio alone fills several
    /// a second), and a whole latency with no byte is a sender gone quiet. The driver reads the
    /// count once per tick (C08), well inside it (`TICK_WIDEST_MS`). C3's choice: the stg counters
    /// read it (Owed 111).
    pub input_bytes_fresh_ms: Ms,
    /// C10: the SRT receive latency the input pipeline asks of CF Input A's playback (`srtsrc`
    /// `latency`, in its URI). The POC's `INPUT_LATENCY_MS` (1 000). F-002 measured every CF reader
    /// at 1 000 ms: 0 packets lost or retransmitted in 120 s, the first AU at 2.22–3.89 s, and an
    /// in-chain camera→overlay delay of 3 083–3 105 ms that includes it, so the overlay delay's
    /// default (F-002, ADR 0006) moves with this value.
    pub input_srt_latency_ms: Ms,
    /// F-032's recommended fix (a) (Owed 108, `_FINDINGS.md`; the dispatch of 2026-10-26):
    /// Cloudflare's SRT playback holds a dropped publisher's broadcast about 30 s, then closes every
    /// attached reader with an error. An input error at least this long after the last frame, on a
    /// build whose handshake completed and that carried no media, is read as that close: the link
    /// rebuilds a base wait later, its attempt count reset, once an outage (`input_link`). F-032's
    /// bound: the errors at a new publisher's arrival come about 4 s after the last frame, no other
    /// error fell between the drop + 10 s and the close, and the close came 27.9 to 29.2 s after the
    /// last frame, which leaves 8 s of margin.
    pub input_hold_end_after_ms: Ms,
    /// PDF failure table: "alert if it repeats > 3 times in 5 min" (alert when count EXCEEDS this).
    pub kick_alert_count: usize,
    /// PDF failure table: the "5 min" of "> 3 times in 5 min".
    pub kick_window_ms: Ms,
    /// C02 brief default, sized to the PDF budget "output reconnect ≤ 5 s": back-off 0.5 s → 2 s keeps
    /// attempts inside it. The first wait.
    pub output_backoff_base_ms: Ms,
    /// C02 brief default, sized to the PDF budget "output reconnect ≤ 5 s": the 2 s cap.
    pub output_backoff_cap_ms: Ms,
    /// Spec §4.2: "a reconnect is expected to complete within 5 s". F-004 addendum: stall detection,
    /// one capped back-off and one whole connect must stay under YouTube's 15 s broadcast split.
    pub output_connect_timeout_ms: Ms,
    /// F-011 (Owed 38; the coordinator's ruling): a sink is Connected at its first growth at least
    /// this long after its first growth, with no error between (since Owed 87 a silence between is no
    /// failure and does not restart the proof: while the encoders starve or are unread, only the sink's
    /// error ends it; with both delivering, Owed 94's backstop ends a sink flat for
    /// `output_egress_flat_ms`). The
    /// first nonzero `out-bytes-total` is the 476 B RTMP `connect` command (22–69 ms after the build,
    /// n = 10 on CF), and a REFUSED publish keeps counting until its refusal arrives: 262 334 B by
    /// 538 ms on CF (a made-up key) and 1 008 779 B over 2 033 ms on mediamtx (a denied path). No byte
    /// floor tells those apart; time does. 5 s is about 2.5× the slowest refusal measured. It is not
    /// longer because the first `playing` beat and the back-off's reset both wait on Connected.
    pub output_connected_after_ms: Ms,
    /// Owed 94 (the owner, 2026-10-24, "B", to the coordinator's question: a backstop that swaps the
    /// sink after about 15 s with no egress bytes while the encoders deliver, the beat saying so): a
    /// Proving or Connected sink whose bytes-sent total has not grown for this long, while both
    /// encoders kept delivering, is swapped (`SwapReason::EgressStalled`). It cures a hang inside the
    /// output pipeline that no sink error reports (Owed 94 (ii)). Derived, in one place
    /// (`output_egress_flat_ms`), from F-004's 15 s budget: the longest threshold, on the poll's grid,
    /// that still leaves the swap's back-off and one whole connect inside it. The owner's figure was
    /// "about 15 s"; that would put the viewer's gap near 20 s, past F-004's split. The owner,
    /// 2026-10-25, in chat, relayed by the coordinator ("Apply rec", the coordinator's recommendation
    /// of this derived value): 9 s, keeps the worst outage under 15 s.
    pub output_egress_flat_ms: Ms,
    /// Spec §4.2: 3 CEF failures in 5 min → graphics Failed.
    pub cef_fail_count: usize,
    /// Spec §4.2: the "5 min" of "3 CEF failures in 5 min".
    pub cef_fail_window_ms: Ms,
    /// PDF page contract "loads in under 2 s"; ×5 margin before it counts as a failure. F-001 measured
    /// cefsrc start → first buffer at 2.781–2.804 s, well inside it.
    pub cef_load_timeout_ms: Ms,
    /// Spec §6: CEF recycled every 2 h with its last frame held.
    pub cef_recycle_every_ms: Ms,
    /// C02 brief default: spec §6 says "restart with backoff" and names no number, so CEF mirrors the
    /// input back-off, 1 s → 10 s. The first wait.
    pub cef_backoff_base_ms: Ms,
    /// C02 brief default: the cap of the CEF back-off (1 s → 10 s, mirroring the input's PDF numbers;
    /// F-036 moved the input's cap to 4 s for the camera's return, which says nothing of CEF's, so
    /// this one stays).
    pub cef_backoff_cap_ms: Ms,
    /// R2005: the overlay page logs `seazn:alive` this often after ready, on F-001's console channel
    /// (`docs/_INDEX.md` Owed 18, owner 2026-10-18). The domain does not read it: it is the page's
    /// contract, kept here so its ratio to `cef_hang_after_ms` is pinned in one place.
    pub cef_alive_every_ms: Ms,
    /// C05b (Owed 18): a Ready page silent this long is hung, a counted failure that hides the overlay
    /// at once. A hung page paints nothing new and cefsrc emits only on paint (F-001), so frame silence
    /// is no signal; the beat is. Five beats, equal to the page's SCORE_STALE_AFTER_MS (spec §6);
    /// shorter turns CPU starvation (CEF children run at nice 10, so beats slip under load) into
    /// restarts whose load spikes feed the ladder.
    pub cef_hang_after_ms: Ms,
    /// Spec §6: CEF's memory is capped; above the cap C11 reports `CefCrashed` and the domain restarts
    /// the page. C11 reads it as the PSS sum of CEF's child processes (F-013: their RSS sum counts the
    /// zygote's shared pages once per process, 436 MB for one healthy page against 173 MB of PSS).
    pub cef_rss_cap_mb: u64,
    /// Owner ruling 2026-10-19 "B" (Owed 55; C11b): a teardown that never returns leaks its build's
    /// thread, pipeline and renderer (about 22–38 MB PSS each, the PR #32 review's P6). The process
    /// restarts in place once this many are leaked AT ONCE: a live count, so a leak whose teardown
    /// returns after all no longer counts (the coordinator's recommendation, matching C11's gauge).
    /// Fewer stay until the process ends.
    pub cef_leaks_to_restart: usize,
    /// F-013 / Owed 61 (C11b): Chromium aborts the whole process AT its `CHROMIUM_GPU_DEATHS_ABORT`-th
    /// GPU-process death. The process restarts in place at this many, with two survivable deaths
    /// after it (`+ 2 < CHROMIUM_GPU_DEATHS_ABORT`): one the 1 s child poll has not read yet, and one
    /// during the exit (the coordinator's decision on the PR #34 review's I1, 2026-10-20; was 4, whose
    /// second death after it is the abort itself).
    pub cef_gpu_deaths_to_restart: usize,
    /// C02 brief default (no PDF number): overruns within `ladder_window_ms` needed to step down a rung.
    pub ladder_overruns_to_step: usize,
    /// C02 brief default (no PDF number): the window `ladder_overruns_to_step` is counted in.
    pub ladder_window_ms: Ms,
    /// R1 sends SIGINT and SIGKILLs after RUNNER_STOP_GRACE_SECONDS (seazn.club
    /// `apps/web/src/server/relay/config.ts`): 20 s since the owner's D15 ruling (2026-10-16), which
    /// fits the final hold, this drain and the headroom. The drain must finish with headroom for EOS to
    /// reach the sink and the process to exit: 3 s. C12's `final_hold_and_drain_fit_inside_r1_stop_grace`
    /// pins `final_hold_ms + drain_timeout_ms + 3 s` inside R1's grace.
    pub drain_timeout_ms: Ms,
    /// Overlay templates spec 2026-10-15 §4.2 (D4): FINAL_HOLD_MS, how long the Final scene holds before
    /// EOS. C12's `final_hold_and_drain_fit_inside_r1_stop_grace` pins final_hold + drain + 3 s inside
    /// R1's grace.
    pub final_hold_ms: Ms,
    /// Spec §5.3 default (`heartbeatIntervalMs`: 5000); the bootstrap's `heartbeatIntervalMs` overrides it.
    pub heartbeat_interval_ms: Ms,
    /// C02 brief default (no PDF number): bootstrap GET retry back-off while web is unreachable at boot.
    /// The first wait.
    pub bootstrap_backoff_base_ms: Ms,
    /// C02 brief default (no PDF number): the cap of the bootstrap GET retry back-off.
    pub bootstrap_backoff_cap_ms: Ms,
}

/// The floor for "the encoder keeps real time". A 1 s window at 30 fps holds 29–31 frames
/// (0.967–1.033), so 1.0 would fail a third of healthy windows. It is the C00 cost gate's "encoder
/// speed ≥ 0.95" (`_FINDINGS.md`, Scope and gate); F-005's worst 10 s window was 0.9963. A const,
/// not a `Thresholds` field, because `Thresholds` derives `Eq`. C09's band and C14 case 6 both read it.
pub const ENCODER_SPEED_REALTIME_MIN: f64 = 0.95;

/// Owed 86 (the owner's "Ok", 2026-10-24, in chat, relayed by the coordinator, to the coordinator's
/// recommendation of a starvation alarm when the real output frame rate falls below a floor). The
/// floor, as a share of the profile's `fps`: the encoder's real-time floor (`ENCODER_SPEED_REALTIME_MIN`,
/// the C00 cost gate), read in frames rather than in PTS. No spec line or brief names a frame-rate
/// floor for the beat, so this share is a coordinator-medium decision, recommended in the Owed 86 PR.
/// `OutputProfile::fps_floor` applies it, and `crate::starvation` raises the alarm below it.
pub const OUTPUT_FPS_FLOOR_SHARE: f64 = ENCODER_SPEED_REALTIME_MIN;

/// Owed 86: the alarm's hysteresis. Once raised, it clears only on a beat at or above this share of
/// the profile's `fps`: C00 Step 4's gate (`fps ≥ 29.5` for its 30 fps page), which C14 cases 6 and
/// 10 also hold (`fps_mean ≥ 29.5`, "the clock holds"). Between the floor and this level the alarm
/// keeps its state, so a rate hovering at the floor does not raise and clear on alternate beats.
pub const OUTPUT_FPS_RECOVERED_SHARE: f64 = 29.5 / 30.0;

/// Owed 86: the shortest window the output's frame rate is read over, half the spec's 5 s beat
/// (§5.3). The first build took `ENCODER_SPEED_REALTIME_MIN`'s 1 s, which covers only the one-frame
/// quantum; the PR #69 review's m4 found an off-grid beat (the hold's first, the drain's) can close a
/// window just after the driver's catch-up burst, 2 to 3 frames short, which over 1 s reads 27 to 28,
/// under the floor on a healthy clock. Over 2.5 s, 3 frames short reads 28.8 (a 4th reads 28.4). A
/// beat sooner than this after the last reading repeats that reading and leaves the window open, so
/// the next beat reads over all of it (with a shorter bootstrap `heartbeatIntervalMs`, every other
/// beat reads).
pub const OUTPUT_FPS_MIN_WINDOW_MS: Ms = 2_500;

/// F-004 (owner-observed; docs/_INDEX.md decision log, the C00 owner session): YouTube kept the
/// organiser's broadcast through 5 s and 15 s output gaps and started a new broadcast after a 30 s
/// one. The longest gap it was seen to keep. A const, not a `Thresholds` field: it is YouTube's
/// behaviour, not a setting of ours. The process restart's budget (spec §6, "Output back ≤15 s") is
/// this number; the output's recovery paths add F-036's floor to it (`OUTPUT_GAP_BUDGET_MS`).
pub const YOUTUBE_KEPT_GAP_MS: Ms = 15_000;

/// Every output recovery path the broadcast must survive finishes its output gap UNDER this: F-004's
/// 15 s and the forced keyframe's floor (F-036; the C1 review's I2, the controller's ruling and the
/// owner's apply-rec, 2026-11-24). A recovering sink's picture comes back at the keyframe its swap
/// asks for, which the encoder holds to the floor after its last one: up to the floor after the new
/// sink's first bytes. The rest of each path keeps F-004's 15 s (`output_egress_flat_ms` takes the
/// floor back out, so the backstop's threshold does not move: a false hung declaration costs more
/// than the floor). 17.2 s is past the longest gap YouTube was seen to keep and short of the 30 s that
/// split the broadcast; the cost if YouTube splits between them is a documented worst gap 2.2 s
/// longer. Not in it: F-028's lost forced keyframe (a destination that drops it, seen once in two
/// MediaMTX runs, waits for the encoder's next own keyframe, a GOP after the forced one: up to
/// floor + GOP = 5.2 s after the reconnect, 2 s before F-036). No recovery path the domain models
/// loses a keyframe, so it is stated here, not budgeted.
pub const OUTPUT_GAP_BUDGET_MS: Ms = YOUTUBE_KEPT_GAP_MS + FORCED_KEYFRAME_MIN_INTERVAL_MS;

/// rtmp2sink's `timeout`, which C09 sets on every sink: the only detector of a destination stall (Owed 87;
/// the owner, 2026-10-24, "A": the output's stall rule is retired). rtmp2 counts a message's bytes only once
/// the whole message is written (`rtmpconnection.c:519-628`), so the rule's "no growth for 5 s" was the same
/// event as one socket wait of 5 s, and the socket's own timeout swapped 17 of 17 CI destination stalls
/// (docs/_INDEX.md Owed 87). It now answers the PDF's chaos row "destination accepts but stalls: detected
/// within 5 s" (C14 case 17). Today's default, pinned, not inherited: GStreamer 1.24.2 (tag 81e2548f),
/// `gst/rtmp2/gstrtmp2locationhandler.c:35` `#define DEFAULT_TIMEOUT 5`, the property's default ("RTMP
/// timeout in seconds", a uint, `:84-86`), which `rtmp/rtmpclient.c:419` hands to
/// `g_socket_client_set_timeout`: it bounds the TCP connect and every wait of the socket after it. A const,
/// not a `Thresholds` field: the domain decides nothing on it (the sink's error is its event).
pub const OUTPUT_SINK_TIMEOUT_MS: Ms = 5_000;

/// F-036: Cloudflare Stream's live input requirements
/// (<https://developers.cloudflare.com/stream/stream-live/start-stream-live/>, "Requirements": "GOP
/// duration (keyframe interval) must be between 2 and 10 seconds. Closed GOPs are required"). Outside
/// it the input errors (`live_input.errored`, `ERR_GOP_OUT_OF_RANGE`), as the programme input (our
/// x264 output) did twice on stg, sessions d908c90c and 5e5403ce, 2026-11-24. The floor.
pub const CF_KEYFRAME_INTERVAL_MIN_MS: Ms = 2_000;
/// F-036: Cloudflare's keyframe interval range, its ceiling (`CF_KEYFRAME_INTERVAL_MIN_MS`).
pub const CF_KEYFRAME_INTERVAL_MAX_MS: Ms = 10_000;

/// F-036: the programme's GOP, one keyframe every 3 s (the C1 brief): the capture app's own fix for
/// the same 2 s edge (it measured ±14 ms of jitter at 2 s), a second inside Cloudflare's floor. C09
/// sets x264enc's `key-int-max` to it in frames (`OutputProfile::gop_frames`) with x264's scene cut
/// off (`scenecut=0`, which x264 honours and its `min-keyint` cannot replace: x264 clamps that to
/// `key-int-max / 2 + 1`, F-036), so every GOP is this long, unless an output swap forces a keyframe
/// (Owed 84; `FORCED_KEYFRAME_MIN_INTERVAL_MS` bounds that one).
pub const PROGRAMME_GOP_MS: Ms = 3_000;

/// F-036 (the C1 review's I1, the controller's ruling 2026-11-24): the margin a forced keyframe keeps
/// over Cloudflare's floor. A held keyframe lands exactly at the floor (the programme stamps tick `i`
/// at `i × 10⁹ / fps` ns, and GstVideoEncoder throttles on a strict `>`), and whether Cloudflare takes
/// exactly 2 000 ms is not measured; the phone errored at 2 s ±14 ms. 200 ms: whole frames at 25, 30
/// and 60 fps, and a GOP less the floor still leaves Owed 84's tests their 16 ticks (`config` test).
pub const FORCED_KEYFRAME_MARGIN_MS: Ms = 200;

/// F-036: the shortest GOP a forced keyframe may cut, Cloudflare's floor and a margin. Owed 84's
/// output swap asks the encoder every output shares for a keyframe; asked within this of the last
/// keyframe, it is deferred to this interval, never dropped. C09 sets it as x264enc's
/// `min-force-key-unit-interval` (GstVideoEncoder, since 1.18), which holds the request in running
/// time, the stream's own clock: a wall-clock rule cannot see when the last keyframe was encoded.
pub const FORCED_KEYFRAME_MIN_INTERVAL_MS: Ms = CF_KEYFRAME_INTERVAL_MIN_MS + FORCED_KEYFRAME_MARGIN_MS;

/// The phone's GOP: the capture app publishes a keyframe every 3 s (the C1 brief; F-036, the same
/// 2 s edge `PROGRAMME_GOP_MS` moved off). The camera side's, not ours: the decode wait reads it
/// (`input_decode_wait_ms`), and the owner's 4.5 s slate is one of it and a margin.
pub const CAMERA_GOP_MS: Ms = 3_000;

/// C3 (S5 item 2): the most compressed camera video the queue before the decoder holds, as running
/// time. The queue no longer leaks: a dropped access unit loses a reference mid-GOP, and with
/// `output-corrupt=false` (Owed 48) the picture then freezes to the next keyframe, up to a whole phone
/// GOP; a full queue blocks the source instead, and libsrt's too-late drop is the one loss left on the
/// leg. The bound: one phone GOP (`CAMERA_GOP_MS`), so a decoder that waits a GOP for its keyframe
/// still has room, and one SRT latency (`input_srt_latency_ms`, F-002's 1 000 ms), the burst
/// libsrt releases after a network pause. A decoder wedged past it blocks the pull, and the stall
/// rule rebuilds it (the bytes stop with it, so no decode wait extends it). A full queue blocks
/// tsdemux, so the camera's audio stops with its video until the decoder drains or the rebuild.
// `+ 1_000` is `input_srt_latency_ms` (a `const` cannot read `Thresholds::default()`):
// `the_camera_video_queue_holds_a_phone_gop_and_one_srt_latency` pins the two equal.
pub const CAMERA_VIDEO_QUEUE_MS: Ms = CAMERA_GOP_MS + 1_000;

/// C3 (the C3 review's minor): the same queue's byte cap, GStreamer's own `max-size-bytes` default
/// (10 MiB) set explicitly. Time is the bound (`CAMERA_VIDEO_QUEUE_MS`; the queue's buffer cap is
/// off, since its default 200 would cut the bound to 3.3 s at 60 fps); the bytes are the backstop
/// for buffers with no timestamp, and hold the 4 s at up to 20 Mbit/s.
pub const CAMERA_VIDEO_QUEUE_BYTES: u32 = 10 * 1024 * 1024;

/// C3 (S2 item 2), derived in one place: the stall rule's bound while bytes still arrive, the stall
/// and one phone GOP (`Thresholds::input_decode_wait_ms`'s doc). A const fn, so the default and the
/// config test read the same terms.
pub const fn input_decode_wait_ms(input_stall_after_ms: Ms) -> Ms {
    input_stall_after_ms + CAMERA_GOP_MS
}

/// Owed 112 (C3b; chaos run 45480676592, `cef-hung`, an 813 ms head outage): the longest backlog of
/// the encoder's first frames that a sink whose publish lands after them starts from, at the
/// encoder's first keyframe, instead of dropping it (Owed 84 drops every frame queued before a
/// sink can send). Dropping it costs a head outage: the keyframe asked for then is held to
/// `FORCED_KEYFRAME_MIN_INTERVAL_MS` after frame 0 (F-036 fact 4), so the stream starts the floor
/// less the backlog late. Taking it costs the backlog as latency, sent at once (a burst, F-023's
/// 4 s there). Half the floor is where the two are equal, so whichever start is taken the head is
/// at most 1.1 s off. The run's race was 38 ms (one frame). Under the GOP (`PROGRAMME_GOP_MS`), so
/// only the encoder's first GOP is ever taken, and under the encoded ring's 2 s (C09, the plan's
/// Global Constraints), so the ring still holds it.
pub const FIRST_GOP_CATCH_UP_MAX_MS: Ms = FORCED_KEYFRAME_MIN_INTERVAL_MS / 2;

/// How often C09 reads the output's bytes-sent total (Owed 15, the choice recorded in `_FINDINGS.md`
/// F-011): 250 ms. The totals prove a new sink Connected (`output_connected_after_ms`, Owed 38), and
/// their flatness times the egress backstop (`output_egress_flat_ms`, Owed 94), which can see a total
/// stop growing only on this grid. Here, not in the adapters, because the backstop's budget reads it.
pub const OUTPUT_PROGRESS_POLL_MS: Ms = 250;

/// The widest gap between two of C08's ticks, which land on the frame period floored to the ms: 34 ms
/// at ADR 0005's 30 fps, in both modes. A timed step (a back-off ending, the backstop's threshold)
/// acts on the first tick at or after it, so up to this late.
pub const TICK_WIDEST_MS: Ms = {
    let (a, b) = (OutputProfile::P720.fps as Ms, OutputProfile::P1080.fps as Ms);
    let (a, b) = (1_000u64.div_ceil(a), 1_000u64.div_ceil(b));
    if a > b {
        a
    } else {
        b
    }
};

/// Owed 94's threshold, derived (the one place it is): the output gap budget less the backstop path's
/// other terms, the forced keyframe's floor (F-036: the picture comes back up to it after the next
/// sink's first bytes), the next sink's whole connect, the base back-off (the swap is failure 1 from
/// Connected), the poll that sees the last growth late, and a tick each for the backstop and the
/// build; then floored to the poll's grid. With today's numbers
/// 17 200 − 2 200 − 5 000 − 500 − 250 − 2 × 34 = 9 182, so 9 000 ms, as before F-036 (the C1 review's
/// I2: the floor does not shorten hung detection). A threshold the budget cannot hold is a compile
/// error (the subtraction underflows).
pub const fn output_egress_flat_ms(backoff_base_ms: Ms, connect_timeout_ms: Ms) -> Ms {
    let room = OUTPUT_GAP_BUDGET_MS
        - FORCED_KEYFRAME_MIN_INTERVAL_MS
        - connect_timeout_ms
        - backoff_base_ms
        - OUTPUT_PROGRESS_POLL_MS
        - 2 * TICK_WIDEST_MS;
    room / OUTPUT_PROGRESS_POLL_MS * OUTPUT_PROGRESS_POLL_MS
}

/// F-013 (C11's confirm pass, probe GR, 2 of 2; the final C11 review's probe FD, kills 20 s apart):
/// with its zygote alive, a killed GPU process is relaunched 5 times, and the 6th death is Chromium's
/// FATAL "GPU process isn't usable. Goodbye." and SIGTRAP of the whole process. A count over the
/// process's life, not a rate (no decay over 100 s). A const, not a `Thresholds` field: it is
/// Chromium's behaviour, not a setting of ours. `cef_gpu_deaths_to_restart` stays two survivable
/// deaths under it.
pub const CHROMIUM_GPU_DEATHS_ABORT: usize = 6;

/// R4 sponsors on stream, the frame grab (spec 2026-10-27 §3.6.3: "1.5 s after a `start` line, grab one
/// programme frame"; task SC2: `FRAME_GRAB_DELAY_MS = 1_500`): from a sponsor start held for the beat to
/// the grab of its frame, so the card is on screen in it. Consts, not `Thresholds` fields: the
/// supervisor decides nothing on them (`crate::frame_grab` does).
pub const FRAME_GRAB_DELAY_MS: Ms = 1_500;

/// Spec §3.6.3, "≤ 1 per 20 s" (task SC2: `FRAME_GRAB_MIN_GAP_MS = 20_000`; the SC2 dispatch: "across ALL
/// airings"): the least time between two grabs, whatever their airings. A start whose grab would come
/// sooner after the last grab gets none.
pub const FRAME_GRAB_MIN_GAP_MS: Ms = 20_000;

/// Spec §3.6.3, "JPEG 640×360 q≈75" (task SC2: "a JPEG at 640×360, q 75"): the grabbed frame's width and
/// height, a 16:9 frame like both programme profiles.
pub const FRAME_GRAB_SIZE: (u32, u32) = (640, 360);

/// Spec §3.6.3 and task SC2, as above: the JPEG's quality (jpegenc's `quality`, 0 to 100).
pub const FRAME_GRAB_JPEG_QUALITY: u32 = 75;

/// Web's frame endpoint (task S9: "body `image/jpeg` ≤ 256 KiB"): a larger frame would be refused
/// (413), so it is never sent.
pub const FRAME_UPLOAD_MAX_BYTES: usize = 256 * 1024;

/// The SC2 dispatch (2026-10-31): "a 5xx or transport error may retry at most once": an upload is tried
/// at most twice. A 4xx is not retried, and a 410 stops every later upload.
pub const FRAME_UPLOAD_ATTEMPTS: u32 = 2;

/// Each upload attempt's bound, connect to last byte (the implementer's choice): the heartbeat's own,
/// `Thresholds::heartbeat_interval_ms` (5 s, spec §5.3), so a frame never holds a connection longer
/// than a beat does.
pub const FRAME_UPLOAD_TIMEOUT_MS: Ms = 5_000;

/// How long a grab waits for the programme's next frame once it has asked for one (the implementer's
/// choice): 30 frame periods at the 30 fps of both profiles. A programme that makes no frame in that
/// time (drained, stalled) gives none.
pub const FRAME_GRAB_WAIT_MS: Ms = 1_000;

/// How long a grab waits for its JPEG once it has the frame (the implementer's choice): one 640×360
/// encode takes milliseconds, so this only bounds a stuck encoder.
pub const FRAME_ENCODE_WAIT_MS: Ms = 2_000;

/// The PDF's timing budget "slate ≤ 3.1 s" against its own 3 s threshold: the 100 ms the tick that
/// sees the threshold may take. The owner's ruling of 2026-11-24 moved the threshold to 4.5 s and
/// kept this slack, so the budget is the threshold and this (`Thresholds::slate_budget_ms`).
pub const SLATE_BUDGET_SLACK_MS: Ms = 3_100 - 3_000;

impl Thresholds {
    /// The slate's budget (spec §6 "slate after camera loss", amended 2026-11-24 with the owner's
    /// ruling): the reconnecting slate is on air at most this long after the last camera frame. The
    /// threshold and the PDF's slack (`SLATE_BUDGET_SLACK_MS`), so a move of the threshold moves it.
    pub fn slate_budget_ms(&self) -> Ms {
        self.slate_after_ms + SLATE_BUDGET_SLACK_MS
    }

    /// C2 (S3), derived: the least time between two heartbeats once a programme phase change asks
    /// for one before the cadence's (the "edge beat"). It is `resume_after_ms`, the shortest stay the
    /// programme table guarantees the reconnecting slate: a return needs that long of frames, all
    /// after the slate went up (the slate goes up `slate_after_ms` after the last frame, past
    /// `frozen_after_ms`, so the next frame starts a new run). The slate's entry comes
    /// `slate_after_ms − frozen_after_ms` after a freeze, which is longer. So a slate's entry and its
    /// exit each beat on their own tick when the beat before them did, and a phase that flaps faster
    /// costs at most one beat a gap (1 000 ms today: one beat a second at most, against the cadence's
    /// one in five), each carrying the phase of its tick. A change the gap holds beats on the first
    /// tick the gap allows. The pins: `the_phase_beat_gap_lets_a_slates_entry_and_exit_each_beat`, and the
    /// slate's shortest stay as the supervisor makes it, `a_slates_shortest_stay_is_at_least_the_gap`.
    pub fn phase_beat_gap_ms(&self) -> Ms {
        self.resume_after_ms
    }
}

impl Default for Thresholds {
    fn default() -> Self {
        let (output_backoff_base_ms, output_connect_timeout_ms) = (500, 5_000);
        let (input_stall_after_ms, input_srt_latency_ms) = (5_000, 1_000);
        Self {
            frozen_after_ms: 500,
            slate_after_ms: 4_500,
            resume_after_ms: 1_000,
            input_backoff_base_ms: 1_000,
            input_backoff_cap_ms: 4_000,
            input_connect_timeout_ms: 9_000,
            input_stall_after_ms,
            input_decode_wait_ms: input_decode_wait_ms(input_stall_after_ms),
            input_bytes_fresh_ms: input_srt_latency_ms,
            input_srt_latency_ms,
            input_hold_end_after_ms: 20_000,
            kick_alert_count: 3,
            kick_window_ms: 300_000,
            output_backoff_base_ms,
            output_backoff_cap_ms: 2_000,
            output_connect_timeout_ms,
            output_connected_after_ms: 5_000,
            output_egress_flat_ms: output_egress_flat_ms(output_backoff_base_ms, output_connect_timeout_ms),
            cef_fail_count: 3,
            cef_fail_window_ms: 300_000,
            cef_load_timeout_ms: 10_000,
            cef_recycle_every_ms: 7_200_000,
            cef_backoff_base_ms: 1_000,
            cef_backoff_cap_ms: 10_000,
            cef_alive_every_ms: 2_000,
            cef_hang_after_ms: 10_000,
            cef_rss_cap_mb: 600,
            cef_leaks_to_restart: 3,
            cef_gpu_deaths_to_restart: 3,
            ladder_overruns_to_step: 3,
            ladder_window_ms: 1_000,
            drain_timeout_ms: 7_000,
            final_hold_ms: 10_000,
            heartbeat_interval_ms: 5_000,
            bootstrap_backoff_base_ms: 1_000,
            bootstrap_backoff_cap_ms: 10_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutputProfile {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub video_kbps: u32,
}

impl OutputProfile {
    /// ADR 0005 default. 4000 kbps = top of YouTube's 720p30 range; F-005 measured it real time at
    /// 0.83 of 4 cores.
    pub const P720: Self = Self {
        width: 1280,
        height: 720,
        fps: 30,
        video_kbps: 4_000,
    };
    /// ADR 0005 flag. 6000 kbps = the POC default; F-005 measured it real time at 1.11 of 4 cores.
    pub const P1080: Self = Self {
        width: 1920,
        height: 1080,
        fps: 30,
        video_kbps: 6_000,
    };

    pub fn frame_period_ms(&self) -> f64 {
        1_000.0 / f64::from(self.fps)
    }

    /// F-036: `PROGRAMME_GOP_MS` in this profile's frames (x264enc's `key-int-max`).
    pub fn gop_frames(&self) -> u32 {
        frames_in(self.fps, PROGRAMME_GOP_MS)
    }

    /// F-036: `FORCED_KEYFRAME_MIN_INTERVAL_MS` in this profile's frames.
    pub fn forced_keyframe_min_frames(&self) -> u32 {
        frames_in(self.fps, FORCED_KEYFRAME_MIN_INTERVAL_MS)
    }

    /// Owed 86: the real output frame rate below which the output is starved
    /// (`OUTPUT_FPS_FLOOR_SHARE` of this profile's `fps`).
    pub fn fps_floor(&self) -> f64 {
        f64::from(self.fps) * OUTPUT_FPS_FLOOR_SHARE
    }

    /// Owed 86: the real output frame rate at or above which a raised starvation alarm clears
    /// (`OUTPUT_FPS_RECOVERED_SHARE` of this profile's `fps`).
    pub fn fps_recovered(&self) -> f64 {
        f64::from(self.fps) * OUTPUT_FPS_RECOVERED_SHARE
    }
}

/// `ms` at `fps`, in whole frames (rounded down; the config tests hold every interval to a whole
/// number of frames in both profiles).
fn frames_in(fps: u32, ms: Ms) -> u32 {
    u32::try_from(u64::from(fps) * ms / 1_000).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    // These pin the declarations to their SOURCES (PDF / spec / ADR / F-id), not to the code that reads
    // them: every later task takes its expected values from `Thresholds::default()`, so a wrong number
    // here would be frozen in as right everywhere else.

    const BOTH_MODES: [OutputProfile; 2] = [OutputProfile::P720, OutputProfile::P1080];

    // The PDF state machine's frozen and resume numbers; the slate is the owner's ruling of
    // 2026-11-24 ("keep the frozen picture, 4.5 s slate"): one 3 s phone GOP (C1, F-036) and a margin.
    #[test]
    fn programme_thresholds_match_the_pdf_state_machine() {
        let t = Thresholds::default();
        assert_eq!(
            (t.frozen_after_ms, t.slate_after_ms, t.resume_after_ms),
            (500, 4_500, 1_000)
        );
        let phone_gop = 3_000;
        assert!(
            t.slate_after_ms > phone_gop,
            "a freeze to the next keyframe must not slate"
        );
    }

    // In both modes (ADR 0005) a tick may land one frame late, and the slate is still on screen inside
    // its budget: the PDF's 3.1 s for a 3 s threshold, so the threshold and 100 ms (4.6 s since the
    // owner's ruling of 2026-11-24).
    #[test]
    fn slate_lands_inside_its_budget_with_one_frame_of_tick_jitter() {
        let t = Thresholds::default();
        assert_eq!(SLATE_BUDGET_SLACK_MS, 100, "the PDF's 3.1 s against its 3 s");
        assert_eq!(
            t.slate_budget_ms(),
            4_500 + 100,
            "the ruling's 4.5 s and the PDF's slack"
        );
        let mut checked = 0;
        for p in BOTH_MODES {
            let worst = t.slate_after_ms as f64 + p.frame_period_ms();
            assert!(worst <= t.slate_budget_ms() as f64, "{p:?}: slate at {worst} ms");
            checked += 1;
        }
        assert_eq!(checked, BOTH_MODES.len());
    }

    // C2 (S3): the edge beat's gap lets the slate's entry beat after a freeze; it is under the cadence
    // (or no edge beat would ever come early) and above 0 (or a flapping phase would beat every
    // tick). That it is no longer than the slate's shortest stay is read off the real supervisor, not
    // here (the gap IS `resume_after_ms`): `a_slates_shortest_stay_is_at_least_the_gap` in
    // `tests/phase_beat.rs` (C2 review, m3).
    #[test]
    fn the_phase_beat_gap_lets_a_slates_entry_and_exit_each_beat() {
        let t = Thresholds::default();
        let gap = t.phase_beat_gap_ms();
        assert!(gap > 0, "a flap would beat every tick");
        assert!(
            gap <= t.slate_after_ms - t.frozen_after_ms,
            "the slate's entry, after the freeze"
        );
        assert!(
            gap < t.heartbeat_interval_ms,
            "an edge beat comes before the cadence's"
        );
    }

    // The stall rule fires after the slate is on air: the slate goes up on the first tick at or past
    // `slate_after_ms`, at most `TICK_WIDEST_MS` late, and the stall is read on a tick too, so the
    // slate is on screen at least one tick before the input is given up. With the 4.5 s slate (the
    // owner's ruling, 2026-11-24) the gap is 500 ms, a tick and more; the stall default stays 5 s.
    #[test]
    fn stall_comes_after_the_slate() {
        let t = Thresholds::default();
        assert!(
            t.input_stall_after_ms > t.slate_after_ms,
            "viewers see the slate before the input is torn down"
        );
        assert!(
            t.input_stall_after_ms >= t.slate_after_ms + TICK_WIDEST_MS,
            "the slate is on air a tick before the stall: {} − {}",
            t.input_stall_after_ms,
            t.slate_after_ms
        );
        assert_eq!(
            t.input_stall_after_ms, 5_000,
            "spec §6's stall, unmoved by the slate ruling"
        );
    }

    #[test]
    fn output_backoff_cap_fits_inside_the_5_s_reconnect_budget() {
        let t = Thresholds::default();
        assert!(t.output_backoff_cap_ms < 5_000);
    }

    // ADR 0005 and the plan: 1280×720 at 30 fps and 4000 kbps by default; 1920×1080 at 30 fps and
    // 6000 kbps behind the flag.
    #[test]
    fn profiles_are_adr_0005() {
        let p = OutputProfile::P720;
        assert_eq!((p.width, p.height, p.fps, p.video_kbps), (1280, 720, 30, 4_000));
        let p = OutputProfile::P1080;
        assert_eq!((p.width, p.height, p.fps, p.video_kbps), (1920, 1080, 30, 6_000));
    }

    // 30 fps in both modes (ADR 0005): 30 frame periods fill exactly one second.
    #[test]
    fn frame_period_is_a_thirtieth_of_a_second_in_both_modes() {
        for p in BOTH_MODES {
            let thirty = p.frame_period_ms() * 30.0;
            assert!(
                (thirty - 1_000.0).abs() < 1e-9,
                "{p:?}: 30 frames span {thirty} ms"
            );
        }
    }

    // Each number against its own source. The plan's Global Constraints: input back-off 1 s, capped
    // at 4 s since F-036 (C2; the plan's 10 s, amended); a kick alert at MORE than 3 kicks in 5 min; a
    // heartbeat every 5000 ms; CEF recycled every 2 h.
    // Spec §4.2: CEF Failed after 3 failures in 5 min. The C02 brief default: output back-off 0.5 s →
    // 2 s, sized to the PDF's 5 s reconnect budget. (The plan's output stall after 5 s is rtmp2sink's
    // own timeout since Owed 87: `the_sink_timeout_is_rtmp2s_own_default`.)
    #[test]
    fn link_and_lifecycle_numbers_match_their_sources() {
        let t = Thresholds::default();
        assert_eq!(
            (t.input_backoff_base_ms, t.input_backoff_cap_ms),
            (1_000, 4_000),
            "input back-off (F-036, C2: the cap is 4 s)"
        );
        assert_eq!((t.kick_alert_count, t.kick_window_ms), (3, 300_000), "kick alert");
        assert_eq!(
            (t.output_backoff_base_ms, t.output_backoff_cap_ms),
            (500, 2_000),
            "output back-off"
        );
        assert_eq!(t.heartbeat_interval_ms, 5_000, "heartbeat");
        assert_eq!(
            (t.cef_fail_count, t.cef_fail_window_ms),
            (3, 300_000),
            "CEF failed"
        );
        assert_eq!(t.cef_recycle_every_ms, 7_200_000, "CEF recycle");
        assert_eq!(t.cef_rss_cap_mb, 600, "CEF memory cap (spec §6)");
    }

    // PDF "loads in under 2 s" with a ×5 margin; F-001 measured cefsrc start → first buffer at 2.804 s
    // at worst, which must not count as a failure.
    #[test]
    fn cef_load_timeout_is_five_pdf_loads_and_clears_the_f001_first_buffer() {
        let t = Thresholds::default();
        assert_eq!(t.cef_load_timeout_ms, 5 * 2_000);
        assert!(t.cef_load_timeout_ms > 2_804);
    }

    // Owed 18 (owner, 2026-10-18, "2 Ok"): the page beats every 2 s (R2005), and five missed beats
    // are a hang, the page's own SCORE_STALE_AFTER_MS (spec §6). Against the sources, not each other
    // alone: 5 × 2 000 = 10 000. A hang deadline at or past the recycle could never fire before it.
    #[test]
    fn the_hang_threshold_is_five_beats() {
        let t = Thresholds::default();
        assert_eq!(t.cef_alive_every_ms, 2_000, "R2005's beat");
        assert_eq!(t.cef_hang_after_ms, 5 * t.cef_alive_every_ms, "five beats");
        assert_eq!(t.cef_hang_after_ms, 10_000, "the page's SCORE_STALE_AFTER_MS");
        assert!(
            t.cef_hang_after_ms < t.cef_recycle_every_ms,
            "a hang is detected before the planned recycle"
        );
    }

    // F-002 and F-004 rerun 1: the slowest healthy connect on CF SRT playback was a first decoded
    // frame at 4.19 s and a first AU at 4.4 s. A timeout at or below that tears down a healthy input.
    #[test]
    fn input_connect_timeout_clears_the_slowest_measured_first_frame() {
        let t = Thresholds::default();
        assert!(
            t.input_connect_timeout_ms > 4_400,
            "{} ms",
            t.input_connect_timeout_ms
        );
    }

    // F-036 (the C1 brief's rule, 2026-11-24): 9 s. Each term from its source, written here: the
    // brief's estimate of the stg session's slow returns, 6 to 8 s after the connect (an estimate, not
    // a measurement); F-032's first frames after the hold came up to 4.2 s after the connect behind a
    // 2 s publisher GOP, and the phone's GOP is now 3 s, so up to a second more (5.2 s, derived). The
    // timeout clears both, and the hold-end gate (F-032) still sits after a stall and a whole connect.
    #[test]
    fn input_connect_timeout_is_f036s_9_s_and_clears_a_3_s_gop_return() {
        let t = Thresholds::default();
        assert_eq!(t.input_connect_timeout_ms, 9_000, "F-036's rule");
        let (brief_estimate, f032_slowest, gop_now, gop_then) = (8_000, 4_200, 3_000, 2_000);
        assert!(t.input_connect_timeout_ms > brief_estimate);
        assert!(t.input_connect_timeout_ms > f032_slowest + (gop_now - gop_then));
        assert!(t.input_hold_end_after_ms > t.input_stall_after_ms + t.input_connect_timeout_ms);
    }

    // C3 (S2 item 2), each term from its source, written here: the stall rule's 5 s (spec §6), the
    // phone's 3 s GOP (the C1 brief), and the worst decoder wait with `output-corrupt=false`, a lost
    // keyframe, so two GOPs from the last good frame. The extension clears that wait, stays short of
    // the hold-end gate with a whole connect after it (F-032: a hold-end close must still read as one),
    // and is the stall's own bound when no byte arrives (the fresh window is under the stall).
    #[test]
    fn the_decode_wait_clears_a_lost_keyframe_and_keeps_the_hold_end_gate() {
        let t = Thresholds::default();
        let (stall, phone_gop) = (5_000, 3_000);
        assert_eq!(CAMERA_GOP_MS, phone_gop, "the C1 brief's 3 s");
        assert_eq!(t.input_decode_wait_ms, stall + phone_gop, "the stall and one GOP");
        assert!(t.input_decode_wait_ms > t.input_stall_after_ms);
        assert!(t.input_decode_wait_ms >= 2 * phone_gop, "a lost keyframe's wait");
        assert!(t.input_hold_end_after_ms > t.input_decode_wait_ms + t.input_connect_timeout_ms);
    }

    // C3 (S5 item 2): the camera's pre-decoder queue holds one phone GOP and one SRT latency, each
    // from its source (the C1 brief's 3 s, F-002's 1 000 ms), and so more than a whole lost
    // keyframe's GOP; it is under the stall, so a queue that fills (a wedged decoder) is rebuilt by
    // the stall rule well after it, and the hung-decoder probe's 3 s (adapters input.rs) fits in it.
    #[test]
    fn the_camera_video_queue_holds_a_phone_gop_and_one_srt_latency() {
        let t = Thresholds::default();
        assert_eq!(CAMERA_VIDEO_QUEUE_MS, 3_000 + 1_000);
        assert_eq!(CAMERA_VIDEO_QUEUE_MS, CAMERA_GOP_MS + t.input_srt_latency_ms);
        assert!(t.input_srt_latency_ms > 0, "so the queue holds more than a GOP");
        assert!(CAMERA_VIDEO_QUEUE_MS < t.input_stall_after_ms);
        // The byte backstop holds the whole bound at 20 Mbit/s (a phone's 1080p ceiling), so the
        // time bound is the one that acts.
        assert_eq!(CAMERA_VIDEO_QUEUE_BYTES, 10_485_760, "GStreamer queue's default");
        assert!(u64::from(CAMERA_VIDEO_QUEUE_BYTES) * 8 * 1_000 / CAMERA_VIDEO_QUEUE_MS >= 20_000_000);
    }

    // C3 (S2 item 2): bytes count as still arriving for one SRT latency (F-002's 1 000 ms). The window
    // is read once per tick, so it holds several ticks, and it is under the stall, so a sender gone
    // quiet before the stall bound is stalled at the plain bound.
    #[test]
    fn the_bytes_window_is_one_srt_latency_and_under_the_stall() {
        let t = Thresholds::default();
        assert_eq!(t.input_bytes_fresh_ms, 1_000, "F-002's SRT latency");
        assert_eq!(t.input_bytes_fresh_ms, t.input_srt_latency_ms);
        assert!(t.input_bytes_fresh_ms > 2 * TICK_WIDEST_MS);
        assert!(t.input_bytes_fresh_ms < t.input_stall_after_ms);
    }

    // C10: the SRT latency is the one every F-002 reader used (1 000 ms), so the measured first-frame
    // times and the in-chain overlay delay (ADR 0006's default) hold for it. A different value owes a
    // re-measure and moves the overlay delay with it.
    #[test]
    fn input_srt_latency_is_the_one_f002_measured() {
        assert_eq!(Thresholds::default().input_srt_latency_ms, 1_000);
    }

    // F-032 (Owed 108 fix (a)), each number from the finding, written here: the gate is 20 s; the
    // errors at a new publisher's arrival came about 4 s after the last frame and no other error fell
    // between the drop + 10 s and the close; the close came 27.9 to 29.2 s after the last frame. So
    // the gate sits above every early error and below the earliest close, with the finding's 8 s of
    // margin to it, and above the stall rule and the connect timeout, which come first in an outage.
    #[test]
    fn the_hold_end_gate_sits_between_f032s_early_errors_and_its_closes() {
        let t = Thresholds::default();
        assert_eq!(t.input_hold_end_after_ms, 20_000, "F-032's bound");
        let (latest_early, earliest_close) = (10_000, 27_900);
        assert!(t.input_hold_end_after_ms > latest_early);
        assert!(
            t.input_hold_end_after_ms + 7_900 <= earliest_close,
            "F-032's 8 s of margin"
        );
        assert!(t.input_hold_end_after_ms > t.input_stall_after_ms + t.input_connect_timeout_ms);
    }

    // F-004 addendum: YouTube split the organiser's broadcast after a 30 s output gap and kept it
    // through 15 s, so every recovery path must finish its gap under 15 s, and under F-036's floor
    // more for the keyframe the new sink starts at. The output's own worst single-attempt recovery
    // from a destination stall: the sink's own timeout reports it (Owed 87), one capped back-off, a
    // connect that uses its whole timeout, then the keyframe held to the floor.
    #[test]
    fn a_destination_stall_and_one_reconnect_fit_inside_youtubes_15_s_split() {
        let t = Thresholds::default();
        let connect_path = OUTPUT_SINK_TIMEOUT_MS + t.output_backoff_cap_ms + t.output_connect_timeout_ms;
        assert!(
            connect_path < YOUTUBE_KEPT_GAP_MS,
            "output recovery to the connect {connect_path} ms"
        );
        let gap = connect_path + FORCED_KEYFRAME_MIN_INTERVAL_MS;
        assert!(gap < OUTPUT_GAP_BUDGET_MS, "output recovery gap {gap} ms");
    }

    // Owed 87 (the owner, 2026-10-24, "A"): the sink's timeout is pinned to the value every CI
    // destination stall was swapped by, GStreamer 1.24.2's own default: `DEFAULT_TIMEOUT 5` (seconds),
    // `gst/rtmp2/gstrtmp2locationhandler.c:35`, read at the tag. A literal on purpose: a test that read
    // the constant it checks could not catch a change to it.
    #[test]
    fn the_sink_timeout_is_rtmp2s_own_default() {
        assert_eq!(OUTPUT_SINK_TIMEOUT_MS, 5 * 1_000, "rtmp2 DEFAULT_TIMEOUT 5 s");
    }

    // Owed 94 (the owner, 2026-10-24, "B"): the egress backstop's worst path, each term from its own
    // source and written here, not read from the derivation: the last growth is seen up to one poll late
    // (C09's poll), the backstop fires on the first tick at or after its threshold, the swap waits the
    // base back-off (failure 1 from Connected) and then the first tick after it, the next sink's
    // first bytes come 1 ms before its connect timeout, and its keyframe up to F-036's floor after
    // them. All of it stays under the output gap budget, and a threshold one poll longer would not:
    // the threshold is as long as the budget allows, on the poll's grid (nothing finer can see a flat
    // total). F-036 did not move it (the C1 review's I2(b)): 9 000 ms, as Owed 94 set it.
    #[test]
    fn the_egress_backstop_is_the_longest_threshold_that_keeps_the_15_s_budget() {
        let t = Thresholds::default();
        let tick = BOTH_MODES
            .iter()
            .map(|p| p.frame_period_ms().ceil() as Ms)
            .max()
            .expect("two modes");
        assert_eq!(tick, 34, "30 fps ticks land on whole ms 33 to 34 apart");
        let worst = |flat: Ms| {
            OUTPUT_PROGRESS_POLL_MS
                + flat
                + tick
                + t.output_backoff_base_ms
                + tick
                + (t.output_connect_timeout_ms - 1)
                + FORCED_KEYFRAME_MIN_INTERVAL_MS
        };
        let flat = t.output_egress_flat_ms;
        assert_eq!(flat, 9_000, "Owed 94's threshold, unmoved by F-036's floor");
        assert!(
            worst(flat) < OUTPUT_GAP_BUDGET_MS,
            "backstop path {} ms",
            worst(flat)
        );
        assert!(
            worst(flat + OUTPUT_PROGRESS_POLL_MS) >= OUTPUT_GAP_BUDGET_MS,
            "{flat} ms is a poll short of the longest that fits"
        );
        assert_eq!(
            flat % OUTPUT_PROGRESS_POLL_MS,
            0,
            "{flat} ms is off the poll's grid"
        );
        assert_eq!(TICK_WIDEST_MS, tick, "the tick the derivation reads");
    }

    // Owed 94: a socket wait that outlasts the sink's timeout reaches the sink's own error first (ruling
    // A's detector), so the backstop never takes a stall rtmp2sink reports: from the last counted
    // growth the error lands one socket wait later, seen within a poll, acted on at the next tick. The
    // coordinator's question bounded it more widely (the timeout, a back-off, a poll); it holds with the
    // capped back-off too. Not every destination stall: a multi-wake trickle, whose every wait is under
    // the timeout, never errors, and only the backstop swaps it (the PR #73 review's P-3; links.rs).
    #[test]
    fn a_socket_wait_past_the_timeout_errors_before_the_backstop() {
        let t = Thresholds::default();
        let flat = t.output_egress_flat_ms;
        assert!(
            flat > OUTPUT_SINK_TIMEOUT_MS + OUTPUT_PROGRESS_POLL_MS + TICK_WIDEST_MS,
            "backstop {flat} ms"
        );
        assert!(
            flat > OUTPUT_SINK_TIMEOUT_MS + t.output_backoff_cap_ms + OUTPUT_PROGRESS_POLL_MS,
            "backstop {flat} ms"
        );
    }

    // F-036, each number against its source, written here: Cloudflare's 2 to 10 s (its live input
    // requirements), the 3 s GOP (the brief; the capture app's fix), and the forced keyframe's floor at
    // Cloudflare's plus 200 ms (the controller's ruling on the C1 review's I1). In both modes each is
    // a whole number of frames, so x264's frame counts land on the millisecond, and the GOP and the
    // floor sit inside Cloudflare's range, the floor strictly above its edge.
    //
    // The bound: Owed 84's programme tests (`adapters/tests/programme_output.rs`) place a swap a
    // floor after the GOP's keyframe and need the encoder's own next keyframe at least
    // `OWED84_MARGIN_TICKS` (10) + `OWED84_KEY_WAIT_TICKS` (6) ticks after it, so the GOP less the
    // floor is at least 16 frames (24 today; a 2 500 ms floor would leave 15).
    #[test]
    fn the_gop_and_the_forced_keyframe_floor_sit_inside_cloudflares_range() {
        assert_eq!(
            (CF_KEYFRAME_INTERVAL_MIN_MS, CF_KEYFRAME_INTERVAL_MAX_MS),
            (2_000, 10_000),
            "Cloudflare's range"
        );
        assert_eq!(PROGRAMME_GOP_MS, 3_000, "the brief's 3 s");
        assert_eq!(FORCED_KEYFRAME_MARGIN_MS, 200, "the ruling's margin");
        assert_eq!(
            FORCED_KEYFRAME_MIN_INTERVAL_MS,
            2_000 + 200,
            "Cloudflare's floor and the margin"
        );
        // Constant against constant: the compiler checks it (clippy refuses a runtime assert).
        const _: () = assert!(FORCED_KEYFRAME_MIN_INTERVAL_MS > CF_KEYFRAME_INTERVAL_MIN_MS);
        let (owed84_margin_ticks, owed84_key_wait_ticks) = (10, 6);
        let mut checked = 0;
        for p in BOTH_MODES {
            assert!(
                p.gop_frames() - p.forced_keyframe_min_frames()
                    >= owed84_margin_ticks + owed84_key_wait_ticks,
                "{p:?}: GOP {} less floor {} frames",
                p.gop_frames(),
                p.forced_keyframe_min_frames()
            );
            for ms in [PROGRAMME_GOP_MS, FORCED_KEYFRAME_MIN_INTERVAL_MS] {
                let fps = Ms::from(p.fps);
                assert_eq!(fps * ms % 1_000, 0, "{p:?}: {ms} ms is not whole frames");
                assert!(
                    (CF_KEYFRAME_INTERVAL_MIN_MS..=CF_KEYFRAME_INTERVAL_MAX_MS).contains(&ms),
                    "{ms} ms"
                );
                checked += 1;
            }
            // 30 fps (ADR 0005): 90 frames a GOP, the floor 66.
            assert_eq!(
                (p.gop_frames(), p.forced_keyframe_min_frames()),
                (90, 66),
                "{p:?}"
            );
            assert!(p.forced_keyframe_min_frames() < p.gop_frames(), "{p:?}");
        }
        // The margin holds whole frames at the rates the ruling names, not only ours.
        for fps in [25, 30, 60] {
            assert_eq!(fps * FORCED_KEYFRAME_MIN_INTERVAL_MS % 1_000, 0, "{fps} fps");
            checked += 1;
        }
        assert_eq!(checked, 7, "intervals checked: {checked}");
    }

    // Owed 15 (the choice recorded in `_FINDINGS.md` F-011): C09 reads the sink's total every 250 ms.
    #[test]
    fn the_progress_poll_is_owed_15s_250_ms() {
        assert_eq!(OUTPUT_PROGRESS_POLL_MS, 250);
    }

    // Owed 112 (C3b): half F-036's floor, whole frames in both modes, inside the first GOP.
    #[test]
    fn the_first_gop_catch_up_is_half_the_forced_keyframes_floor() {
        assert_eq!(FIRST_GOP_CATCH_UP_MAX_MS, 2_200 / 2, "half the floor");
        // Constant against constant: the compiler checks it (clippy refuses a runtime assert).
        const _: () = assert!(FIRST_GOP_CATCH_UP_MAX_MS < PROGRAMME_GOP_MS);
        let mut checked = 0;
        for p in BOTH_MODES {
            let fps = Ms::from(p.fps);
            assert_eq!(
                fps * FIRST_GOP_CATCH_UP_MAX_MS % 1_000,
                0,
                "{p:?}: not whole frames"
            );
            // 30 fps (ADR 0005): 33 frames.
            assert_eq!(fps * FIRST_GOP_CATCH_UP_MAX_MS / 1_000, 33, "{p:?}");
            checked += 1;
        }
        assert_eq!(checked, 2, "modes checked: {checked}");
    }

    // F-011 (Owed 38): a refused publish counted bytes until its refusal arrived, the last total at
    // 538 ms on CF (a made-up key) and the error at 2 033 ms on mediamtx (a denied path). Connected
    // must wait past both with at least twice their length to spare; the ruling set 5 s.
    #[test]
    fn output_connected_bound_clears_every_measured_refusal_twice_over() {
        let t = Thresholds::default();
        assert_eq!(
            t.output_connected_after_ms, 5_000,
            "the coordinator's ruling, from F-011"
        );
        for (leg, refused_by) in [("CF, a made-up key", 538), ("mediamtx, a denied path", 2_033)] {
            assert!(
                t.output_connected_after_ms >= 2 * refused_by,
                "{leg}: refused by {refused_by} ms, bound {} ms",
                t.output_connected_after_ms
            );
        }
    }

    // F-004: the same broadcast survived a 15 s gap, and a 30 s gap split it. F-036 (the C1 review's
    // I2, 2026-11-24): the output's budget is that and the forced keyframe's 2.2 s floor, still short
    // of the gap that split it.
    #[test]
    fn output_gap_budget_is_the_15_s_youtube_kept_in_f004() {
        assert_eq!(YOUTUBE_KEPT_GAP_MS, 15_000);
        assert_eq!(OUTPUT_GAP_BUDGET_MS, 15_000 + 2_200);
        // Short of F-004's split; constant against constant, so the compiler checks it.
        const _: () = assert!(OUTPUT_GAP_BUDGET_MS < 30_000);
    }

    // C11b, each number against its own source. Owner ruling 2026-10-19 "B": an early in-place restart
    // once 3 teardowns are leaked (a live count). F-013 / Owed 61: Chromium aborts the process AT the
    // 6th GPU-process death of its life (probe GR 2 of 2; the final C11 review's FD, kills 20 s apart).
    // The restart comes at 3 (the coordinator's decision on the PR #34 review's I1): two deaths after
    // it stay survivable (the 4th and the 5th), one for a death the 1 s child poll has not read yet
    // and one for a death during the exit.
    #[test]
    fn cef_restart_thresholds_match_their_sources() {
        let t = Thresholds::default();
        assert_eq!(t.cef_leaks_to_restart, 3, "owner ruling B");
        assert_eq!(CHROMIUM_GPU_DEATHS_ABORT, 6, "F-013: the 6th death aborts");
        assert_eq!(
            t.cef_gpu_deaths_to_restart, 3,
            "the coordinator's decision (PR #34 I1)"
        );
        assert!(
            t.cef_gpu_deaths_to_restart >= 1,
            "0 would restart a healthy process"
        );
        assert!(
            t.cef_gpu_deaths_to_restart + 2 < CHROMIUM_GPU_DEATHS_ABORT,
            "two survivable deaths after the threshold, before Chromium's abort"
        );
        assert!(t.cef_leaks_to_restart >= 1, "0 would restart a healthy process");
    }

    // The PR #34 review's I1, as a walk rather than arithmetic: the poll reads the threshold while one
    // more death has already happened unread, and one more lands during the exit (the final beat's
    // flush and a tick). Every one of those deaths must be one Chromium survives: it relaunches a dead
    // GPU process up to the 5th death and aborts the process AT the 6th (F-013). The old `+ 2 <=`
    // guard let 4 through, whose worst case is the 6th death: the abort itself.
    #[test]
    fn the_gpu_restart_survives_an_unread_death_and_one_during_the_exit() {
        let t = Thresholds::default();
        let read = t.cef_gpu_deaths_to_restart;
        let worst = [read, read + 1, read + 2]; // read; plus one unread; plus one during the exit
        let survivable = |deaths: usize| deaths < CHROMIUM_GPU_DEATHS_ABORT;
        let mut checked = 0;
        for deaths in worst {
            assert!(
                survivable(deaths),
                "death {deaths} aborts the process before the restart (threshold {read})"
            );
            checked += 1;
        }
        assert_eq!(checked, 3);
        // And the walk is not vacuous: the abort itself is not survivable.
        assert!(!survivable(CHROMIUM_GPU_DEATHS_ABORT));
    }

    // The C00 cost gate is "encoder speed ≥ 0.95"; F-005's worst 10 s window (0.9963) and a healthy
    // 29-frame second at 30 fps must both read as real time.
    #[test]
    fn encoder_realtime_floor_is_the_c00_gate_and_passes_a_29_frame_second() {
        let floor = ENCODER_SPEED_REALTIME_MIN;
        assert!((floor - 0.95).abs() < 1e-12, "C00 gate, got {floor}");
        assert!(29.0 / 30.0 >= floor, "a 29-frame second fails a {floor} floor");
        assert!(0.9963 >= floor, "F-005's worst window fails a {floor} floor");
    }

    // Owed 86, each share against its source: the floor is the C00 cost gate's 0.95 (the encoder's
    // real-time floor, read in frames), and the recovery level is C00 Step 4's `fps ≥ 29.5` for its
    // 30 fps page, which C14 cases 6 and 10 also hold (`fps_mean ≥ 29.5`). At ADR 0005's 30 fps that
    // is 28.5 and 29.5 in both modes.
    #[test]
    fn the_output_fps_floor_and_recovery_match_their_sources() {
        assert!((OUTPUT_FPS_FLOOR_SHARE - 0.95).abs() < 1e-12, "C00 cost gate");
        assert!(
            (OUTPUT_FPS_RECOVERED_SHARE - 29.5 / 30.0).abs() < 1e-12,
            "C00 Step 4"
        );
        for p in BOTH_MODES {
            assert!((p.fps_floor() - 28.5).abs() < 1e-9, "{p:?}: {}", p.fps_floor());
            assert!(
                (p.fps_recovered() - 29.5).abs() < 1e-9,
                "{p:?}: {}",
                p.fps_recovered()
            );
        }
    }

    // Owed 86: a band, not a line. The floor is under the recovery level, which is under the profile's
    // own rate, so a healthy clock clears the alarm and a rate between the two keeps its state.
    #[test]
    fn the_starvation_band_lies_below_the_profiles_rate() {
        for p in BOTH_MODES {
            let fps = f64::from(p.fps);
            assert!(
                p.fps_floor() < p.fps_recovered(),
                "{p:?}: no band between floor and recovery"
            );
            assert!(p.fps_recovered() < fps, "{p:?}: a healthy clock never clears");
        }
    }

    // Owed 86 (the PR #69 review's m4): the minimum window is half the spec's beat (§5.3). An off-grid
    // beat (the hold's first, the drain's) can close a window just after the driver's catch-up burst,
    // the review's 2 to 3 frames short. Over the minimum, a healthy clock 3 frames short still passes
    // the floor; over the first build's 1 s window it fails it, so the minimum is what keeps a healthy
    // clock unalarmed. And over the 5 s beat, one frame short, a healthy clock reaches the recovery
    // level, so the alarm clears on a healthy beat.
    #[test]
    fn the_minimum_window_keeps_a_healthy_clock_above_the_floor() {
        // The PR #69 review's m4: a catch-up burst leaves an off-grid window 2 to 3 frames short.
        const CATCH_UP_SHORT: f64 = 3.0;
        const FIRST_BUILD_WINDOW_MS: f64 = 1_000.0;
        let t = Thresholds::default();
        let beat = t.heartbeat_interval_ms as f64;
        assert_eq!(beat, 5_000.0, "spec §5.3");
        assert_eq!(
            OUTPUT_FPS_MIN_WINDOW_MS * 2,
            t.heartbeat_interval_ms,
            "half the spec's beat"
        );
        let mut checked = 0;
        for p in BOTH_MODES {
            let fps = f64::from(p.fps);
            let short_by =
                |frames: f64, window_ms: f64| (fps * window_ms / 1_000.0 - frames) * 1_000.0 / window_ms;
            let min = OUTPUT_FPS_MIN_WINDOW_MS as f64;
            assert!(
                short_by(CATCH_UP_SHORT, min) >= p.fps_floor(),
                "{p:?}: {} over the minimum",
                short_by(CATCH_UP_SHORT, min)
            );
            assert!(
                short_by(CATCH_UP_SHORT, FIRST_BUILD_WINDOW_MS) < p.fps_floor(),
                "{p:?}: 1 s passes too, so the minimum is not the bound"
            );
            assert!(
                short_by(1.0, beat) >= p.fps_recovered(),
                "{p:?}: a healthy beat {} never clears",
                short_by(1.0, beat)
            );
            checked += 1;
        }
        assert_eq!(checked, BOTH_MODES.len());
    }

    // R4 task SC2, each frame-grab number against its source: the spec's §3.6.3 line ("1.5 s after a
    // `start` line … JPEG 640×360 q≈75 … ≤ 1 per 20 s"), the SC2 brief's constants, web's S9 bound
    // ("≤ 256 KiB") and the dispatch's one retry.
    #[test]
    fn the_frame_grab_numbers_match_the_spec_and_s9() {
        assert_eq!(
            FRAME_GRAB_DELAY_MS, 1_500,
            "spec §3.6.3: 1.5 s after a start line"
        );
        assert_eq!(FRAME_GRAB_MIN_GAP_MS, 20_000, "spec §3.6.3: at most 1 per 20 s");
        assert_eq!(FRAME_GRAB_SIZE, (640, 360), "spec §3.6.3: 640×360");
        assert_eq!(FRAME_GRAB_JPEG_QUALITY, 75, "spec §3.6.3: q≈75; SC2: q 75");
        assert_eq!(FRAME_UPLOAD_MAX_BYTES, 262_144, "S9: at most 256 KiB");
        assert_eq!(FRAME_UPLOAD_ATTEMPTS, 2, "the dispatch: one retry at most");
        let t = Thresholds::default();
        assert_eq!(
            FRAME_UPLOAD_TIMEOUT_MS, t.heartbeat_interval_ms,
            "a beat's own bound"
        );
        // Both profiles are 16:9, so the grab scales without borders.
        let mut checked = 0;
        for p in BOTH_MODES {
            assert_eq!(p.width * FRAME_GRAB_SIZE.1, p.height * FRAME_GRAB_SIZE.0, "{p:?}");
            checked += 1;
        }
        assert_eq!(checked, BOTH_MODES.len());
    }

    // A grab's whole run (the frame, the encode, every upload attempt at its bound) ends before the next
    // grab can be due, `FRAME_GRAB_MIN_GAP_MS` after it: so no grab waits behind another's upload and
    // lands late, showing a later picture than its card's (ureq 2's DNS is unbounded, Owed 28: the one
    // part no bound here covers).
    #[test]
    fn a_grabs_whole_run_ends_before_the_next_can_be_due() {
        let run = FRAME_GRAB_WAIT_MS
            + FRAME_ENCODE_WAIT_MS
            + u64::from(FRAME_UPLOAD_ATTEMPTS) * FRAME_UPLOAD_TIMEOUT_MS;
        assert_eq!(run, 13_000);
        assert!(run < FRAME_GRAB_MIN_GAP_MS, "{run} ms");
    }
}
