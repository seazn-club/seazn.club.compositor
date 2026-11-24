//! C2 (S4): every generation the input link gives up carries a typed reason, the one of the rule or
//! the report that gave it up: the connect timeout, the stall rule, the phone-live nudge's discard,
//! Cloudflare's hold-end close (F-032), or the adapter's end of stream or error. A give-up is a move
//! from Connecting or Running into Backoff, and each one records exactly one `GiveUp` for the
//! generation it left; a Backoff the link only reschedules (a nudge, a hold-end close, a second loss)
//! records nothing. Every instant is derived from `Thresholds`. They live here, not inline, because
//! the domain's crate root forbids `clippy::print_stderr`, and a sweep must print the count it
//! checked.
use domain::config::Thresholds;
use domain::input_link::{BuildFacts, GiveUp, GiveUpReason, InputAction, InputLink, InputState, LossReason};
use domain::time::Ms;

fn t() -> Thresholds {
    Thresholds::default()
}

/// The facts of Cloudflare's hold-end close: the handshake completed, no byte (F-032).
const HELD: BuildFacts = BuildFacts {
    handshake_completed: true,
    carried_media: false,
};

/// Any other error's facts: a refused connect, with no handshake.
const REFUSED: BuildFacts = BuildFacts {
    handshake_completed: false,
    carried_media: false,
};

/// Build generation 1 at `at` and connect it: Running.
fn running(at: Ms, th: &Thresholds) -> InputLink {
    let mut l = InputLink::new();
    assert_eq!(l.on_tick(at, th), Some(InputAction::Rebuild { gen: 1 }));
    l.on_connected(at, 1);
    assert!(matches!(l.state(), InputState::Running { .. }), "setup");
    l
}

/// Ticks `l` every 10 ms from `from` to the first build; returns its instant (bounded).
fn next_build(l: &mut InputLink, from: Ms, th: &Thresholds) -> Ms {
    let limit = from + th.input_connect_timeout_ms + th.input_backoff_cap_ms + 10;
    let mut now = from;
    loop {
        if matches!(l.on_tick(now, th), Some(InputAction::Rebuild { .. })) {
            return now;
        }
        now += 10;
        assert!(now <= limit, "no build by {limit}: {:?}", l.state());
    }
}

/// Each path that gives a generation up, run to its give-up: the reason it must carry (written from
/// the rule's own name, never read off the link) and the link after it.
fn paths(th: &Thresholds) -> Vec<(&'static str, GiveUpReason, InputLink)> {
    let mut out = Vec::new();

    // The connect timeout: Connecting `input_connect_timeout_ms` with no first frame.
    let mut l = InputLink::new();
    l.on_tick(0, th);
    assert_eq!(
        l.on_tick(th.input_connect_timeout_ms, th),
        Some(InputAction::Abandon { gen: 1 })
    );
    out.push(("connect timeout", GiveUpReason::ConnectTimeout, l));

    // The stall rule: Running with no frame for `input_stall_after_ms`.
    let mut l = running(0, th);
    assert_eq!(
        l.check_stall(th.input_stall_after_ms, Some(0), th),
        Some(InputAction::Abandon { gen: 1 })
    );
    out.push(("stall rule", GiveUpReason::Stalled, l));

    // The adapter's reports, while Running and while Connecting.
    for (name, loss, want) in [
        ("eos", LossReason::Eos, GiveUpReason::Eos),
        ("error", LossReason::Error, GiveUpReason::Error),
        ("stall reported", LossReason::Stalled, GiveUpReason::Stalled),
    ] {
        let mut l = running(0, th);
        l.on_lost(1_000, 1, loss, th);
        out.push((name, want, l));
        let mut l = InputLink::new();
        l.on_tick(0, th);
        l.on_lost(1_000, 1, loss, th);
        out.push((name, want, l));
    }

    // An error that is not the hold-end close (no handshake), while Running and while Connecting.
    let mut l = running(0, th);
    l.on_error(1_000, 1, REFUSED, Some(900), th);
    out.push(("refused error, running", GiveUpReason::Error, l));
    let mut l = InputLink::new();
    l.on_tick(0, th);
    l.on_error(1_000, 1, REFUSED, None, th);
    out.push(("refused error, connecting", GiveUpReason::Error, l));

    // Cloudflare's hold-end close on a connect: the camera ran (armed), stalled, and a later build
    // is connecting when the close comes `input_hold_end_after_ms` after the last frame.
    let mut l = running(0, th);
    l.check_stall(th.input_stall_after_ms, Some(0), th);
    let mut now = th.input_stall_after_ms;
    let close = th.input_hold_end_after_ms;
    while !(now >= close && matches!(l.state(), InputState::Connecting { .. })) {
        now = next_build(&mut l, now, th);
        if now < close {
            // Let it time out, so the next build is a fresh connect.
            l.on_tick(now + th.input_connect_timeout_ms, th);
            now += th.input_connect_timeout_ms;
        }
    }
    let gen = l.generation();
    l.on_error(now, gen, HELD, Some(0), th);
    assert!(
        matches!(l.state(), InputState::Backoff { .. }),
        "setup: the close fired"
    );
    out.push(("hold-end close", GiveUpReason::HoldEnd, l));

    // The phone-live nudge's discard: a connect with no byte when web's counter rises.
    let mut l = running(0, th);
    l.on_lost(1_000, 1, LossReason::Error, th);
    let built = next_build(&mut l, 1_000, th);
    l.on_phone_live_edge(0);
    l.on_phone_live_edge(1);
    assert_eq!(
        l.check_nudge(built + 10, th),
        Some(InputAction::Abandon { gen: 2 })
    );
    out.push(("phone-live discard", GiveUpReason::PhoneLiveDiscard, l));
    out
}

