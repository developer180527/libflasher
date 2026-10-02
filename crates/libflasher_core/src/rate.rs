//! Speed and time remaining, computed the same way for every front end.
//!
//! Speed is measured over a sliding window rather than since the start, so a
//! drive whose write cache fills (fast for a gigabyte, then slow) shows its
//! real current speed instead of an average that promises minutes it will not
//! deliver. No estimate is offered until there is enough data to mean
//! something.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const WINDOW: Duration = Duration::from_secs(8);
/// Less than this much history and any speed would be noise.
const MIN_HISTORY: Duration = Duration::from_secs(2);

#[derive(Debug, Clone)]
/// Measures bytes per second over a sliding window.
pub struct RateMeter {
    samples: VecDeque<(Instant, u64)>,
}

impl Default for RateMeter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateMeter {
    /// A meter with no samples.
    pub fn new() -> Self {
        Self {
            samples: VecDeque::new(),
        }
    }

    /// Record that `done` bytes are complete as of now.
    pub fn record(&mut self, done: u64) {
        self.record_at(Instant::now(), done);
    }

    /// [`RateMeter::record`] at a given time, for tests and replays.
    pub fn record_at(&mut self, at: Instant, done: u64) {
        // A new phase starting from zero (writing → verifying) restarts the meter.
        if self.samples.back().is_some_and(|&(_, d)| done < d) {
            self.samples.clear();
        }
        self.samples.push_back((at, done));
        while self.samples.len() > 2 && at.duration_since(self.samples[1].0) >= WINDOW {
            self.samples.pop_front();
        }
    }

    /// Bytes per second over the recent window, once there is enough history.
    pub fn rate(&self) -> Option<f64> {
        let (&(t0, d0), &(t1, d1)) = (self.samples.front()?, self.samples.back()?);
        let span = t1.duration_since(t0);
        (span >= MIN_HISTORY && d1 > d0).then(|| (d1 - d0) as f64 / span.as_secs_f64())
    }

    /// Time left to reach `total`, at the current rate.
    pub fn remaining(&self, total: u64) -> Option<Duration> {
        let done = self.samples.back()?.1;
        let rate = self.rate()?;
        Some(Duration::from_secs_f64(
            total.saturating_sub(done) as f64 / rate,
        ))
    }
}

/// `45 s`, `4 min`, `1 h 05 min`: rounded the way a person would say it.
pub fn human_duration(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..=59 => format!("{} s", s.max(1)),
        60..=3599 => format!("{} min", (s + 30) / 60),
        _ => format!("{} h {:02} min", s / 3600, (s % 3600 + 30) / 60),
    }
}

/// `18.3 MB/s`.
pub fn human_rate(bytes_per_sec: f64) -> String {
    format!("{}/s", crate::platform::human_size(bytes_per_sec as u64))
}

/// One line describing a flash in progress, worded the same in every front
/// end: what is happening, how far, how fast, how long — and, when a figure is
/// an estimate, saying so.
///
/// Feed it every [`Progress`](crate::Progress) and read [`StatusLine::text`]
/// whenever you redraw; the text includes timers, so it changes even while no
/// progress arrives.
#[derive(Debug, Clone)]
pub struct StatusLine {
    meter: RateMeter,
    last: Option<crate::Progress>,
    phase_started: Instant,
}

impl Default for StatusLine {
    fn default() -> Self {
        Self::new()
    }
}

/// `fraction` scaled to an integer the meter can measure.
const FRACTION_SCALE: f64 = 1e9;

impl StatusLine {
    /// A status line with nothing reported yet ("Preparing…").
    pub fn new() -> Self {
        Self {
            meter: RateMeter::new(),
            last: None,
            phase_started: Instant::now(),
        }
    }

    /// Take in the latest progress report.
    pub fn update(&mut self, p: crate::Progress) {
        use crate::Progress::*;
        let same_phase = matches!(
            (&self.last, &p),
            (Some(Writing { .. }), Writing { .. })
                | (Some(Syncing), Syncing)
                | (Some(Verifying { .. }), Verifying { .. })
                | (Some(Checking { .. }), Checking { .. })
        );
        if !same_phase {
            self.phase_started = Instant::now();
            self.meter = RateMeter::new();
        }
        match p {
            // With no exact total, measure progress through the file instead
            // of bytes written, so the time left is at least consistent.
            Writing {
                written,
                total: Some(_),
                ..
            } => self.meter.record(written),
            Writing {
                fraction,
                total: None,
                ..
            } => self.meter.record((fraction as f64 * FRACTION_SCALE) as u64),
            Verifying { verified, .. } => self.meter.record(verified),
            Checking { done, .. } => self.meter.record(done),
            Syncing => {}
        }
        self.last = Some(p);
    }

    /// 0..1 for a progress bar, or `None` while nothing measurable is
    /// happening (flushing), which should be drawn as indeterminate.
    pub fn fraction(&self) -> Option<f32> {
        match self.last? {
            crate::Progress::Writing { fraction, .. } => Some(fraction),
            crate::Progress::Verifying { verified, total } => {
                Some((verified as f64 / total.max(1) as f64) as f32)
            }
            crate::Progress::Checking { done, total } => {
                Some((done as f64 / total.max(1) as f64) as f32)
            }
            crate::Progress::Syncing => None,
        }
    }

