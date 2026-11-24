# shellcheck shell=bash disable=SC2034,SC2016 # sourced by run-case.sh; jq filters keep their $ (SC2016)
# Case 20, camera-gop-loss (C3, S2 and S5; 2026-11-24): the capture app's camera, a keyframe every
# 3 s (the C1 brief), on a lossy uplink, then a pause past the stall rule; 100 s live.
#
# - 20 s to 60 s: `netem in loss 1.5%` on MediaMTX's SRT media to the compositor (the brief's 1-2 %).
#   The queue before the camera's decoder no longer drops (C3, S5 item 2), so a loss reaches the
#   decoder only as libsrt's too-late drop; the programme clock must hold through it.
# - 70 s: the camera pauses for `input_stall_after_ms` + `input_backoff_base_ms` / 2 (5 500 ms from
#   thresholds.json; the brief's 5.2 s left ~200 ms of margin on the early side, the C3 review):
#   past the stall, so the stall rule gives the live generation up, and inside the first back-off
#   after it, so its first frame back reaches the shut gate before the rebuild is due and the domain
#   takes the generation back (C3, S2; Owed 49): `input take back gen=<g>`, no rebuild. The pause is
#   past the slate (4 500 ms, the owner's ruling of 2026-11-24), so the slate goes up inside its
#   budget, and the camera is live again within the return budget.
# - The generation is whichever is live at the pause, never a literal: the compositor's first pull
#   usually starts before MediaMTX has the camera, so generation 1 is given up `error` at boot and 2
#   is live (CI run 45480676592: `input give up gen=1 reason=error` 7 s before the drive).
# - The give-up's window comes from the rules it is judged by: the camera's last frame leaves at the
#   pause and arrives a glass-to-glass latency later, so the stall rule fires no sooner than
#   `input_stall_after_ms` after the pause mark; the bytes stop with the frames, so the plain bound
#   holds, and a give-up at or past `input_decode_wait_ms` after the pause would be the bytes
#   extension wrongly in force.
#
# Expected (CI-owed, never run locally): every check below passes. `pause_in_window` is the
# anti-vacuity guard, judged on the marks (the pause as driven, sleep and curl included): a pause the
# take-back cannot answer (at or under the stall, or past the stall and the first back-off) reds it, as a run whose netem dropped nothing reds `fault_applied`. The
# content gaps inside the loss window are measured, not judged (`loss_window_gaps`): how often a lost
# reference still froze the picture on a 3 s GOP is the number Owed 111 asks for.

CASE_LIVE_S=100
CASE_KEY=taken_back
export CAMERA_GOP_MS=3000

case_drive() {
  local pause_ms
  pause_ms=$(($(threshold input_stall_after_ms) + $(threshold input_backoff_base_ms) / 2))
  at_s 20
  mark impair
  "$HERE/netem.sh" in loss 1.5%
  at_s 60
  mark netem_in "$("$HERE/netem.sh" stats in)"
  "$HERE/netem.sh" clear in
  mark clear
  at_s 70
  mark pause "{\"pause_ms\":$pause_ms}"
  camera POST /pause >/dev/null
  sleep "$(awk -v d="$pause_ms" 'BEGIN { printf "%.3f", d / 1000 }')"
  mark resume
  camera POST /resume >/dev/null
  at_s "$CASE_LIVE_S"
}

CASE_CHECKS=16
case_verdict() {
  require fault_applied 'mark_value("netem_in") | .present and .sent_pkts > 0 and .dropped > 0' 'mark_value("netem_in")'
  require pause_in_window '(mark_at("resume") - mark_at("pause")) as $p
    | $p > $t.input_stall_after_ms and $p < $t.input_stall_after_ms + $t.input_backoff_base_ms' \
    '{asked_ms: mark_value("pause").pause_ms, paused_ms: (mark_at("resume") - mark_at("pause")),
      stall: $t.input_stall_after_ms, backoff: $t.input_backoff_base_ms}'
  require gave_up_at_the_pause 'mark_at("pause") as $p
    | [.log.input_give_ups[] | select(.reason == "stalled"
        and .at_ms >= $p + $t.input_stall_after_ms and .at_ms < $p + $t.input_decode_wait_ms)] | length == 1' \
    'mark_at("pause") as $p | {window_from: $t.input_stall_after_ms, window_to: $t.input_decode_wait_ms,
      give_ups: [.log.input_give_ups[] | {after_pause_ms: (.at_ms - $p), gen, reason}]}'
  require taken_back 'mark_at("pause") as $p
    | ([.log.input_give_ups[] | select(.at_ms > $p)] | first) as $g
    | $g != null and ([.log.input_take_backs[] | select(.at_ms >= $g.at_ms and .gen == $g.gen)] | length) == 1' \
    '{give_ups: [.log.input_give_ups[] | {at_ms, gen}], take_backs: [.log.input_take_backs[] | {at_ms, gen}]}'
  require slate_after_loss '([.camera.resumes[] | select(.slate_from_ms != null)] | last) as $r
    | $r != null and $r.last_shown_wall_ms != null
    and ($r.slate_from_ms - $r.last_shown_wall_ms) <= $t.budget_slate_after_loss_ms' \
    '([.camera.resumes[] | select(.slate_from_ms != null)] | last) as $r
      | {slate_after_ms: (if $r == null then null else $r.slate_from_ms - $r.last_shown_wall_ms end),
        budget: $t.budget_slate_after_loss_ms}'
  require returned '(.camera.resumes | last) as $r
    | $r != null and $r.suppressed_ms != null and $r.suppressed_ms <= $t.budget_return_to_live_ms' \
    '{suppressed_ms: (.camera.resumes | last | .suppressed_ms), budget: $t.budget_return_to_live_ms}'
  require pts_gaps '.pts_gaps == []' '.pts_gaps | length'
  require no_output_outages '.output_outages == []' '.output_outages | length'
  require counter_regressions '.camera.counter_regressions == 0'
  measure loss_window_gaps '[.camera.content_gaps[] | select(.to_emit_ms > mark_at("impair") and .to_emit_ms <= mark_at("clear"))] | length'
  require_no_false_restart
}
