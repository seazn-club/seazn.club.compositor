//! C6 (F-041): the recovery beat. Web reaches the relay only in a heartbeat's reply (`phoneLiveEdge`,
//! Owed 108 fix (b)), so while the camera is down the relay beats at `input_recovery_beat_gap_ms`
//! instead of every `heartbeat_interval_ms`, for `input_recovery_beat_window_ms` from the instant the
//! link left Running, and then falls back to the plain cadence. A link that never ran, a camera that
//! runs, and the final hold keep the plain cadence.
//!
//! Every expectation comes from `Thresholds` and the tick grid, swept over four tick periods, and each
//! sweep prints its count (the domain's crate root forbids `clippy::print_stderr`, so it lives here).
use domain::config::Thresholds;
use domain::lifecycle::Lifecycle;
use domain::supervisor::{Desired, Effect, Event, Supervisor};
use domain::time::Ms;

/// 1 ms lands on each rule's instant exactly, 33 ms is C08's frame period, the coarse two put it
/// between ticks.
const STEPS: [Ms; 4] = [1, 33, 250, 1_000];
const FAR: Ms = 100_000_000;

fn t() -> Thresholds {
    Thresholds::default()
}

/// The supervisor on a tick grid from a boot at 0, recording the instant of every beat.
#[derive(Debug, Clone)]
struct Run {
    s: Supervisor,
    now: Ms,
    step: Ms,
    beats: Vec<Ms>,
    /// Frames are sent on each tick while this holds, to the generation the link is on.
    camera: bool,
    connected: u64,
}

impl Run {
    fn booted(step: Ms, th: Thresholds) -> Self {
        let hb = th.heartbeat_interval_ms;
        let s = Supervisor::new(th).decide(Event::Tick, 0).next;
        let d = s.decide(
            Event::BootstrapOk {
                heartbeat_interval_ms: hb,
                deadline_at: FAR,
            },
            0,
        );
        assert_eq!(d.next.lifecycle(), Lifecycle::Running, "setup: booted");
        Self {
            s: d.next,
            now: 0,
            step,
            beats: Vec::new(),
            camera: false,
            connected: 0,
        }
    }

    fn send(&mut self, e: Event) {
        let d = self.s.decide(e, self.now);
        self.beats.extend(d.effects.iter().filter_map(|fx| match fx {
            Effect::SendHeartbeat(_) => Some(self.now),
            _ => None,
        }));
        self.s = d.next;
    }

    /// One tick: with the camera on, the current generation connects (once) and delivers a frame.
    fn tick(&mut self) {
        if self.camera && self.s.input_generation() > 0 {
            let gen = self.s.input_generation();
            if self.connected != gen {
                self.send(Event::InputConnected { gen });
                self.connected = gen;
            }
            self.send(Event::InputFrame { gen });
        }
        self.send(Event::Tick);
        self.now += self.step;
    }

    fn run_to(&mut self, until: Ms) {
        while self.now < until {
            self.tick();
        }
    }

    /// The camera runs to `until`, then goes silent: no frames, so the stall rule makes the link
    /// leave Running. Returns the last tick that carried a frame.
    fn run_then_drop(&mut self, until: Ms) -> Ms {
        self.camera = true;
        self.run_to(until);
        let last = self.now - self.step;
        self.camera = false;
        last
    }

    /// The beats from instant `from` (inclusive) to `to` (exclusive).
    fn beats_in(&self, from: Ms, to: Ms) -> Vec<Ms> {
        self.beats
            .iter()
            .copied()
            .filter(|&b| b >= from && b < to)
            .collect()
    }
}

fn gaps(beats: &[Ms]) -> Vec<Ms> {
    beats.windows(2).map(|w| w[1] - w[0]).collect()
}

/// The instant the link leaves Running after the last frame at `last`: the first tick at or after the
/// stall bound. Everything here is stated from the thresholds, never read from the supervisor.
fn stall_tick(last: Ms, step: Ms, th: &Thresholds) -> Ms {
    (last + th.input_stall_after_ms).div_ceil(step) * step
}

