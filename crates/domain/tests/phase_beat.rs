//! C2 (S3): a programme phase change reaches web on the next beat, not on the 5 s cadence. The
//! supervisor beats on the tick a phase changes once `Thresholds::phase_beat_gap_ms` has passed since
//! its last beat, and otherwise on the first tick the gap allows. So the phase web last read is never
//! behind the programme's for more than the gap (and the tick that lands after it), and no two beats
//! are closer than the gap however the camera flaps. A phase that changes and changes back inside
//! the gap is superseded: the beat carries the phase of its tick, which web already had. Swept over
//! seeded camera scripts that flap faster than the gap, at three tick periods and two cadences (the
//! default's 5 s and a session's 2 s). Every expected bound is the threshold's own declaration. They
//! live here, not inline, because the domain's crate root forbids `clippy::print_stderr`, and a sweep
//! must print the count it checked.
use domain::config::{OutputProfile, Thresholds};
use domain::lifecycle::Lifecycle;
use domain::programme::ProgrammePhase;
use domain::supervisor::{Effect, Event, Supervisor};
use domain::time::Ms;

const FAR: Ms = 10_000_000;

/// The tick periods swept: C08's frame period, a fine one and a coarse one.
fn steps() -> [Ms; 3] {
    [OutputProfile::P720.frame_period_ms().round() as Ms, 10, 100]
}

/// A small deterministic generator (an LCG): no RNG crate, and the same cases every run.
struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) % n
    }
}

/// A booted supervisor at 0 on `th`, beating every `interval` ms (the session's).
fn booted(th: &Thresholds, interval: Ms) -> Supervisor {
    let s = Supervisor::new(th.clone()).decide(Event::Tick, 0).next;
    let d = s.decide(
        Event::BootstrapOk {
            heartbeat_interval_ms: interval,
            deadline_at: FAR,
        },
        0,
    );
    assert_eq!(d.next.lifecycle(), Lifecycle::Running, "setup: booted");
    d.next
}

/// The camera's script: alternating runs of frames and of silence, `(frames, ms)`, from 0. The
/// silences run from under `frozen_after_ms` to past `slate_after_ms`, so every phase the table has
/// is reached, and short runs of frames after a long enough silence flip Live and Frozen faster
/// than the gap.
fn script(seed: u64, th: &Thresholds, span: Ms) -> Vec<(bool, Ms)> {
    let mut r = Lcg(seed);
    let mut out = Vec::new();
    let mut at = 0;
    let mut frames = true;
    while at < span {
        let len = if frames {
            // From a single frame to past the 1 s a return needs.
            50 + r.next(th.resume_after_ms + 600)
        } else {
            // From under the freeze to past the slate.
            100 + r.next(th.slate_after_ms + 1_000)
        };
        out.push((frames, len));
        at += len;
        frames = !frames;
    }
    out
}

fn frames_at(script: &[(bool, Ms)], at: Ms) -> bool {
    let mut from = 0;
    for &(frames, len) in script {
        if at < from + len {
            return frames;
        }
        from += len;
    }
    false
}

/// One run: every beat `(at, phase it carried)` and every phase change `(at, to)`, on the tick grid.
struct Seen {
    beats: Vec<(Ms, ProgrammePhase)>,
    changes: Vec<(Ms, ProgrammePhase)>,
    /// Each tick after which web's phase (the last beat's) is not the programme's: (tick, how long
    /// the programme has been in its phase).
    stale: Vec<(Ms, Ms)>,
}

fn drive(th: &Thresholds, interval: Ms, step: Ms, script: &[(bool, Ms)], span: Ms) -> Seen {
    let mut s = booted(th, interval);
    let mut seen = Seen {
        beats: Vec::new(),
        changes: Vec::new(),
        stale: Vec::new(),
    };
    let mut entered = 0;
    let mut now = 0;
    while now <= span {
        if frames_at(script, now) {
            // The current generation connects (a no-op once it runs) and delivers this instant's
            // frame: a rebuilt generation's frames count as the first one's did.
            let gen = s.input_generation();
            s = s.decide(Event::InputConnected { gen }, now).next;
            s = s.decide(Event::InputFrame { gen }, now).next;
        }
        let before = s.programme_phase();
        let d = s.decide(Event::Tick, now);
        for e in &d.effects {
            if let Effect::SendHeartbeat(h) = e {
                assert_eq!(
                    h.programme_phase,
                    d.next.programme_phase(),
                    "a beat carries the phase of its own tick"
                );
                seen.beats.push((now, h.programme_phase));
            }
        }
        s = d.next;
        if s.programme_phase() != before {
            seen.changes.push((now, s.programme_phase()));
            entered = now;
        }
        if seen.beats.last().is_some_and(|b| b.1 != s.programme_phase()) {
            seen.stale.push((now, now - entered));
        }
        now += step;
    }
    seen
}

