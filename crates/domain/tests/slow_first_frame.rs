//! F-036 (D3; stg session 5e5403ce, 2026-11-24): a phone that republishes with a 3 s GOP behind a
//! 1 s SRT latency can take longer than F-002's 5 s to a first decoded frame, and the relay gave up
//! six connects in a row while the camera was back. The connect timeout is now 9 s (`config.rs`).
//!
//! Through the supervisor, on the tick grid of each output profile:
//! - a camera whose first decoded frame comes 6 to 8 s after its build is kept and goes back on air,
//!   twice in one session (a second return after the first);
//! - a phone-live nudge that lands while such a connect already carries bytes keeps it too;
//! - a pull that never decodes is still given up on the first tick at or after its build plus
//!   `input_connect_timeout_ms`, and no sooner.
//!
//! Each kept return is held against the same run at F-002's 5 s, which gives it up, so the sweep
//! reaches the rule it pins. These sweeps live here, not inline, because the domain's crate root
//! forbids `clippy::print_stderr`, and a sweep prints its count.
use domain::config::{OutputProfile, Thresholds};
use domain::lifecycle::Lifecycle;
use domain::programme::ProgrammePhase;
use domain::supervisor::{Effect, Event, Supervisor};
use domain::time::Ms;

/// F-036's stg window: the first decoded frame 6 to 8 s after the connect, in 0.5 s steps.
const FIRST_FRAME_AFTER: [Ms; 5] = [6_000, 6_500, 7_000, 7_500, 8_000];

/// F-002's connect timeout, the one the stg session ran with (F-036: each connect "abandoned by the
/// domain" about 5 s after its build). A literal on purpose: it is the old value, not today's.
const F002_CONNECT_TIMEOUT_MS: Ms = 5_000;

/// How long each return stays on air: past `resume_after_ms`, so the programme goes back to Live.
const ON_AIR_MS: Ms = 3_000;

const FAR: Ms = 10_000_000;

fn t() -> Thresholds {
    Thresholds::default()
}

fn at_f002(th: &Thresholds) -> Thresholds {
    Thresholds {
        input_connect_timeout_ms: F002_CONNECT_TIMEOUT_MS,
        ..th.clone()
    }
}

/// The tick grids swept, labelled: 1 ms lands on each rule's instant exactly; then each output
/// profile's frame period as C08 ticks it (ADR 0005: 30 fps in both, so the two grids match today,
/// and a profile whose rate moves gets its own); then a coarse 250 ms (under `frozen_after_ms`, so
/// frames on it still read as continuous).
fn grids() -> Vec<(String, Ms)> {
    let mut v = vec![("1 ms".to_string(), 1)];
    for (name, p) in [("720p", OutputProfile::P720), ("1080p", OutputProfile::P1080)] {
        v.push((format!("{name} frame tick"), p.frame_period_ms().floor() as Ms));
    }
    v.push(("250 ms".to_string(), 250));
    v
}

/// The first tick at or after `at` on a grid of `step` from 0.
fn first_tick(at: Ms, step: Ms) -> Ms {
    at.div_ceil(step) * step
}

/// The supervisor driven on a tick grid from a boot at 0, recording every input build and abandon.
struct Run {
    s: Supervisor,
    th: Thresholds,
    now: Ms,
    step: Ms,
    builds: Vec<(Ms, u64)>,
    abandons: Vec<(Ms, u64)>,
}

impl Run {
    /// Booted at 0; the first tick builds gen 1.
    fn booted(th: &Thresholds, step: Ms) -> Self {
        let s = Supervisor::new(th.clone()).decide(Event::Tick, 0).next;
        let d = s.decide(
            Event::BootstrapOk {
                heartbeat_interval_ms: th.heartbeat_interval_ms,
                deadline_at: FAR,
            },
            0,
        );
        assert_eq!(d.next.lifecycle(), Lifecycle::Running, "setup: booted");
        let mut r = Self {
            s: d.next,
            th: th.clone(),
            now: 0,
            step,
            builds: Vec::new(),
            abandons: Vec::new(),
        };
        r.instant(&[]);
        assert_eq!(r.builds, vec![(0, 1)], "setup: gen 1 at once");
        r
    }