/// A camera that dropped: inside the window the beats are at most one gap and a tick apart and never
/// closer than the gap; after it they are the plain cadence's.
#[test]
fn a_camera_down_beats_at_the_recovery_gap_for_the_window_and_then_at_the_cadence() {
    let th = t();
    let gap = th.input_recovery_beat_gap_ms();
    let window = th.input_recovery_beat_window_ms;
    let mut checked = 0;
    for step in STEPS {
        let mut r = Run::booted(step, th.clone());
        let last = r.run_then_drop(6_000);
        let down = stall_tick(last, step, &th);
        r.run_to(down + window + 4 * th.heartbeat_interval_ms);
        // Inside the window, from the first beat the outage makes.
        let inside = r.beats_in(down, down + window);
        assert!(!inside.is_empty(), "step {step}: no beat in the window");
        for g in gaps(&inside) {
            assert!(
                g >= gap,
                "step {step}: a beat {g} ms after the last, under the gap {gap}"
            );
            assert!(
                g <= gap + step,
                "step {step}: {g} ms between recovery beats, over the gap {gap} and a tick"
            );
            checked += 1;
        }
        // The floor of the count: a beat every gap and a tick.
        let floor = (window / (gap + step)).saturating_sub(1) as usize;
        assert!(
            inside.len() >= floor,
            "step {step}: {} beats in the window, under {floor}",
            inside.len()
        );
        // After the window the cadence is the plain one, from the first beat past it.
        let after = r.beats_in(down + window + gap, FAR);
        assert!(after.len() >= 3, "step {step}: setup, beats after the window");
        for g in gaps(&after) {
            assert!(
                g >= th.heartbeat_interval_ms && g <= th.heartbeat_interval_ms + step,
                "step {step}: {g} ms between beats past the window"
            );
            checked += 1;
        }
    }
    eprintln!("recovery-beat gaps checked: {checked}");
    assert!(checked >= 4 * 20, "too few gaps checked: {checked}");
}

/// The empty case: a link that has never run (a phone that has not started) is Backoff for as long as
/// it likes and beats at the plain cadence, because the first `phoneLiveEdge` is a baseline and a
/// faster beat shortens nothing.
#[test]
fn a_link_that_never_ran_keeps_the_plain_cadence() {
    let th = t();
    let mut checked = 0;
    for step in STEPS {
        let mut r = Run::booted(step, th.clone());
        r.run_to(3 * th.input_recovery_beat_window_ms);
        let gs = gaps(&r.beats);
        assert!(gs.len() >= 10, "step {step}: setup, {} gaps", gs.len());
        for g in gs {
            assert!(
                g >= th.heartbeat_interval_ms && g <= th.heartbeat_interval_ms + step,
                "step {step}: {g} ms between beats with no camera yet"
            );
            checked += 1;
        }
    }
    eprintln!("never-ran gaps checked: {checked}");
    assert!(checked >= 4 * 10);
}

/// A camera that runs beats at the plain cadence: the rule adds nothing to a healthy session.
#[test]
fn a_running_camera_keeps_the_plain_cadence() {
    let th = t();
    let mut checked = 0;
    for step in STEPS {
        let mut r = Run::booted(step, th.clone());
        r.camera = true;
        r.run_to(3 * th.input_recovery_beat_window_ms);
        // From the first beat past the start-up, whose Slate-to-Live phase change is C2's edge beat.
        let gs = gaps(&r.beats_in(2 * th.heartbeat_interval_ms, FAR));
        assert!(gs.len() >= 10, "step {step}: setup");
        for g in gs {
            assert!(
                g >= th.heartbeat_interval_ms && g <= th.heartbeat_interval_ms + step,
                "step {step}: {g} ms between beats with the camera running"
            );
            checked += 1;
        }
    }
    eprintln!("running gaps checked: {checked}");
    assert!(checked >= 4 * 10);
}

/// A second outage after the camera returned has its own window; the return ends the first.
#[test]
fn a_return_ends_the_window_and_the_next_outage_gets_its_own() {
    let th = t();
    let gap = th.input_recovery_beat_gap_ms();
    let mut checked = 0;
    for step in STEPS {
        let mut r = Run::booted(step, th.clone());
        let last = r.run_then_drop(6_000);
        let down = stall_tick(last, step, &th);
        r.run_to(down + 10_000);
        // The camera comes back: the link rebuilds, connects on the next tick, and runs.
        r.camera = true;
        let back = r.now + 20_000;
        r.run_to(back);
        let settled = r.beats_in(back - 12_000, back);
        for g in gaps(&settled) {
            assert!(
                g >= th.heartbeat_interval_ms,
                "step {step}: {g} ms between beats after the return"
            );
            checked += 1;
        }
        assert!(settled.len() >= 2, "step {step}: setup, beats after the return");
        // Down again: a fresh window, beating at the gap at once.
        r.camera = false;
        let last2 = r.now - step;
        let down2 = stall_tick(last2, step, &th);
        r.run_to(down2 + 10_000);
        let second = r.beats_in(down2, down2 + 10_000);
        assert!(
            second.len() >= (10_000 / (gap + step)) as usize - 1,
            "step {step}: {second:?}"
        );
        for g in gaps(&second) {
            assert!(g <= gap + step, "step {step}: {g} ms in the second window");
            checked += 1;
        }
    }
    eprintln!("window restarts checked: {checked}");
    assert!(checked >= 4 * 10);
}