/// No two beats closer than the gap; web's phase never behind the programme's for the gap and a tick
/// (`stale`); and every change not superseded inside the gap on a beat within it, carrying it. A
/// change too near the run's end (`span`) for its bound to have passed is not read. Returns
/// (changes read, beat on their own tick, held by the gap, superseded inside it).
fn check(seen: &Seen, th: &Thresholds, interval: Ms, step: Ms, span: Ms, label: &str) -> [usize; 4] {
    let gap = th.phase_beat_gap_ms();
    for w in seen.beats.windows(2) {
        assert!(
            w[1].0 - w[0].0 >= gap.min(interval),
            "{label}: beats at {} and {} are closer than the gap {gap}",
            w[0].0,
            w[1].0
        );
    }
    for &(at, behind) in &seen.stale {
        assert!(
            behind < gap + step,
            "{label}: at {at} web's phase is {behind} ms behind (gap {gap}, step {step})"
        );
    }
    let [mut own_tick, mut held, mut superseded] = [0; 3];
    let read: Vec<(usize, Ms, ProgrammePhase)> = seen
        .changes
        .iter()
        .enumerate()
        .map(|(i, c)| (i, c.0, c.1))
        .filter(|c| c.1 + gap + step <= span)
        .collect();
    for &(i, at, to) in &read {
        let next = seen.beats.iter().find(|b| b.0 >= at).copied();
        let again = seen.changes.get(i + 1).map(|c| c.0);
        let web = seen.beats.iter().rev().find(|b| b.0 < at).map(|b| b.1);
        if web == Some(to) || again.is_some_and(|c| next.is_none_or(|b| c <= b.0)) {
            // Back to the phase web has, which owes no beat, or changed again before a beat: the
            // later phase is the one owed (`stale` bounds both).
            superseded += 1;
            continue;
        }
        let next = next.unwrap_or_else(|| panic!("{label}: no beat after the change to {to:?} at {at}"));
        assert!(
            next.0 - at <= gap + step,
            "{label}: the change to {to:?} at {at} waited for the beat at {} (gap {gap}, step {step})",
            next.0
        );
        assert_eq!(next.1, to, "{label}: the change's beat carries it");
        if next.0 == at {
            own_tick += 1;
        } else {
            held += 1;
        }
    }
    [read.len(), own_tick, held, superseded]
}

#[test]
fn every_phase_change_beats_within_the_gap_and_no_two_beats_are_closer() {
    let th = Thresholds::default();
    let span = 120_000;
    let mut runs = 0;
    let mut n = [0usize; 4];
    let mut stale = 0;
    for interval in [th.heartbeat_interval_ms, 2_000] {
        for step in steps() {
            for seed in 1..=6u64 {
                let sc = script(seed, &th, span);
                let seen = drive(&th, interval, step, &sc, span);
                let label = format!("interval {interval}, step {step}, seed {seed}");
                let got = check(&seen, &th, interval, step, span, &label);
                runs += 1;
                stale += seen.stale.len();
                for (sum, k) in n.iter_mut().zip(got) {
                    *sum += k;
                }
            }
        }
    }
    let [changes, own_tick, held, superseded] = n;
    eprintln!(
        "phase beat sweep: {runs} runs, {changes} phase changes read: {own_tick} beat on their own \
         tick, {held} held by the gap, {superseded} superseded inside it; {stale} ticks behind, each \
         checked against the gap"
    );
    // Anti-vacuity: changes were read, each branch of the rule was taken, and the scripts flapped
    // faster than the gap (a held or superseded change is one inside it).
    assert!(changes > 0 && own_tick > 0 && held > 0 && superseded > 0 && stale > 0);
}