    /// The line to show now. Includes timers, so call it on every redraw.
    pub fn text(&self) -> String {
        use crate::platform::human_size as size;
        let speed = |bytes_per_sec: Option<f64>| {
            bytes_per_sec
                .map(|r| format!(" · {}", human_rate(r)))
                .unwrap_or_default()
        };
        match self.last {
            None => "Preparing…".into(),
            Some(crate::Progress::Writing {
                written,
                total: Some(total),
                ..
            }) => format!(
                "Writing {} of {}{}{}",
                size(written),
                size(total),
                speed(self.meter.rate()),
                self.left(self.meter.remaining(total), false)
            ),
            Some(crate::Progress::Writing {
                written,
                total: None,
                fraction,
            }) => {
                // The meter runs in fraction units here: convert its rate back
                // to bytes through the bytes written so far.
                let bytes_rate = self
                    .meter
                    .rate()
                    .filter(|_| fraction > 0.0)
                    .map(|r| r / FRACTION_SCALE * written as f64 / fraction as f64);
                format!(
                    "Writing {} (size unknown until decompressed){}{}",
                    size(written),
                    speed(bytes_rate),
                    self.left(self.meter.remaining(FRACTION_SCALE as u64), true)
                )
            }
            Some(crate::Progress::Syncing) => {
                format!(
                    "Flushing to the drive… {}",
                    human_duration(self.phase_started.elapsed())
                )
            }
            Some(crate::Progress::Checking { done, total }) => format!(
                "Checking the image's SHA-256 · {} of {}{}{}",
                size(done),
                size(total),
                speed(self.meter.rate()),
                self.left(self.meter.remaining(total), false)
            ),
            Some(crate::Progress::Verifying { verified, total }) => format!(
                "Verifying {} of {}{}{}",
                size(verified),
                size(total),
                speed(self.meter.rate()),
                self.left(self.meter.remaining(total), false)
            ),
        }
    }

    fn left(&self, d: Option<Duration>, estimated: bool) -> String {
        match d {
            Some(d) if estimated => format!(" · roughly {} left (estimated)", human_duration(d)),
            Some(d) => format!(" · about {} left", human_duration(d)),
            None => String::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_estimate_without_history() {
        let mut m = RateMeter::new();
        let t = Instant::now();
        m.record_at(t, 0);
        m.record_at(t + Duration::from_millis(500), 10 << 20);
        assert_eq!(m.rate(), None, "half a second is not a measurement");
    }

    #[test]
    fn follows_the_current_speed_not_the_average() {
        let mut m = RateMeter::new();
        let t = Instant::now();
        // 100 MB/s for 10 s into the drive's cache, then 10 MB/s for 20 s.
        let mut done = 0u64;
        for s in 0..=30u64 {
            m.record_at(t + Duration::from_secs(s), done);
            done += if s < 10 { 100_000_000 } else { 10_000_000 };
        }
        let r = m.rate().unwrap();
        assert!(
            (9e6..11e6).contains(&r),
            "rate {r} should be the current ~10 MB/s"
        );
        let left = m.remaining(done + 100_000_000).unwrap().as_secs();
        assert!((9..=12).contains(&left), "{left}");
    }

    #[test]
    fn a_new_phase_restarts_it() {
        let mut m = RateMeter::new();
        let t = Instant::now();
        m.record_at(t, 0);
        m.record_at(t + Duration::from_secs(5), 500);
        m.record_at(t + Duration::from_secs(6), 10);
        assert_eq!(m.rate(), None);
    }

    #[test]
    fn durations_read_naturally() {
        assert_eq!(human_duration(Duration::from_secs(0)), "1 s");
        assert_eq!(human_duration(Duration::from_secs(45)), "45 s");
        assert_eq!(human_duration(Duration::from_secs(250)), "4 min");
        assert_eq!(human_duration(Duration::from_secs(3900)), "1 h 05 min");
    }

    #[test]
    fn status_line_says_what_is_known_and_what_is_estimated() {
        use crate::Progress::*;
        let mut s = StatusLine::new();
        assert_eq!(s.text(), "Preparing…");

        s.update(Writing {
            written: 1_000_000_000,
            total: Some(6_700_000_000),
            fraction: 0.15,
        });
        assert!(
            s.text().starts_with("Writing 1.0 GB of 6.7 GB"),
            "{}",
            s.text()
        );
        assert!(!s.text().contains("estimated"));
        assert_eq!(s.fraction(), Some(0.15));

        let mut s = StatusLine::new();
        s.update(Writing {
            written: 2_000_000_000,
            total: None,
            fraction: 0.3,
        });
        assert!(
            s.text().contains("size unknown until decompressed"),
            "{}",
            s.text()
        );

        s.update(Syncing);
        assert!(
            s.text().starts_with("Flushing to the drive…"),
            "{}",
            s.text()
        );
        assert_eq!(s.fraction(), None, "a flush has no measurable progress");

        s.update(Verifying {
            verified: 500_000_000,
            total: 2_000_000_000,
        });
        assert!(
            s.text().starts_with("Verifying 500.0 MB of 2.0 GB"),
            "{}",
            s.text()
        );
        assert_eq!(s.fraction(), Some(0.25));
    }
}