#[test]
fn each_give_up_carries_the_reason_of_the_rule_or_report_that_made_it() {
    let th = t();
    let mut checked = 0;
    let mut reasons = Vec::new();
    for (name, want, l) in paths(&th) {
        assert!(
            matches!(l.state(), InputState::Backoff { .. }),
            "{name}: given up"
        );
        assert_eq!(
            l.last_give_up(),
            Some(GiveUp {
                gen: l.generation(),
                reason: want
            }),
            "{name}"
        );
        if !reasons.contains(&want) {
            reasons.push(want);
        }
        checked += 1;
    }
    eprintln!("give-up paths checked: {checked}, reasons seen: {reasons:?}");
    // Every reason is reached by some path.
    assert_eq!(reasons.len(), 6, "reasons: {reasons:?}");
    assert_eq!(checked, 12);
}

/// A Backoff the link only reschedules records nothing: the nudge's build due now, a hold-end close
/// between builds, and a second loss of the generation already lost.
#[test]
fn a_backoff_that_is_only_rescheduled_records_no_new_give_up() {
    let th = t();
    let mut checked = 0;
    // Each case: a link in Backoff after a give-up, and what reschedules it.
    let mut base = running(0, &th);
    base.check_stall(th.input_stall_after_ms, Some(0), &th);
    let was = base.last_give_up();
    assert_eq!(
        was,
        Some(GiveUp {
            gen: 1,
            reason: GiveUpReason::Stalled
        }),
        "setup"
    );
    let at = th.input_stall_after_ms + 10;

    let mut l = base.clone();
    l.on_phone_live_edge(0);
    l.on_phone_live_edge(1);
    assert_eq!(l.check_nudge(at, &th), None);
    assert_eq!(l.last_give_up(), was, "a nudge in Backoff");
    checked += 1;

    let mut l = base.clone();
    l.on_error(th.input_hold_end_after_ms, 1, HELD, Some(0), &th);
    assert!(matches!(l.state(), InputState::Backoff { .. }));
    assert_eq!(l.last_give_up(), was, "a hold-end close in Backoff");
    checked += 1;

    for loss in [LossReason::Eos, LossReason::Error, LossReason::Stalled] {
        let mut l = base.clone();
        l.on_lost(at, 1, loss, &th);
        assert_eq!(l.last_give_up(), was, "a second loss ({loss:?}) in Backoff");
        checked += 1;
    }

    // A stale generation's loss, and a nudge while Running, change nothing either.
    let mut l = running(0, &th);
    l.on_lost(10, 7, LossReason::Eos, &th);
    l.on_phone_live_edge(0);
    l.on_phone_live_edge(1);
    assert_eq!(l.check_nudge(20, &th), None);
    assert_eq!(l.last_give_up(), None, "nothing given up");
    checked += 1;
    eprintln!("reschedules checked: {checked}");
    assert_eq!(checked, 6);
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

/// The give-up record changes exactly when the link moves from Connecting or Running into Backoff,
/// and then names the generation it left: over seeded runs of every event the link takes.
#[test]
fn a_give_up_is_recorded_exactly_when_a_generation_is_left_for_backoff() {
    let th = t();
    let (mut steps, mut give_ups, mut reschedules) = (0usize, 0usize, 0usize);
    for seed in 1..=40u64 {
        let mut r = Lcg(seed);
        let mut l = InputLink::new();
        let mut now: Ms = 0;
        let mut last_frame: Option<Ms> = None;
        let mut counter = 0u64;
        for _ in 0..400 {
            now += 1 + r.next(1_500);
            let before = (l.state(), l.generation(), l.last_give_up());
            let gen = l.generation();
            match r.next(9) {
                0 | 1 => {
                    l.on_tick(now, &th);
                }
                2 => {
                    l.on_connected(now, gen);
                    l.on_media(gen);
                    last_frame = Some(now);
                }
                3 => l.on_lost(now, gen, LossReason::Eos, &th),
                4 => l.on_lost(now, gen, LossReason::Error, &th),
                5 => l.on_error(
                    now,
                    gen,
                    if r.next(2) == 0 { HELD } else { REFUSED },
                    last_frame,
                    &th,
                ),
                6 => {
                    counter += r.next(2);
                    l.on_phone_live_edge(counter);
                    l.check_nudge(now, &th);
                }
                7 => {
                    l.check_stall(now, last_frame, &th);
                }
                _ => {
                    // Long silences reach the stall rule, the timeout and the hold-end gate. One
                    // call a step, so each step makes at most one move.
                    now += th.input_hold_end_after_ms / 2;
                    l.check_stall(now, last_frame, &th);
                }
            }
            let left = !matches!(before.0, InputState::Backoff { .. })
                && matches!(l.state(), InputState::Backoff { .. });
            if left {
                give_ups += 1;
                let got = l.last_give_up().expect("a give-up is recorded");
                assert_eq!(got.gen, before.1, "seed {seed}: names the generation it left");
                assert_ne!(Some(got), before.2, "seed {seed}: a new record");
            } else {
                if matches!(before.0, InputState::Backoff { .. })
                    && matches!(l.state(), InputState::Backoff { .. })
                    && l.state() != before.0
                {
                    reschedules += 1;
                }
                assert_eq!(l.last_give_up(), before.2, "seed {seed}: no give-up, no record");
            }
            steps += 1;
        }
    }
    eprintln!("give-up sweep: {steps} steps, {give_ups} give-ups, {reschedules} reschedules");
    assert!(steps > 0 && give_ups > 0 && reschedules > 0);
}

/// Through the supervisor: the beat after a give-up carries it (`HealthReport::input_give_up`), the
/// connect timeout's on the next cadence beat, and nothing before the first give-up.
#[test]
fn the_next_beat_carries_the_give_up() {
    use domain::lifecycle::Lifecycle;
    use domain::supervisor::{Effect, Event, Supervisor};
    let th = t();
    let s = Supervisor::new(th.clone()).decide(Event::Tick, 0).next;
    let mut s = s
        .decide(
            Event::BootstrapOk {
                heartbeat_interval_ms: th.heartbeat_interval_ms,
                deadline_at: 10_000_000,
            },
            0,
        )
        .next;
    assert_eq!(s.lifecycle(), Lifecycle::Running, "setup");
    let (mut gave_up_at, mut carried, mut before) = (None, None, 0);
    let mut now = 0;
    // Nothing connects: generation 1 times out, and a later beat names it.
    while carried.is_none() {
        let d = s.decide(Event::Tick, now);
        if d.effects.contains(&Effect::AbandonInput { gen: 1 }) {
            gave_up_at = Some(now);
        }
        for e in &d.effects {
            if let Effect::SendHeartbeat(h) = e {
                match gave_up_at {
                    None => {
                        assert_eq!(h.input_give_up, None, "nothing given up at {now}");
                        before += 1;
                    }
                    Some(_) => carried = h.input_give_up,
                }
            }
        }
        s = d.next;
        now += 10;
        assert!(
            now < 3 * th.heartbeat_interval_ms + th.input_connect_timeout_ms,
            "no beat"
        );
    }
    assert_eq!(
        gave_up_at,
        Some(th.input_connect_timeout_ms),
        "the timeout's own instant"
    );
    assert_eq!(
        carried,
        Some(GiveUp {
            gen: 1,
            reason: GiveUpReason::ConnectTimeout
        })
    );
    assert_eq!(s.input_give_up(), carried, "the driver reads the same");
    eprintln!("beats before the give-up: {before}");
    assert!(before > 0);
}