    fn send(&mut self, e: Event) {
        let d = self.s.decide(e, self.now);
        for fx in &d.effects {
            match *fx {
                Effect::RebuildInput { gen } => self.builds.push((self.now, gen)),
                Effect::AbandonInput { gen } => self.abandons.push((self.now, gen)),
                _ => {}
            }
        }
        self.s = d.next;
    }

    /// One grid instant: the adapter's reports of it, then the tick.
    fn instant(&mut self, events: &[Event]) {
        for e in events {
            self.send(e.clone());
        }
        self.send(Event::Tick);
        self.now += self.step;
    }

    /// Generation `gen`, built at `built`, sends nothing until `built + after`, then its first decoded
    /// frame and a frame on every tick for `ON_AIR_MS`. `extra` events go in at the first instant at
    /// or after each one's time, before that instant's frame. Returns the last frame's instant.
    fn returns(&mut self, gen: u64, built: Ms, after: Ms, extra: &[(Ms, Event)]) -> Ms {
        let first = built + after;
        let end = first + ON_AIR_MS;
        let (mut sent, mut connected, mut last) = (0, false, 0);
        while self.now <= end {
            let mut e = Vec::new();
            while let Some((at, x)) = extra.get(sent) {
                if self.now < *at {
                    break;
                }
                e.push(x.clone());
                sent += 1;
            }
            if self.now >= first {
                if !connected {
                    e.push(Event::InputConnected { gen });
                    connected = true;
                }
                e.push(Event::InputFrame { gen });
                last = self.now;
            }
            self.instant(&e);
        }
        assert_eq!(sent, extra.len(), "setup: every extra event went in");
        last
    }

    /// Ticks with no event until gen `gen` is built (bounded by a stall, the capped back-off and a
    /// connect timeout from now); returns its build instant.
    fn idle_until_build(&mut self, gen: u64) -> Ms {
        let bound = self.now
            + self.th.input_stall_after_ms
            + self.th.input_backoff_cap_ms
            + self.th.input_connect_timeout_ms
            + 2 * self.step;
        loop {
            if let Some(&(at, _)) = self.builds.iter().find(|&&(_, g)| g == gen) {
                return at;
            }
            assert!(self.now <= bound, "gen {gen} never built: {:?}", self.builds);
            self.instant(&[]);
        }
    }
}

/// Gen 1 on air from the first tick, then the camera drops: returns gen 2's build instant and gen 1's
/// last frame.
fn on_air_then_dropped(r: &mut Run) -> (Ms, Ms) {
    let last1 = r.returns(1, 0, r.step, &[]);
    (r.idle_until_build(2), last1)
}

/// The camera drops and comes back twice; each return's first decoded frame comes `after.0` and
/// `after.1` after its build. Every abandon is the stall rule's, on the first tick at or after a
/// return's last frame plus `input_stall_after_ms` (the rule reads Running only, so the connect was
/// kept), each build is the next generation, and the programme is Live at the end of each return.
#[test]
fn a_first_frame_6_to_8_s_after_the_build_is_kept_on_each_return() {
    let th = t();
    let (mut kept, mut runs) = (0, 0);
    for (grid, step) in grids() {
        for (i, &a1) in FIRST_FRAME_AFTER.iter().enumerate() {
            let a2 = FIRST_FRAME_AFTER[FIRST_FRAME_AFTER.len() - 1 - i];
            let label = format!("{grid}, first frames {a1} then {a2} ms after the build");
            let mut r = Run::booted(&th, step);
            let (b2, last1) = on_air_then_dropped(&mut r);
            let last2 = r.returns(2, b2, a1, &[]);
            assert_eq!(r.s.programme_phase(), ProgrammePhase::Live, "{label}: return 1");
            let b3 = r.idle_until_build(3);
            r.returns(3, b3, a2, &[]);
            assert_eq!(r.s.programme_phase(), ProgrammePhase::Live, "{label}: return 2");
            assert_eq!(r.builds, vec![(0, 1), (b2, 2), (b3, 3)], "{label}");
            let stall = |last: Ms| first_tick(last + th.input_stall_after_ms, step);
            assert_eq!(
                r.abandons,
                vec![(stall(last1), 1), (stall(last2), 2)],
                "{label}: only the stall rule gave a generation up"
            );
            kept += 2;

            // Control: the same first return at F-002's 5 s is given up before its frame.
            let old = at_f002(&th);
            let mut c = Run::booted(&old, step);
            let (b2, _) = on_air_then_dropped(&mut c);
            c.returns(2, b2, a1, &[]);
            assert!(
                c.abandons
                    .contains(&(first_tick(b2 + F002_CONNECT_TIMEOUT_MS, step), 2)),
                "{label}: the control at 5 s kept it: {:?}",
                c.abandons
            );
            assert_ne!(c.s.programme_phase(), ProgrammePhase::Live, "{label}: control");
            runs += 1;
        }
    }
    eprintln!(
        "slow returns kept: {kept} over {runs} runs on {} tick grids; each run's control at F-002's 5 s gave it up",
        grids().len()
    );
    assert_eq!(kept, 2 * FIRST_FRAME_AFTER.len() * grids().len());
}

