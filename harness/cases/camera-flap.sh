# shellcheck shell=bash disable=SC2034,SC2016 # sourced by run-case.sh; jq filters keep their $ (SC2016)
# Case 2, camera-flap (C14 brief): the camera pauses and resumes 12 times, one flap every 10 s from
# 5 s; 125 s live.
#
# Each pause lasts halfway between slate_after_ms and input_stall_after_ms (both from
# thresholds.json): long enough to slate, short of the input's stall rule, so every flap is the same
# flap (the PR #52 review's I5). The brief's "every 5 s" made each pause 5 s, which IS
# input_stall_after_ms: some flaps rebuilt the input and some did not, and CI read 900 to 3 733 ms
# (a false premise, `docs/_INDEX.md`). Since the owner's 4.5 s slate (2026-11-24, C3) the pause is
# 4 750 ms, 250 ms from each rule (it was 4 000, 1 000 ms each side): the slate's entry is judged on
# the strip and the stall's on the log. The drive's sleep and curl lengthen each pause by ~120 ms
# (CI run 45480676592: a 5 200 ms pause marked 5 319 ms), so the stall side keeps ~130 ms
# (a flap past it would log `input give up … reason=stalled`). fake-camera's resume starts with a key frame
# (`camera::Schedule::take_key_due`), so a decoder's wait for one is not the product's hold-off.
#
# Hysteresis from the strip, never the beats (a 5 s beat cannot see a 1 s rule): every resume holds
# the camera off for resume_after_ms, less what the programme's pipeline holds between the input and
# the frame on air (at most BUDGET_FRAME_TO_ENCODER_MS: the hold-off starts at the first frame's
# arrival, and the frame shown at the switch is that much older than the newest; CI's 900 ms flaps),
# and less C4's playout (the camera's picture airs that long behind its arrival, so the frame shown
# at the switch is older again). That delay's worst is the cap, not the target (C4 fix round 2, the
# re-review's floor finding): I1's rule anchors on the later stream, so the picture waits the target
# plus however far its audio trails, up to `playout_cap_ms`; on the tick grid that is a tick more
# (`tick_widest_ms`), and the switch's phase against the camera's frames a frame more
# (`frame_period_ms`). Returns within BUDGET_RETURN_TO_LIVE_MS. Mutant 1 (resume_after_ms 0 in a
# scratch build) reds `flap_hysteresis`.
#
# Each live window after a return, too (C4 fix round 2: f3eff28's run passed on the switch while 10
# of its 11 windows stormed, 22–23 counted re-anchors, ~62 % repeated ticks, 1.6–3.6 s of late
# audio each). A window runs from the return's first frame shown to the last shown before the next
# pause (the last to the final beat); its compositor counts are the beats logged from its start to
# its end plus the frozen rule and the pipeline's hold (the closing live→frozen beat, ~500 ms after
# the last frame shown, never the frozen→reconnecting one 4 s on). In each:
# - counted re-anchors at most 1 when `playout_early_reanchor_ms` outlasts the hold-off (the early
#   rule's one correction can land after the switch), else 0;
# - the compositor's repeated ticks within the pause's own (the queue drains its target, then each
#   tick repeats until the frozen rule: (frozen_after_ms - playout_target_ms) / frame_period_ms,
#   rounded up, and a tick) plus camera-burst's 2 % of the window's ticks;
# - on the strip, no freeze and no skipped counter inside it (the storm's 5-frame freeze, 6-counter
#   skip, 12 times a window);
# - late audio at most one re-anchor's skip, (playout_cap_ms - playout_target_ms) of audio
#   (`audio_bytes_per_ms`).
# And the run's A/V within budget_av_pair_max_ms on the tone-to-flash pairs from `av_return_cut_ms` after each
# anchor (the playout's return, the run's first pair), the earlier pairs within `budget_av_offset_excluded_ms` (lib.jq's `av_gate`; at least one pair a flap), and the median pair within budget_av_offset_ms (round 6).

CASE_LIVE_S=125
CASE_KEY=flap_hysteresis
FLAPS=12
FLAP_EVERY_S=10

case_drive() {
  local i pause_ms
  pause_ms=$((($(threshold slate_after_ms) + $(threshold input_stall_after_ms)) / 2))
  for ((i = 0; i < FLAPS; i++)); do
    at_s $((5 + i * FLAP_EVERY_S))
    mark pause "{\"pause_ms\":$pause_ms}"
    camera POST /pause >/dev/null
    sleep "$(awk -v d="$pause_ms" 'BEGIN { printf "%.3f", d / 1000 }')"
    mark resume
    camera POST /resume >/dev/null
  done
  at_s "$CASE_LIVE_S"
}

