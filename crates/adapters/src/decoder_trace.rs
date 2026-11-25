//! The video decoder's side of a clamped run (C4 fix round 3). GstVideoDecoder never lets a stamp
//! go back: a frame stamped below the last it put out leaves with that last stamp ("decreasing
//! timestamp"), so after camera-flap's outlier every frame for a pause repeats one stamp, and the
//! playout places them a step apart (`playout::VideoStamps`). Twice on PR #113's head 028e8b3 (chaos
//! run 45611982713, returns 7 and 12) a run placed one frame more than the stamps advanced, and the
//! playout's own notes cannot say which: it sees only the stamps the decoder put out. This pairs each
//! frame the decoder puts out with the one it took in, so the log says, frame by frame, the stamp
//! taken, the stamp given, and how long the decoder held it; the playout's `clamp_frame` notes give
//! each one's placement and the audio clock, counted the same way (`n`).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use domain::config::PLAYOUT_CAP_MS;

/// The frames in flight it keeps (a decoder holds a handful: avdec_h264's frame threads); past it
/// the oldest is forgotten.
const IN_FLIGHT_MAX: usize = 64;
/// The lines one clamped run writes at most (a pause of 8.5 s at 60 fps), and the unpaired frames
/// written at most for good: past them a broken pairing would log every frame.
const LINES_MAX: u32 = 512;

/// The pairing and the open run. `on_input` at the decoder's sink, `on_output` at its source.
#[derive(Debug, Default)]
pub struct DecoderTrace {
    inputs: VecDeque<(Option<u64>, Instant)>,
    /// The frames of the open clamped run (0: none open).
    run: u32,
    unpaired: u32,
}

impl DecoderTrace {
    /// A frame into the decoder, stamped `pts` (ns), at `at`.
    pub fn on_input(&mut self, pts: Option<u64>, at: Instant) {
        if self.inputs.len() == IN_FLIGHT_MAX {
            self.inputs.pop_front();
        }
        self.inputs.push_back((pts, at));
    }

    /// A frame out of the decoder stamped `pts` at `at`, paired with the frame in flight of its own
    /// stamp (those before it the decoder dropped), else the oldest (a stamp the decoder changed).
    /// Returns the line to log: a clamped frame (put out above the stamp it took), the frame that
    /// ends a clamped run, a frame held past the playout's cap, or one it cannot pair.
    pub fn on_output(&mut self, pts: Option<u64>, at: Instant) -> Option<String> {
        let dropped = pts
            .and_then(|p| self.inputs.iter().position(|(q, _)| *q == Some(p)))
            .unwrap_or(0);
        self.inputs.drain(..dropped);
        let input = self.inputs.pop_front();
        let held = input.map(|(_, t)| at.saturating_duration_since(t));
        let verdict = match (pts, input.and_then(|(q, _)| q)) {
            (Some(o), Some(i)) if o > i => {
                self.run += 1;
                "clamped"
            }
            (Some(o), Some(i)) if o == i && self.run > 0 => "end",
            (Some(o), Some(i)) if o == i => "held",
            _ => "unpaired",
        };
        let n = self.run;
        let write = match verdict {
            "clamped" => n <= LINES_MAX,
            "end" => true,
            "held" => held.is_some_and(|h| h >= Duration::from_millis(PLAYOUT_CAP_MS)),
            _ => {
                self.unpaired += 1;
                self.unpaired <= LINES_MAX
            }
        };
        if verdict == "end" {
            self.run = 0;
        }
        let ms = |v: Option<u64>| v.map_or_else(|| "-".into(), |n| format!("{:.1}", n as f64 / 1e6));
        write.then(|| {
            format!(
                "video decoder {verdict} n={n} out_ms={} in_ms={} held_ms={} dropped={dropped}",
                ms(pts),
                ms(input.and_then(|(q, _)| q)),
                held.map_or_else(|| "-".into(), |h| h.as_millis().to_string()),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    /// One camera-flap return through a clamping decoder: two frames held over the pause, the new
    /// frame stamped a pause on, the frames after it clamped to it, then their own stamps again. Each
    /// frame the decoder changed is logged with the stamp it took and the run's count, the run's end
    /// once, the held frames with their hold, and a steady frame not at all.
    #[test]
    fn a_clamped_run_is_logged_frame_by_frame_with_the_stamps_taken() {
        let t0 = Instant::now();
        let at = |ms: u64| t0 + Duration::from_millis(ms);
        let mut d = DecoderTrace::default();
        let mut lines = Vec::new();
        // Steady: in and out at once.
        for k in 0..3 {
            d.on_input(Some(k * 33 * MS), at(k * 33));
            lines.extend(d.on_output(Some(k * 33 * MS), at(k * 33 + 1)));
        }
        assert!(lines.is_empty(), "steady frames: {lines:?}");
        // Two frames in before the pause, out at the return 4.9 s later.
        d.on_input(Some(99 * MS), at(99));
        d.on_input(Some(132 * MS), at(132));
        let back = 132 + 4_900;
        lines.extend(d.on_output(Some(99 * MS), at(back)));
        lines.extend(d.on_output(Some(132 * MS), at(back)));
        // The outlier, a pause on, then three frames clamped to it, then the end.
        let outlier = (165 + 4_900) * MS;
        d.on_input(Some(outlier), at(back));
        lines.extend(d.on_output(Some(outlier), at(back + 1)));
        for k in 1..=3 {
            let own = (165 + k * 33) * MS;
            d.on_input(Some(own), at(back + k * 33));
            lines.extend(d.on_output(Some(outlier), at(back + k * 33 + 1)));
        }
        d.on_input(Some(outlier + 30 * MS), at(back + 200));
        lines.extend(d.on_output(Some(outlier + 30 * MS), at(back + 201)));
        let kinds: Vec<&str> = lines.iter().map(|l| l.split(' ').nth(2).unwrap_or("")).collect();
        assert_eq!(
            kinds,
            ["held", "held", "clamped", "clamped", "clamped", "end"],
            "{lines:#?}"
        );
        assert!(lines[0].contains("held_ms=4933"), "{}", lines[0]);
        assert!(lines[2].contains("n=1 out_ms=5065.0 in_ms=198.0"), "{}", lines[2]);
        assert!(lines[4].contains("n=3 out_ms=5065.0 in_ms=264.0"), "{}", lines[4]);
        assert!(lines[5].contains("end n=3 "), "{}", lines[5]);
        eprintln!("lines checked: {}", lines.len());
    }

    /// A frame the decoder drops (a corrupt one, `output-corrupt=false`) never comes out: the next
    /// output pairs with its own stamp past it, counted, so the pairing does not slip a frame and read
    /// every later frame as clamped.
    #[test]
    fn a_dropped_frame_does_not_slip_the_pairing() {
        let t0 = Instant::now();
        let mut d = DecoderTrace::default();
        let mut lines = Vec::new();
        for k in 0..6u64 {
            d.on_input(Some(k * 33 * MS), t0);
            if k != 2 {
                lines.extend(d.on_output(Some(k * 33 * MS), t0));
            }
        }
        assert!(lines.is_empty(), "{lines:?}");
        // With nothing in flight an output cannot be paired: logged as such.
        let l = d.on_output(Some(999 * MS), t0).expect("an unpaired line");
        assert!(
            l.starts_with("video decoder unpaired n=0 out_ms=999.0 in_ms=-"),
            "{l}"
        );
    }
}
