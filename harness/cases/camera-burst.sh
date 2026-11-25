# shellcheck shell=bash disable=SC2034 # sourced by run-case.sh, which reads CASE_CHECKS
# C4 (S1, Owed 89): the camera's playout under a bursty camera. Smoke's run (60 s live, pass-through
# netem on both legs, compose's `probe` profile) with the fake camera, a 30 fps source
# (`camera::FPS`), holding its frames and their audio for 300 ms every 10 s and then sending the
# backlog at once with its own PTS (`POST /burst?ms=300`): Cloudflare's late-then-burst delivery
# that stg measured (`_FINDINGS.md` F-039), five times. Sourced by run-case.sh.
#
# The playout plays each frame on a clock `PLAYOUT_TARGET_MS` (150 ms) behind the camera's, so a
# 300 ms hold repeats only the ticks its first frames were late for (300 − 150 ms, less the clock's
# phase against the tick grid; `playout.rs`'s hold test reads 3 a hold at its phase) and drops as
# many at the release. The audio is paced on the same clock, and its ring holds the hold (300 ms
# under the 500 ms cap), so none of it overflows. The newest-frame slot it replaced repeated ~9 ticks
# a hold, and the harness's own mux judder (F-029, ~25 % of smoke's pairs) on top.
#
# Read on CI (PR #113's first run, 45575918830, before fix round 1): 34 repeats in 1 953 pairs,
# 1.74 %, about 7 a hold rather than the 3 the pure test reads (the review's I1: the clock anchored
# on the earlier stream, eroding the later one's target; fix round 1 anchors on the later). The A/V
# offset read p50 25 ms, max 27 ms over 66 pairs, inside the 40 ms budget.
#
# Expected (CI-owed, never run locally): repeat ticks under 2 % of the camera's cadence pairs, 0
# audio overflow bytes over every beat's camera line, the audio in step with the picture within
# `budget_av_offset_ms` at every tone-to-flash pair, no slate inside the run, a clean exit. The
# compositor's own repeat count is measured beside the checker's, not judged.

CASE_LIVE_S=60
CASE_KEY=repeat_ticks_under_2pct
# Owed 81's input-tap (compose's `probe` profile): tap.jsonl, read by scripts/cadence_stages.py's
# input stage, so the run's arrival cadence at the compositor is on record beside its output's.
export COMPOSE_PROFILES=probe

case_drive() {
  "$HERE/netem.sh" in limit 10000
  "$HERE/netem.sh" out limit 10000
  local s
  for s in 10 20 30 40 50; do
    at_s "$s"
    mark burst '{"ms":300}'
    camera POST "/burst?ms=300" >/dev/null
  done
  at_s "$CASE_LIVE_S"
  "$HERE/netem.sh" check in
  "$HERE/netem.sh" check out
}

CASE_CHECKS=11
case_verdict() {
  # The fault: five holds, as asked.
  require bursts_applied '(mark("burst") | length) == 5 and all(mark("burst")[]; .value.ms == 300)' \
    '[mark("burst")[] | .value.ms]'
  # Anti-vacuity: the run recorded and read the camera.
  require frames '.frames >= 1700'
  require camera_strips '.camera.strip_frames >= 1700'
  require cadence_pairs '.camera.cadence.pairs >= 1700'
  # The brief's verdict: repeat ticks under 2 %, judged on the programme's own frames (the checker's
  # cadence), never on the compositor's count of itself.
  require repeat_ticks_under_2pct '.camera.cadence.repeated_frames * 50 < .camera.cadence.pairs' \
    '{repeated: .camera.cadence.repeated_frames, pairs: .camera.cadence.pairs, skipped: .camera.cadence.skipped_counters}'
  # And 0 audio overflow, over the camera lines of at least every beat but the first (a beat every
  # heartbeat_interval_ms, the line once the camera is on air).
  require playout_lines '(.log.camera_playout | length) >= ($t.heartbeat_interval_ms as $b | 60 * 1000 / $b - 1)' \
    '.log.camera_playout | length'
  require no_audio_overflow '[.log.camera_playout[].audio_overflow_bytes] | add == 0' \
    '{overflow: ([.log.camera_playout[].audio_overflow_bytes] | add), pad: ([.log.camera_playout[].audio_pad_bytes] | add),
      late: ([.log.camera_playout[].audio_late_bytes] | add), reanchors: ([.log.camera_playout[].reanchors] | add)}'
  # The review's I2: the camera's audio in step with its picture through the real input, judged on
  # the checker's tone-to-flash pairs (a tone burst a second, `audio::BURST_EVERY_FRAMES` at 30 fps)
  # against the budget: the worst pair, so the median too. Audio that loses its stamp at the input
  # plays first in, first out, ahead of the picture by the playout's target, and reds here.
  # Anti-vacuity: at least half the live window's bursts paired (CI read 66 over the 60 s).
  require av_in_step '.audio.av_offset_ms.pairs >= 30 and .audio.av_offset_ms.max_abs <= $t.budget_av_offset_ms' \
    '.audio.av_offset_ms | {p50, max_abs, pairs}'
  measure compositor_repeat_ticks '{repeat_ticks: ([.log.camera_repeats[].repeat_ticks] | add),
    replaced: ([.log.camera_repeats[].replaced] | add)}'
  require no_interior_slates '.camera.slate_frames == 0'
  require_run compositor_exit '.compositor.status == "exited" and .compositor.exit_code == 0'
}