# The live windows (above), as one record each, for the window checks.
FLAP_WINDOWS='def flap_windows:
  . as $doc
  | ([.log.camera_playout[].at_ms] | max) as $last
  | [[.camera.resumes[] | select(.first_shown_wall_ms != null)] as $r | range(0; $r | length) as $i
     | {from: $r[$i].first_shown_wall_ms, to: ($r[$i + 1].last_shown_wall_ms // $last)}
     | . + {until: (.to + $t.frozen_after_ms + $t.budget_frame_to_encoder_ms)}
     | . as $w
     | . + {ticks: ((.to - .from) / $t.frame_period_ms | floor),
            beats: [$doc.log.camera_playout[] | select(.at_ms >= $w.from and .at_ms < $w.until)],
            repeat_ticks: ([$doc.log.camera_repeats[] | select(.at_ms >= $w.from and .at_ms < $w.until)
              | .repeat_ticks] | add // 0),
            freezes: ([$doc.camera.freezes[] | select(.from_ms >= $w.from and .from_ms < $w.to)] | length),
            skips: ([$doc.camera.content_gaps[] | select(.from_shown_ms >= $w.from and .from_shown_ms < $w.to)]
              | length)}
     | . + {reanchors: ([.beats[].reanchors] | add // 0), late: ([.beats[].audio_late_bytes] | add // 0),
            beats: (.beats | length)}];'

# The anchors the A/V gate counts from: the run's first pair, and each return of the PLAYOUT from the
# camera (the wall time of its `reanchor(gap` note; the strip's first frame shown waits out the
# hold-off beside it, `resume_after_ms`, which the cut spans instead).
FLAP_AV_STARTS='def av_starts: [.audio.av_offset_ms.list[0].at_ms // empty] + [.log.playout_returns[].at_ms];'

CASE_CHECKS=16
case_verdict() {
  # Anti-vacuity: 12 flaps leave at least 10 resumes the strip judged (each judged resume followed a
  # slate, so each pause was long enough to slate), and no pause reached the input's stall rule.
  require flaps_seen "(mark(\"pause\") | length) == $FLAPS and (mark(\"resume\") | length) == $FLAPS
    and (mark(\"pause\") | all(.value.pause_ms < \$t.input_stall_after_ms))
    and ([.camera.resumes[] | select(.suppressed_ms != null)] | length) >= 10" \
    '{pauses: (mark("pause") | length), pause_ms: mark_value("pause").pause_ms, resumes: (.camera.resumes | length),
      judged: ([.camera.resumes[] | select(.suppressed_ms != null)] | length)}'
  require flap_hysteresis '(.camera.resumes | length) >= 1 and ([.camera.resumes[] | .suppressed_ms]
    | all(. != null and . >= ($t.resume_after_ms - $t.budget_frame_to_encoder_ms - $t.playout_cap_ms
        - $t.tick_widest_ms - $t.frame_period_ms) and . <= $t.budget_return_to_live_ms))' \
    '[.camera.resumes[] | .suppressed_ms] | {min: min, max: max,
      floor: ($t.resume_after_ms - $t.budget_frame_to_encoder_ms - $t.playout_cap_ms - $t.tick_widest_ms
        - $t.frame_period_ms), budget: $t.budget_return_to_live_ms}'
  # Anti-vacuity for the window checks: at least 10 windows, each with its closing beat.
  require live_windows "$FLAP_WINDOWS (flap_windows | length) >= 10 and (flap_windows | all(.beats >= 1))" \
    "$FLAP_WINDOWS flap_windows | {windows: length, beats: map(.beats), ticks: map(.ticks)}"
  require window_reanchors "$FLAP_WINDOWS (if \$t.playout_early_reanchor_ms >= \$t.resume_after_ms then 1 else 0 end)
    as \$most | flap_windows | all(.reanchors <= \$most)" "$FLAP_WINDOWS flap_windows | map(.reanchors)"
  require window_repeats "$FLAP_WINDOWS ((\$t.frozen_after_ms - \$t.playout_target_ms) / \$t.frame_period_ms
    | ceil + 1) as \$pause | flap_windows | all(.repeat_ticks <= \$pause + (.ticks / 50 | ceil))" \
    "$FLAP_WINDOWS flap_windows | map(.repeat_ticks)"
  require window_strip "$FLAP_WINDOWS flap_windows | all(.freezes == 0 and .skips == 0)" \
    "$FLAP_WINDOWS flap_windows | {freezes: map(.freezes), skips: map(.skips)}"
  require window_late_audio "$FLAP_WINDOWS ((\$t.playout_cap_ms - \$t.playout_target_ms) * \$t.audio_bytes_per_ms)
    as \$most | flap_windows | all(.late <= \$most)" "$FLAP_WINDOWS flap_windows | map(.late)"
  # C4's gate (owner ruling, 2026-11-25, option 1): the budget is judged on the pairs from `av_return_cut_ms`
  # (the return hold-off, the playout's settle skip and its settle) after each anchor, the earlier ones
  # against their own bound (`budget_av_offset_excluded_ms`, lib.jq's `av_gate`), at least a pair a
  # flap, at least one kept pair in every window, and the kept and excluded counts in the measure. Every
  # resume the strip showed has a playout return line (a dropped note would silently merge two windows).
  require av_in_step "$FLAP_AV_STARTS ([.log.playout_returns[]] | length) >= ([.camera.resumes[]
    | select(.first_shown_wall_ms != null)] | length) and av_gate_ok(av_starts; $FLAPS)" \
    "$FLAP_AV_STARTS av_gate(av_starts) + {p50: .audio.av_offset_ms.p50, returns: (.log.playout_returns | length)}"
  require no_output_outages '.output_outages == []' '.output_outages | length'
  require counter_regressions '.camera.counter_regressions == 0'
  require_no_false_restart
}
