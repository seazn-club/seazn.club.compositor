//! C3 (S2), through the REAL driver and supervisor, every port faked: the take-back reaches the input
//! port (`take_back`) after its log line, with no rebuild; a taken-back generation given up again is
//! logged again; and the camera's source bytes are read from the input port each tick and lengthen
//! the stall bound. Every duration comes from `Thresholds::default()`, never from the driver.
use app::driver::Driver;
use app::ports::{Inbound, InputBytes, Measured};
use domain::config::{OutputProfile, Thresholds};
use domain::supervisor::Event;
use testkit::fakes::{ports, session_720p, BytesCell, Call, FakeInputBytes, Log};

const FAR: u64 = 100_000_000;
const SESSION_AT: u64 = 10;

fn th() -> Thresholds {
    Thresholds::default()
}

/// A booted driver on a manual clock; the camera's port is `FakeInputBytes` over `bytes`.
struct Rig {
    d: Driver,
    log: Log,
    bytes: BytesCell,
    now: u64,
}

impl Rig {
    fn new() -> Self {
        let log = Log::default();
        let bytes = BytesCell::default();
        let mut p = ports(&log, Measured::default());
        p.input = Box::new(FakeInputBytes {
            log: log.clone(),
            bytes: bytes.clone(),
        });
        let mut d = Driver::new(th(), p, FAR, 0);
        assert_eq!(d.tick_due(0), None);
        assert_eq!(d.step(Inbound::Session(session_720p()), SESSION_AT), None);
        Self {
            d,
            log,
            bytes,
            now: SESSION_AT,
        }
    }

    /// Runs every tick before `until`; `before_tick(at, rig)` delivers the adapter's reports of that
    /// tick's instant first.
    fn run(&mut self, until: u64, mut before_tick: impl FnMut(u64, &mut Driver, &BytesCell)) {
        loop {
            let at = self.d.next_tick_at();
            if at >= until {
                self.now = until;
                return;
            }
            before_tick(at, &mut self.d, &self.bytes);
            assert_eq!(self.d.tick_due(at), None, "no exit at {at}");
            self.now = at;
        }
    }

    /// The instant of the tick after which `pred`'s call first appears (`marks`: each tick's instant
    /// and the log's length after it).
    fn when(&self, marks: &[(u64, usize)], pred: impl Fn(&Call) -> bool) -> Option<u64> {
        let calls = self.log.calls();
        let i = calls.iter().position(pred)?;
        marks.iter().find(|&&(_, len)| len > i).map(|&(at, _)| at)
    }
}

fn built_1(c: &Call) -> bool {
    matches!(c, Call::InputRebuild(1, _))
}

fn send(d: &mut Driver, e: Event, at: u64) {
    assert_eq!(d.step(Inbound::Event(e), at), None);
}

fn lines_starting(log: &Log, prefix: &str) -> Vec<String> {
    log.lines()
        .into_iter()
        .filter(|l| l.starts_with(prefix))
        .collect()
}

fn step_ms() -> u64 {
    OutputProfile::P720.frame_period_ms().round() as u64
}

/// The camera through the driver: gen 1 connects at the session, frames until `stop`; the abandon is
/// answered `again_after` later by the adapter's `InputAgain` (then frames again until `stop2`, and
/// silence after). Returns the rig and the instant of each tick with the log's length after it.
fn walk(stop: u64, again_after: Option<u64>, stop2: u64, until: u64) -> (Rig, Vec<(u64, usize)>) {
    let mut r = Rig::new();
    let log = r.log.clone();
    let mut marks = Vec::new();
    let mut connected = false;
    let mut again_at: Option<u64> = None;
    let mut answered = false;
    let mut seen_abandons = 0;
    loop {
        let at = r.d.next_tick_at();
        if at >= until {
            break;
        }
        // Gen 1 is built by the first tick: its first frame comes on the next.
        if !connected && log.count(built_1) > 0 {
            connected = true;
            send(&mut r.d, Event::InputConnected { gen: 1 }, at);
        }
        let abandons = log.count(|c| *c == Call::InputAbandon(1));
        if abandons > seen_abandons {
            // The abandon came on the previous tick, `r.now`.
            seen_abandons = abandons;
            again_at = again_after.map(|a| r.now + a);
            answered = false;
        }
        let back = again_at.is_some_and(|a| at >= a);
        if back && !answered {
            answered = true;
            send(&mut r.d, Event::InputAgain { gen: 1 }, at);
        }
        if at < stop || (back && at < stop2) {
            send(&mut r.d, Event::InputFrame { gen: 1 }, at);
        }
        assert_eq!(r.d.tick_due(at), None);
        marks.push((at, log.len()));
        r.now = at;
    }
    (r, marks)
}