/// Each phase the table has is reached by the sweep's scripts, so the bound is not read on a subset.
#[test]
fn the_sweep_reaches_every_camera_phase() {
    let th = Thresholds::default();
    let span = 120_000;
    let mut reached = std::collections::BTreeSet::new();
    for seed in 1..=6u64 {
        let sc = script(seed, &th, span);
        let step = steps()[0];
        for (_, to) in drive(&th, th.heartbeat_interval_ms, step, &sc, span).changes {
            reached.insert(format!("{to:?}"));
        }
    }
    eprintln!("phases reached: {reached:?}");
    for p in ["Live", "Frozen", "Reconnecting"] {
        assert!(reached.contains(p), "{p} never reached: {reached:?}");
    }
}

/// The edge beat in isolation: the camera goes live, then stops, with each change at least the gap
/// from the beat before it. Each change beats on its own tick, the cadence restarts from each edge
/// beat, so no cadence beat comes between them, and the only beats are the boot's and the edges'.
#[test]
fn a_slates_entry_and_exit_each_beat_on_their_own_tick() {
    let th = Thresholds::default();
    let gap = th.phase_beat_gap_ms();
    let interval = th.heartbeat_interval_ms;
    // Frames from 1 s for 3 s: Live a return's length in, then Frozen and Reconnecting after the
    // stop. Each edge is at least the gap after the one before it (the setup asserts it), and the
    // walk ends before a cadence beat is due after the last one.
    let sc = [(false, 1_000), (true, 3_000), (false, th.slate_after_ms + 2_000)];
    let span = 1_000 + 3_000 + th.slate_after_ms + 2_000;
    let mut checked = 0;
    for step in steps() {
        let seen = drive(&th, interval, step, &sc, span);
        let want = vec![
            ProgrammePhase::Live,
            ProgrammePhase::Frozen,
            ProgrammePhase::Reconnecting,
        ];
        assert_eq!(
            seen.changes.iter().map(|c| c.1).collect::<Vec<_>>(),
            want,
            "setup: the walk"
        );
        let edges: Vec<Ms> = seen.changes.iter().map(|c| c.0).collect();
        assert!(
            edges.windows(2).all(|w| w[1] - w[0] >= gap) && edges[0] >= gap,
            "setup: each edge the gap from the last: {edges:?}"
        );
        assert!(
            edges[2] + interval > span,
            "setup: no cadence beat is due in the walk"
        );
        let mut want_beats = vec![(0, ProgrammePhase::Waiting)];
        want_beats.extend(seen.changes.iter().copied());
        assert_eq!(seen.beats, want_beats, "step {step}");
        checked += seen.changes.len();
    }
    eprintln!("edge beats checked: {checked}");
    assert_eq!(checked, 3 * steps().len());
}

/// The gap against the programme's own behaviour (C2 review, m3): the reconnecting slate's shortest
/// stay, read off the real supervisor. The camera stops for exactly `slate_after_ms`, so its frames
/// return on the slate's first ticks, and the stay from the slate's entry to the return to Live is
/// the shortest the table allows. It is at least the gap, so the exit beats on its own tick too.
#[test]
fn a_slates_shortest_stay_is_at_least_the_gap() {
    let th = Thresholds::default();
    let gap = th.phase_beat_gap_ms();
    let sc = [(true, 2_000), (false, th.slate_after_ms), (true, 4_000)];
    let span = 2_000 + th.slate_after_ms + 4_000;
    let mut checked = 0;
    for step in steps() {
        let seen = drive(&th, th.heartbeat_interval_ms, step, &sc, span);
        let entry = seen
            .changes
            .iter()
            .find(|c| c.1 == ProgrammePhase::Reconnecting)
            .map(|c| c.0)
            .expect("setup: the slate goes up");
        let exit = seen
            .changes
            .iter()
            .find(|c| c.1 == ProgrammePhase::Live && c.0 > entry)
            .map(|c| c.0)
            .expect("setup: the camera returns");
        let stay = exit - entry;
        eprintln!("step {step}: the slate up at {entry}, down at {exit}: {stay} ms");
        assert!(
            stay <= th.resume_after_ms + 2 * step,
            "setup: the frames return on the slate's first ticks ({stay} ms)"
        );
        assert!(
            stay >= gap,
            "step {step}: a {stay} ms stay is under the {gap} ms gap"
        );
        assert!(
            seen.beats.contains(&(exit, ProgrammePhase::Live)),
            "step {step}: the exit beats on its own tick"
        );
        checked += 1;
    }
    eprintln!("shortest slate stays checked: {checked}");
    assert_eq!(checked, steps().len());
}