/// The final hold ticks no link: an outage that was beating at the gap beats at the plain cadence
/// once web asks the session to end.
#[test]
fn the_final_hold_keeps_the_plain_cadence() {
    let th = t();
    let mut checked = 0;
    for step in [33, 250] {
        let mut r = Run::booted(step, th.clone());
        let last = r.run_then_drop(6_000);
        let down = stall_tick(last, step, &th);
        r.run_to(down + 5_000);
        assert!(r.s.input_generation() > 0, "setup: a link");
        r.send(Event::HeartbeatReply(Desired::Ending));
        let hold_from = r.now;
        r.run_to(hold_from + th.final_hold_ms - 2 * step);
        let held = r.beats_in(hold_from, FAR);
        assert!(held.len() >= 2, "step {step}: setup, beats in the hold: {held:?}");
        for g in gaps(&held) {
            assert!(
                g >= th.heartbeat_interval_ms,
                "step {step}: {g} ms between beats in the hold"
            );
            checked += 1;
        }
    }
    eprintln!("hold gaps checked: {checked}");
    assert!(checked >= 2);
}

/// A window of zero turns the rule off: the guard is `elapsed < window`, never `<=`.
#[test]
fn a_zero_window_is_the_plain_cadence() {
    let th = Thresholds {
        input_recovery_beat_window_ms: 0,
        ..t()
    };
    let mut r = Run::booted(33, th.clone());
    let last = r.run_then_drop(6_000);
    let down = stall_tick(last, 33, &th);
    r.run_to(down + 40_000);
    let gs = gaps(&r.beats_in(down, FAR));
    assert!(gs.len() >= 5, "setup");
    eprintln!("zero-window gaps checked: {}", gs.len());
    assert!(gs.iter().all(|&g| g >= th.heartbeat_interval_ms), "{gs:?}");
}

/// The window's last instant: a recovery beat is due while the outage is OLDER than the window by less
/// than the window, so with the grid on the instant (1 ms ticks) the beat that would land exactly
/// `window` after the outage began does not. The window is built from a first run's own first
/// recovery beat, so the beat the boundary refuses is one the gap would otherwise have made.
#[test]
fn the_window_is_open_at_its_start_and_shut_at_its_end() {
    let th = t();
    let gap = th.input_recovery_beat_gap_ms();
    let mut probe = Run::booted(1, th.clone());
    let last = probe.run_then_drop(6_000);
    let down = stall_tick(last, 1, &th);
    probe.run_to(down + 3 * gap);
    let first = *probe
        .beats_in(down, FAR)
        .first()
        .expect("setup: a beat in the outage");
    // Five gaps of recovery beats from `first`, the sixth at exactly the window's end.
    let window = first - down + 5 * gap;
    let tight = Thresholds {
        input_recovery_beat_window_ms: window,
        ..th.clone()
    };
    let mut r = Run::booted(1, tight);
    let last = r.run_then_drop(6_000);
    assert_eq!(stall_tick(last, 1, &th), down, "setup: the same outage");
    r.run_to(down + window + th.heartbeat_interval_ms);
    let mut checked = 0;
    for i in 0..5 {
        let at = first + i * gap;
        assert_eq!(
            r.beats_in(at, at + 1),
            vec![at],
            "a recovery beat at {at} (gap {i})"
        );
        checked += 1;
    }
    let edge = down + window;
    assert_eq!(
        first + 5 * gap,
        edge,
        "setup: the sixth beat lands on the window's end"
    );
    assert_eq!(
        r.beats_in(edge, edge + 1),
        Vec::<Ms>::new(),
        "no recovery beat at the window's end"
    );
    eprintln!("window boundary checked: {}", checked + 1);
    assert_eq!(checked, 5);
}