// Abandon → the adapter reports a frame at the shut gate 300 ms later → the driver logs the take-back
// and calls `take_back(1)`, in that order, and no rebuild follows through the old rebuild instant.
#[test]
fn a_take_back_reaches_the_input_port_after_its_line_and_no_rebuild_follows() {
    let t = th();
    let stop = SESSION_AT + 2_000;
    let end = stop + t.input_stall_after_ms + t.input_backoff_base_ms + 2_000;
    let (r, marks) = walk(stop, Some(300), end, end);
    let abandon = r
        .when(&marks, |c| *c == Call::InputAbandon(1))
        .expect("abandoned");
    let back = r
        .when(&marks, |c| *c == Call::InputTakeBack(1))
        .expect("taken back");
    let step = step_ms();
    assert!(
        back >= abandon + 300 && back < abandon + 300 + step,
        "{abandon} {back}"
    );
    assert!(back < abandon + t.input_backoff_base_ms, "inside the back-off");
    let calls = r.log.calls();
    let li = calls
        .iter()
        .position(|c| *c == Call::Log("input take back gen=1".to_owned()))
        .expect("the line");
    let ci = calls
        .iter()
        .position(|c| *c == Call::InputTakeBack(1))
        .expect("the call");
    assert!(li < ci, "the line before the port call");
    assert_eq!(
        r.log.count(|c| matches!(c, Call::InputRebuild(g, _) if *g == 2)),
        0,
        "no rebuild after a take-back"
    );
    assert_eq!(r.log.count(|c| *c == Call::InputTakeBack(1)), 1);
    assert_eq!(
        lines_starting(&r.log, "input give up"),
        vec!["input give up gen=1 reason=stalled".to_owned()]
    );
}

// The same generation given up twice (a take-back between): two `input give up gen=1` lines, since the
// driver reads the link's give-up count, not the record (equal both times).
#[test]
fn a_taken_back_generation_given_up_again_is_logged_again() {
    let t = th();
    let stop = SESSION_AT + 2_000;
    let stop2 = stop + t.input_stall_after_ms + 300 + 1_500;
    let end = stop2 + t.input_stall_after_ms + 500;
    let (r, _) = walk(stop, Some(300), stop2, end);
    // The second abandon is answered too (`walk` answers every abandon); the lines are what count.
    assert_eq!(
        r.log.count(|c| *c == Call::InputAbandon(1)),
        2,
        "{:?}",
        r.log.lines()
    );
    assert_eq!(
        lines_starting(&r.log, "input give up"),
        vec![
            "input give up gen=1 reason=stalled".to_owned(),
            "input give up gen=1 reason=stalled".to_owned()
        ]
    );
}

/// Frames to `stop`; the camera port's byte count rises each tick before `bytes_until`, and stays
/// flat after it. Returns the rig and the last frame's instant.
fn bytes_rig(stop: u64, bytes_until: u64, until: u64) -> (Rig, u64) {
    let mut r = Rig::new();
    let log = r.log.clone();
    let mut connected = false;
    let mut total = 0;
    let mut last = 0;
    r.run(until, |at, d, cell| {
        if !connected && log.count(built_1) > 0 {
            connected = true;
            send(d, Event::InputConnected { gen: 1 }, at);
        }
        if at < stop {
            last = at;
            send(d, Event::InputFrame { gen: 1 }, at);
        }
        if at < bytes_until {
            total += 1_316;
        }
        cell.set(Some(InputBytes { gen: 1, total }));
    });
    (r, last)
}

fn first_abandon_tick(r: &Rig) -> Option<usize> {
    r.log.calls().iter().position(|c| *c == Call::InputAbandon(1))
}

// The driver reads the port's byte count on each tick and hands it to the domain: bytes rising keep
// the stalled camera Running past the plain bound, to the decode wait; a flat count (bytes stopped)
// leaves the plain bound. The instants are read from the ticks, both against `Thresholds`.
#[test]
fn the_driver_reads_the_camera_bytes_each_tick_and_they_lengthen_the_stall() {
    let t = th();
    let stop = SESSION_AT + 2_000;
    let step = step_ms();
    let until_plain = stop + t.input_stall_after_ms + 3 * step;
    // Bytes rising through: no abandon by the plain bound.
    let (r, _) = bytes_rig(stop, FAR, until_plain);
    assert_eq!(
        first_abandon_tick(&r),
        None,
        "bytes rising: no abandon at the plain bound"
    );
    // ... and one by the decode wait.
    let (r, _) = bytes_rig(stop, FAR, stop + t.input_decode_wait_ms + 3 * step);
    assert!(first_abandon_tick(&r).is_some(), "the decode wait's bound");
    // Bytes flat from the last frame on: the plain bound.
    let (r, _) = bytes_rig(stop, stop, until_plain);
    assert!(first_abandon_tick(&r).is_some(), "bytes stopped: the plain bound");
}
