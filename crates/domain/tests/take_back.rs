//! C3, S2 (Owed 49): a camera generation the domain gave up by its own rule, whose build delivers a
//! decoded frame before its rebuild is due, is taken back: Running again, no rebuild, and
//! `Effect::TakeBackInput` re-opens its gate. The window is the back-off itself. And (S2 item 2) while
//! the build's source bytes still rise, the stall rule waits `input_decode_wait_ms` instead of
//! `input_stall_after_ms`; when they stop, the plain bound applies again.
//!
//! Every expected instant is derived from `Thresholds::default()` and the tick grid: an effect comes
//! on the first tick at or after the instant its rule names. Swept over a 1 ms grid (each rule's exact
//! instant) and each output profile's frame period (C08's tick). These live here, not inline, because
//! the domain's crate root forbids `clippy::print_stderr`, and a sweep must print the count it checked.
use domain::config::{OutputProfile, Thresholds};
use domain::input_link::{BuildFacts, GiveUpReason, InputAction, InputLink, InputState, LossReason};
use domain::lifecycle::Lifecycle;
use domain::programme::ProgrammePhase;
use domain::supervisor::{Effect, Event, Supervisor};
use domain::time::Ms;

const FAR: Ms = 10_000_000;

fn t() -> Thresholds {
    Thresholds::default()
}

/// The grids swept: 1 ms lands on each instant exactly; each profile's frame period is C08's tick.
fn steps() -> Vec<(String, Ms)> {
    let mut v = vec![("1 ms".to_owned(), 1)];
    for p in [OutputProfile::P720, OutputProfile::P1080] {
        v.push((format!("{p:?}"), p.frame_period_ms().round() as Ms));
    }
    v
}

