//! Two-finger horizontal swipe → history / tab navigation.
//!
//! Wayland does not deliver "swipe left" as an event: a touchpad reports a
//! continuous stream of horizontal axis deltas (`WindowEvent::MouseWheel` with a
//! `PixelDelta`). Navigating on the first delta would fire dozens of times per
//! swipe, so deltas accumulate until they cross a threshold and the accumulator
//! then stays inert for a cooldown — one swipe, one action. A pause longer than
//! the idle timeout starts a fresh gesture, so two deliberate swipes are two
//! actions rather than one long one.

use std::time::{Duration, Instant};

/// What a completed horizontal swipe means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Swipe {
    /// Fingers moved left: go back in history / to the previous tab.
    Back,
    /// Fingers moved right: go forward / to the next tab.
    Forward,
}

/// Accumulates horizontal touchpad deltas into discrete swipes.
#[derive(Debug)]
pub struct SwipeAccumulator {
    threshold: f64,
    cooldown: Duration,
    idle_timeout: Duration,
    accum: f64,
    last: Option<Instant>,
    cooldown_until: Option<Instant>,
}

impl SwipeAccumulator {
    /// `threshold` is in device pixels: how far two fingers travel before the
    /// gesture counts. 60 px is about one deliberate flick.
    pub fn new(threshold: f64) -> Self {
        Self {
            threshold: threshold.max(1.0),
            cooldown: Duration::from_millis(400),
            idle_timeout: Duration::from_millis(250),
            accum: 0.0,
            last: None,
            cooldown_until: None,
        }
    }

    /// Forget any partial gesture — call when the modifier chord is released, so
    /// a half-swipe cannot combine with a later one.
    pub fn reset(&mut self) {
        self.accum = 0.0;
        self.last = None;
    }

    /// Feed one horizontal delta. Returns `Some` at most once per swipe.
    ///
    /// winit negates the Wayland axis value, so a leftward swipe (fingers left)
    /// arrives negative; the mapping lives here and nowhere else.
    pub fn push(&mut self, dx: f64, now: Instant) -> Option<Swipe> {
        if let Some(until) = self.cooldown_until {
            if now < until {
                return None;
            }
            self.cooldown_until = None;
        }

        if let Some(last) = self.last {
            if now.duration_since(last) > self.idle_timeout {
                self.accum = 0.0;
            }
        }
        self.last = Some(now);
        self.accum += dx;

        let crossed = if self.accum <= -self.threshold {
            Some(Swipe::Back)
        } else if self.accum >= self.threshold {
            Some(Swipe::Forward)
        } else {
            None
        };
        if crossed.is_some() {
            self.accum = 0.0;
            self.cooldown_until = Some(now + self.cooldown);
        }
        crossed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(ms: u64) -> Instant {
        // A fixed base keeps the arithmetic deterministic without a clock.
        Instant::now() + Duration::from_millis(ms)
    }

    #[test]
    fn a_short_flick_accumulates_until_it_crosses_the_threshold() {
        let mut s = SwipeAccumulator::new(60.0);
        let t0 = at(0);
        assert_eq!(s.push(-20.0, t0), None);
        assert_eq!(s.push(-25.0, t0 + Duration::from_millis(10)), None);
        assert_eq!(
            s.push(-20.0, t0 + Duration::from_millis(20)),
            Some(Swipe::Back),
            "leftward travel past the threshold is one Back"
        );
    }

    #[test]
    fn one_swipe_fires_exactly_once() {
        let mut s = SwipeAccumulator::new(60.0);
        let t0 = at(0);
        assert_eq!(s.push(-80.0, t0), Some(Swipe::Back));
        // The rest of the same physical swipe keeps delivering deltas.
        assert_eq!(s.push(-80.0, t0 + Duration::from_millis(5)), None);
        assert_eq!(s.push(-80.0, t0 + Duration::from_millis(10)), None);
        // ...and still nothing once the cooldown has expired, because the
        // accumulated travel was reset when the gesture fired.
        assert_eq!(s.push(-10.0, t0 + Duration::from_millis(500)), None);
    }

    #[test]
    fn rightward_swipe_is_forward() {
        let mut s = SwipeAccumulator::new(60.0);
        assert_eq!(s.push(70.0, at(0)), Some(Swipe::Forward));
    }

    #[test]
    fn opposite_motion_cancels_instead_of_firing() {
        let mut s = SwipeAccumulator::new(60.0);
        let t0 = at(0);
        assert_eq!(s.push(-40.0, t0), None);
        assert_eq!(s.push(30.0, t0 + Duration::from_millis(10)), None);
        assert_eq!(s.push(30.0, t0 + Duration::from_millis(20)), None);
    }

    #[test]
    fn a_pause_starts_a_new_gesture() {
        let mut s = SwipeAccumulator::new(60.0);
        let t0 = at(0);
        assert_eq!(s.push(-40.0, t0), None);
        // More than the idle timeout later, this is a new gesture: 40 + 30 is
        // not 70 of travel, so it must not fire.
        assert_eq!(s.push(-30.0, t0 + Duration::from_millis(400)), None);
        assert_eq!(s.push(-40.0, t0 + Duration::from_millis(420)), Some(Swipe::Back));
    }

    #[test]
    fn reset_drops_a_partial_gesture() {
        let mut s = SwipeAccumulator::new(60.0);
        let t0 = at(0);
        assert_eq!(s.push(-40.0, t0), None);
        s.reset();
        assert_eq!(s.push(-40.0, t0 + Duration::from_millis(10)), None);
    }
}