/// Web's phone-live nudge (Owed 108 fix (b)) discards a connect with no byte yet, once an outage. A
/// slow returning camera's bytes arrive before its first decoded frame (one SRT latency after the
/// connect), so a nudge after them keeps the connect: the nudge reads bytes, not decoded frames, and
/// needs no floor of its own. Held against the same nudge on a connect with no byte, which it
/// discards at once.
#[test]
fn a_nudge_keeps_a_slow_connect_that_already_carries_bytes() {
    let th = t();
    let (mut kept, mut discarded) = (0, 0);
    for (grid, step) in grids() {
        for &after in &FIRST_FRAME_AFTER {
            let label = format!("{grid}, first frame {after} ms after the build");
            for with_bytes in [true, false] {
                let mut r = Run::booted(&th, step);
                // The counter's baseline on the camera's first frame, as a heartbeat reply brings it.
                r.send(Event::PhoneLiveEdge { count: 1 });
                let (b2, _) = on_air_then_dropped(&mut r);
                let bytes = b2 + th.input_srt_latency_ms;
                let nudge = bytes + step;
                let mut extra = vec![(nudge, Event::PhoneLiveEdge { count: 2 })];
                if with_bytes {
                    extra.insert(0, (bytes, Event::InputMedia { gen: 2 }));
                }
                r.returns(2, b2, after, &extra);
                let gave_up_2 = r.abandons.iter().any(|&(_, g)| g == 2);
                if with_bytes {
                    assert!(!gave_up_2, "{label}: kept: {:?}", r.abandons);
                    assert_eq!(r.s.programme_phase(), ProgrammePhase::Live, "{label}");
                    assert_eq!(r.builds.len(), 2, "{label}: {:?}", r.builds);
                    kept += 1;
                } else {
                    let at = first_tick(nudge, step);
                    assert!(
                        r.abandons.contains(&(at, 2)),
                        "{label}: no byte, discarded at the nudge: {:?}",
                        r.abandons
                    );
                    discarded += 1;
                }
            }
        }
    }
    eprintln!("nudged slow connects kept: {kept}; with no byte, discarded: {discarded}");
    assert_eq!(kept, FIRST_FRAME_AFTER.len() * grids().len());
    assert_eq!(discarded, kept);
}

/// A pull that never decodes is still given up, on the first tick at or after its build plus
/// `input_connect_timeout_ms`, and no sooner: two dead connects in a row after the camera drops, each
/// once, each before the next build.
#[test]
fn a_dead_pull_is_still_given_up_at_the_connect_timeout() {
    let th = t();
    let mut checked = 0;
    for (grid, step) in grids() {
        let mut r = Run::booted(&th, step);
        let (b2, _) = on_air_then_dropped(&mut r);
        let b3 = r.idle_until_build(3);
        let b4 = r.idle_until_build(4);
        for (built, gen, next) in [(b2, 2, b3), (b3, 3, b4)] {
            let due = first_tick(built + th.input_connect_timeout_ms, step);
            let mine: Vec<_> = r.abandons.iter().filter(|&&(_, g)| g == gen).collect();
            assert_eq!(mine, vec![&(due, gen)], "{grid}: gen {gen} built at {built}");
            assert!(due <= next, "{grid}: gen {gen} given up after gen {}", gen + 1);
            checked += 1;
        }
    }
    eprintln!("dead connects given up at the timeout: {checked}");
    assert_eq!(checked, 2 * grids().len());
}