fn first_tick(at: Ms, step: Ms) -> Ms {
    at.div_ceil(step) * step
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Did {
    Build(u64),
    Abandon(u64),
    TakeBack(u64),
}

/// The supervisor, booted at 0 and driven on a grid; the adapter's reports at each instant go in
/// before its tick.
struct Walk {
    s: Supervisor,
    now: Ms,
    step: Ms,
    log: Vec<(Ms, Did)>,
    /// The programme's phase at each change, as the ticks left it.
    phases: Vec<(Ms, ProgrammePhase)>,
}

impl Walk {
    fn new(step: Ms) -> Self {
        let th = t();
        let s = Supervisor::new(th.clone()).decide(Event::Tick, 0).next;
        let d = s.decide(
            Event::BootstrapOk {
                heartbeat_interval_ms: th.heartbeat_interval_ms,
                deadline_at: FAR,
            },
            0,
        );
        assert_eq!(d.next.lifecycle(), Lifecycle::Running, "setup: booted");
        Self {
            s: d.next,
            now: 0,
            step,
            log: Vec::new(),
            phases: Vec::new(),
        }
    }

    fn send(&mut self, e: Event) {
        let d = self.s.decide(e, self.now);
        for fx in &d.effects {
            match *fx {
                Effect::RebuildInput { gen } => self.log.push((self.now, Did::Build(gen))),
                Effect::AbandonInput { gen } => self.log.push((self.now, Did::Abandon(gen))),
                Effect::TakeBackInput { gen } => self.log.push((self.now, Did::TakeBack(gen))),
                _ => {}
            }
        }
        self.s = d.next;
        let p = self.s.programme_phase();
        if self.phases.last().map(|&(_, q)| q) != Some(p) {
            self.phases.push((self.now, p));
        }
    }

    fn until(&mut self, until: Ms, mut events: impl FnMut(Ms) -> Vec<Event>) {
        while self.now <= until {
            for e in events(self.now) {
                self.send(e);
            }
            self.send(Event::Tick);
            self.now += self.step;
        }
    }

    fn input(&self) -> InputState {
        self.s.input_nudge().state
    }

    fn after(&self, from: Ms) -> Vec<(Ms, Did)> {
        self.log.iter().copied().filter(|&(at, _)| at >= from).collect()
    }
}

/// The camera, as the adapter reports it: gen 1 connects at 100 and sends a frame each instant in
/// `[100, stop)`; `again` (the abandon's instant → the instant the gated build delivers again, or
/// `None`) models the build that recovers behind its shut gate; frames go on after a take-back.
struct Camera {
    stop: Ms,
    connected: bool,
    last_frame: Option<Ms>,
    abandoned_at: Option<Ms>,
    again_at: Option<Ms>,
    taken_back: bool,
}

impl Camera {
    fn new(stop: Ms) -> Self {
        Self {
            stop,
            connected: false,
            last_frame: None,
            abandoned_at: None,
            again_at: None,
            taken_back: false,
        }
    }
}

// ---- S2 item 1: the take-back ----

/// Abandon → the gated build delivers `delay` after the abandon. Returns the walk, the abandon's
/// instant and the instant the report went in (`None` if never sent).
fn abandon_then_again(step: Ms, delay: Option<Ms>, end_after: Ms) -> (Walk, Ms, Option<Ms>, Ms) {
    let th = t();
    let mut w = Walk::new(step);
    let mut cam = Camera::new(2_000);
    // Phase 1: frames to `stop`, then silence to the abandon.
    let silent_through = cam.stop + th.input_stall_after_ms + 2 * step;
    w.until(silent_through, |now| {
        let mut ev = Vec::new();
        if now >= 100 {
            if !cam.connected {
                cam.connected = true;
                ev.push(Event::InputConnected { gen: 1 });
            }
            if now < cam.stop {
                cam.last_frame = Some(now);
                ev.push(Event::InputFrame { gen: 1 });
            }
        }
        ev
    });
    let last = cam.last_frame.expect("setup: frames went out");
    let abandons: Vec<_> = w
        .log
        .iter()
        .filter(|(_, d)| matches!(d, Did::Abandon(_)))
        .collect();
    assert_eq!(abandons.len(), 1, "setup: one abandon {:?}", w.log);
    let (abandon_at, did) = *abandons[0];
    assert_eq!(did, Did::Abandon(1));
    assert_eq!(
        abandon_at,
        first_tick(last + th.input_stall_after_ms, step),
        "the stall's own bound (no bytes reported)"
    );
    cam.abandoned_at = Some(abandon_at);
    cam.again_at = delay.map(|d| first_tick(abandon_at + d, step));
    let again_at = cam.again_at;
    // Phase 2: the gated build delivers again at `again_at` (the adapter's one report), then frames.
    let end = abandon_at + end_after;
    w.until(end, |now| {
        let mut ev = Vec::new();
        if let Some(a) = cam.again_at {
            if now == a {
                ev.push(Event::InputAgain { gen: 1 });
                cam.taken_back = true;
            }
            if cam.taken_back {
                ev.push(Event::InputFrame { gen: 1 });
            }
        }
        ev
    });
    (w, abandon_at, again_at, last)
}

// The brief's first sequence: abandon → a frame at +300 ms → taken back, no rebuild. The camera is
// live again after the resume hysteresis, and the old rebuild instant passes with no build.
#[test]
fn a_build_that_delivers_300_ms_after_its_abandon_is_taken_back_without_a_rebuild() {
    let th = t();
    assert!(
        300 < th.input_backoff_base_ms,
        "setup: +300 ms is inside the first back-off"
    );
    let mut checked = 0;
    for (label, step) in steps() {
        let end_after = th.input_backoff_base_ms + th.resume_after_ms + 2_000;
        let (w, abandon_at, again_at, _) = abandon_then_again(step, Some(300), end_after);
        let again_at = again_at.expect("sent");
        assert_eq!(
            w.after(abandon_at + 1),
            vec![(again_at, Did::TakeBack(1))],
            "{label}: one take-back at the report, no build after"
        );
        assert_eq!(w.s.input_generation(), 1, "{label}: no rebuild");
        assert!(
            matches!(w.input(), InputState::Running { since } if since == again_at),
            "{label}: Running from the take-back, {:?}",
            w.input()
        );
        assert_eq!(w.s.input_give_ups(), 1, "{label}");
        assert_eq!(w.s.programme_phase(), ProgrammePhase::Live, "{label}: live again");
        checked += 1;
    }
    eprintln!("take-back at +300 ms checked: {checked} grids");
    assert_eq!(checked, steps().len());
}

// The brief's second: abandon → no frame → the rebuild at the old time, one first back-off after the
// abandon (a stall from Running is the first failure).
#[test]
fn a_build_that_stays_silent_is_rebuilt_at_the_old_instant() {
    let th = t();
    let mut checked = 0;
    for (label, step) in steps() {
        let (w, abandon_at, _, _) = abandon_then_again(step, None, th.input_backoff_base_ms + 2 * step);
        assert_eq!(
            w.after(abandon_at + 1),
            vec![(
                first_tick(abandon_at + th.input_backoff_base_ms, step),
                Did::Build(2)
            )],
            "{label}"
        );
        assert!(
            !w.log.iter().any(|(_, d)| matches!(d, Did::TakeBack(_))),
            "{label}"
        );
        checked += 1;
    }
    eprintln!("silent abandons checked: {checked} grids");
    assert_eq!(checked, steps().len());
}

// The window's edges, on the 1 ms grid of the link itself: a report 1 ms before the rebuild is due
// takes the build back; one at the rebuild's instant does not (the tick builds as it would have).
#[test]
fn the_window_is_the_back_off_exactly() {
    let th = t();
    let mut l = InputLink::new();
    assert_eq!(l.on_tick(0, &th), Some(InputAction::Rebuild { gen: 1 }));
    l.on_connected(100, 1);
    let at = 100 + th.input_stall_after_ms;
    assert_eq!(
        l.check_stall(at, Some(100), &th),
        Some(InputAction::Abandon { gen: 1 })
    );
    let InputState::Backoff { until, .. } = l.state() else {
        panic!("setup: backing off");
    };
    assert_eq!(until, at + th.input_backoff_base_ms, "the first back-off");
    let mut early = l.clone();
    assert_eq!(
        early.on_again(until - 1, 1),
        Some(InputAction::TakeBack { gen: 1 })
    );
    assert_eq!(early.state(), InputState::Running { since: until - 1 });
    let mut late = l.clone();
    assert_eq!(late.on_again(until, 1), None, "at the rebuild's instant");
    assert_eq!(late, l, "changed nothing");
    assert_eq!(late.on_tick(until, &th), Some(InputAction::Rebuild { gen: 2 }));
    // At the abandon's own instant it is inside the window too.
    let mut same = l.clone();
    assert_eq!(same.on_again(at, 1), Some(InputAction::TakeBack { gen: 1 }));
}

// What is never taken back: a stale generation, a build the adapter reported lost (EOS, an error, the
// hold-end shape), a second report after the take-back, a link that never gave up, and a build gone
// through a reported loss after its abandon. Each changes nothing at all.
#[test]
fn only_an_abandoned_current_build_is_taken_back() {
    let th = t();
    let abandoned = || {
        let mut l = InputLink::new();
        l.on_tick(0, &th);
        l.on_connected(100, 1);
        l.check_stall(100 + th.input_stall_after_ms, Some(100), &th);
        l
    };
    let now = 100 + th.input_stall_after_ms + 10;
    let mut cases: Vec<(&str, InputLink, u64)> = Vec::new();
    cases.push(("another generation", abandoned(), 2));
    cases.push(("a generation torn down", abandoned(), 0));
    for (name, reason) in [("eos", LossReason::Eos), ("error", LossReason::Error)] {
        let mut l = InputLink::new();
        l.on_tick(0, &th);
        l.on_connected(100, 1);
        l.on_lost(200, 1, reason, &th);
        cases.push((name, l, 1));
    }
    let mut erred = abandoned();
    erred.on_error(
        now - 5,
        1,
        BuildFacts {
            handshake_completed: true,
            carried_media: true,
        },
        Some(100),
        &th,
    );
    cases.push(("an error after the abandon", erred, 1));
    let mut twice = abandoned();
    assert!(twice.on_again(now - 5, 1).is_some(), "setup: taken back once");
    cases.push(("a second report", twice, 1));
    let mut running = InputLink::new();
    running.on_tick(0, &th);
    running.on_connected(100, 1);
    cases.push(("never given up", running, 1));
    let mut checked = 0;
    for (name, l, gen) in cases {
        let mut m = l.clone();
        assert_eq!(m.on_again(now, gen), None, "{name}");
        assert_eq!(m, l, "{name}: changed");
        checked += 1;
    }
    eprintln!("not-taken-back cases checked: {checked}");
    assert_eq!(checked, 7);
}

// The connect timeout and the phone-live discard are the link's own give-ups too: a build that
// delivers inside their back-off is taken back. A latched nudge's back-off is zero, so it never is.
#[test]
fn every_own_rule_give_up_can_be_taken_back() {
    let th = t();
    let mut l = InputLink::new();
    l.on_tick(0, &th);
    let timeout = th.input_connect_timeout_ms;
    assert_eq!(l.on_tick(timeout, &th), Some(InputAction::Abandon { gen: 1 }));
    assert_eq!(
        l.last_give_up().map(|g| g.reason),
        Some(GiveUpReason::ConnectTimeout)
    );
    assert_eq!(
        l.on_again(timeout + 300, 1),
        Some(InputAction::TakeBack { gen: 1 })
    );
    assert_eq!(l.state(), InputState::Running { since: timeout + 300 });
    // A loss reported while Connecting is never taken back.
    let mut l = InputLink::new();
    l.on_tick(0, &th);
    l.on_lost(500, 1, LossReason::Eos, &th);
    assert_eq!(l.on_again(600, 1), None);
}

// The brief's third: a take-back, then a second stall. The same generation is given up again (the
// count rises, the generation does not), at the stall's bound from the take-back's frames; that one
// can be taken back as well, or, silent, rebuilt one first back-off later.
#[test]
fn a_taken_back_build_can_stall_again_and_be_taken_back_again() {
    let th = t();
    let mut checked = 0;
    for (label, step) in steps() {
        for second_again in [true, false] {
            let mut w = Walk::new(step);
            let (stop1, mut last) = (2_000, 0);
            let mut connected = false;
            let mut phase = 0u8; // 0 live, 1 silent, 2 taken back + live, 3 silent again, 4 back
            let mut a1 = None;
            let mut stop2 = None;
            let mut a2 = None;
            let end = stop1 + 3 * th.input_stall_after_ms + 2 * th.input_backoff_base_ms + 2_000;
            while w.now <= end {
                let now = w.now;
                if now >= 100 && !connected {
                    connected = true;
                    w.send(Event::InputConnected { gen: 1 });
                }
                let abandons: Vec<Ms> = w
                    .log
                    .iter()
                    .filter_map(|&(at, d)| (d == Did::Abandon(1)).then_some(at))
                    .collect();
                match phase {
                    0 if now >= 100 && now < stop1 => {
                        last = now;
                        w.send(Event::InputFrame { gen: 1 });
                    }
                    0 if now >= stop1 => phase = 1,
                    1 if abandons.len() == 1 && now >= abandons[0] + 300 => {
                        a1 = Some(now);
                        w.send(Event::InputAgain { gen: 1 });
                        phase = 2;
                    }
                    2 if now < a1.unwrap() + 1_000 => {
                        last = now;
                        w.send(Event::InputFrame { gen: 1 });
                    }
                    2 => {
                        stop2 = Some(last);
                        phase = 3;
                    }
                    3 if second_again && abandons.len() == 2 && now >= abandons[1] + 300 => {
                        a2 = Some(now);
                        w.send(Event::InputAgain { gen: 1 });
                        phase = 4;
                    }
                    4 => w.send(Event::InputFrame { gen: 1 }),
                    _ => {}
                }
                w.send(Event::Tick);
                w.now += step;
            }
            let a1 = a1.expect("taken back once");
            let last2 = stop2.expect("the second silence");
            let second = first_tick(last2 + th.input_stall_after_ms, step);
            let abandons: Vec<(Ms, Did)> = w
                .log
                .iter()
                .copied()
                .filter(|(_, d)| matches!(d, Did::Abandon(_)))
                .collect();
            assert_eq!(abandons.len(), 2, "{label}: {:?}", w.log);
            assert_eq!(
                abandons[1],
                (second, Did::Abandon(1)),
                "{label}: the second stall"
            );
            assert_eq!(w.s.input_give_ups(), 2, "{label}");
            let after: Vec<_> = w.after(a1);
            if second_again {
                let a2 = a2.expect("taken back twice");
                assert_eq!(
                    after,
                    vec![
                        (a1, Did::TakeBack(1)),
                        (second, Did::Abandon(1)),
                        (a2, Did::TakeBack(1))
                    ],
                    "{label}"
                );
                assert_eq!(w.s.input_generation(), 1, "{label}");
            } else {
                assert_eq!(
                    after,
                    vec![
                        (a1, Did::TakeBack(1)),
                        (second, Did::Abandon(1)),
                        (first_tick(second + th.input_backoff_base_ms, step), Did::Build(2))
                    ],
                    "{label}: the second, silent, is rebuilt at its first back-off"
                );
            }
            checked += 1;
        }
    }
    eprintln!("second stalls checked: {checked}");
    assert_eq!(checked, 2 * steps().len());
}

// The hold takes nothing back (C06: it ticks no link, and the Final card stays on air).
#[test]
fn the_hold_takes_nothing_back() {
    let th = t();
    let (w, abandon_at, _, _) = abandon_then_again(1, None, 0);
    let s =
        w.s.decide(
            Event::HeartbeatReply(domain::supervisor::Desired::Ending),
            abandon_at + 1,
        )
        .next;
    assert!(matches!(s.lifecycle(), Lifecycle::Holding { .. }), "setup");
    let d = s.decide(Event::InputAgain { gen: 1 }, abandon_at + 2);
    assert!(d.effects.is_empty(), "{:?}", d.effects);
    assert_eq!(d.next, s);
    assert!(
        abandon_at + 2 < abandon_at + th.input_backoff_base_ms,
        "inside the window"
    );
}

// ---- S2 item 2: bytes still arriving ----

/// Frames to 2 000, then silence; bytes rise each instant in `[100, bytes_stop)` (a strictly rising
/// total), from generation `bytes_gen`, or repeat one total when `rising` is false. Returns the walk
/// and the last frame's and last byte's instants.
fn bytes_walk(step: Ms, bytes_stop: Ms, bytes_gen: u64, rising: bool) -> (Walk, Ms, Option<Ms>) {
    let th = t();
    let mut w = Walk::new(step);
    let stop = 2_000;
    let (mut last, mut last_byte) = (0, None);
    let mut connected = false;
    let mut total = 0u64;
    let end = stop + th.input_decode_wait_ms + th.input_bytes_fresh_ms + 4 * step;
    w.until(end, |now| {
        let mut ev = Vec::new();
        if now >= 100 {
            if !connected {
                connected = true;
                ev.push(Event::InputConnected { gen: 1 });
            }
            if now < stop {
                last = now;
                ev.push(Event::InputFrame { gen: 1 });
            }
            if now < bytes_stop {
                if rising || total == 0 {
                    total += 1_316;
                }
                last_byte = Some(now);
                ev.push(Event::InputBytes {
                    gen: bytes_gen,
                    total,
                });
            }
        }
        ev
    });
    (w, last, last_byte)
}

fn first_abandon(w: &Walk) -> Option<Ms> {
    w.log
        .iter()
        .find_map(|&(at, d)| matches!(d, Did::Abandon(_)).then_some(at))
}

// The brief's fourth: bytes flowing with no frame → no abandon until the extended bound; bytes stop
// → the abandon at the normal bound. And bytes that stop between the two bounds: one fresh window
// after the last rise. Bytes from another generation, or a total that does not rise, extend nothing.
#[test]
fn bytes_arriving_lengthen_the_stall_bound_and_their_end_restores_it() {
    let th = t();
    assert!(
        th.input_decode_wait_ms > th.input_stall_after_ms,
        "setup: an extension to see"
    );
    let mut checked = 0;
    for (label, step) in steps() {
        // Flowing through: the extended bound.
        let (w, last, _) = bytes_walk(step, FAR, 1, true);
        assert_eq!(
            first_abandon(&w),
            Some(first_tick(last + th.input_decode_wait_ms, step)),
            "{label}: bytes flowing"
        );
        // Stopped well before the plain bound: the plain bound.
        let (w, last, _) = bytes_walk(step, 2_000 + 1_000, 1, true);
        assert_eq!(
            first_abandon(&w),
            Some(first_tick(last + th.input_stall_after_ms, step)),
            "{label}: bytes stopped early"
        );
        // Stopped between the bounds: one fresh window after the last rise.
        let (w, last, lb) = bytes_walk(step, 2_000 + th.input_stall_after_ms + 1_000, 1, true);
        let lb = lb.expect("bytes went in");
        let want = first_tick(lb + th.input_bytes_fresh_ms, step);
        assert!(want > last + th.input_stall_after_ms && want < last + th.input_decode_wait_ms);
        assert_eq!(first_abandon(&w), Some(want), "{label}: bytes stopped late");
        // Another generation's bytes, or a total that never rises: the plain bound.
        for (what, gen, rising) in [("another gen", 2, true), ("a flat total", 1, false)] {
            let (w, last, _) = bytes_walk(step, FAR, gen, rising);
            assert_eq!(
                first_abandon(&w),
                Some(first_tick(last + th.input_stall_after_ms, step)),
                "{label}: {what}"
            );
            checked += 1;
        }
        checked += 3;
    }
    eprintln!("bytes cases checked: {checked}");
    assert_eq!(checked, 5 * steps().len());
}

// The slate still comes before the stall, flowing bytes or not: the extension waits on the decoder
// behind the slate the owner ruled, and changes nothing on air. The slate goes up inside its budget
// from the last frame, while the link is still Running (before the abandon).
#[test]
fn bytes_flowing_never_hold_off_the_slate() {
    let th = t();
    let mut checked = 0;
    for (label, step) in steps() {
        let (w, last, _) = bytes_walk(step, FAR, 1, true);
        let slate = w
            .phases
            .iter()
            .find_map(|&(at, p)| (at > last && p == ProgrammePhase::Reconnecting).then_some(at))
            .expect("the slate went up");
        assert!(slate <= last + th.slate_budget_ms(), "{label}: slate at {slate}");
        assert!(
            slate < first_abandon(&w).expect("abandoned"),
            "{label}: before the abandon"
        );
        checked += 1;
    }
    eprintln!("slates under flowing bytes checked: {checked}");
    assert_eq!(checked, steps().len());
}

// A rebuild starts the count again: the new build's first bytes are a rise from zero, and the old
// build's total read late (generation 1) changes nothing.
#[test]
fn a_rebuild_starts_the_byte_count_again() {
    let th = t();
    let mut l = InputLink::new();
    l.on_tick(0, &th);
    l.on_connected(100, 1);
    l.on_bytes(150, 1, 50_000);
    l.check_stall(100 + th.input_decode_wait_ms, Some(100), &th);
    let InputState::Backoff { until, .. } = l.state() else {
        panic!("setup: given up");
    };
    assert_eq!(l.on_tick(until, &th), Some(InputAction::Rebuild { gen: 2 }));
    l.on_connected(until + 10, 2);
    let before = l.clone();
    l.on_bytes(until + 20, 1, 60_000);
    assert_eq!(l, before, "the old build's count");
    l.on_bytes(until + 20, 2, 1_316);
    assert_ne!(l, before, "the new build's first bytes are a rise");
}
