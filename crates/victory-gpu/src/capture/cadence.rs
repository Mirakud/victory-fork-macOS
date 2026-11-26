//! Cadence of the PNG timeline, measured from the first presented world frame.

pub(super) struct WorldCadence {
    pub(super) interval: u64,
    /// An explicit interval with WORLD=only requests the timeline alone.
    pub(super) png_only: bool,
}

impl WorldCadence {
    pub(super) fn new(only: bool, value: Option<&str>) -> Self {
        let interval = value
            .filter(|_| only)
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|n| (1..=64).contains(n));
        Self {
            interval: interval.unwrap_or(8),
            png_only: interval.is_some(),
        }
    }

    pub(super) fn scheduled(&self, frame: u64, first: u64) -> bool {
        first != u64::MAX
            && frame
                .checked_sub(first)
                .is_some_and(|offset| offset.is_multiple_of(self.interval))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_world_presentation_starts_with_the_first_world_frame() {
        let cadence = WorldCadence::new(true, Some("1"));
        assert!(cadence.png_only);
        assert!((0..500).all(|n| !cadence.scheduled(n, u64::MAX)));
        assert!((0..213).all(|n| !cadence.scheduled(n, 213)));
        assert!((213..500).all(|n| cadence.scheduled(n, 213)));
    }

    #[test]
    fn valid_intervals_are_relative_to_the_first_presentation() {
        for interval in 1..=64 {
            let value = interval.to_string();
            let cadence = WorldCadence::new(true, Some(&value));
            assert!(cadence.png_only);
            let frames: Vec<_> = (148..=280).filter(|&n| cadence.scheduled(n, 150)).collect();
            assert_eq!(frames, (150..=280).step_by(interval).collect::<Vec<_>>());
        }
    }

    #[test]
    fn missing_or_invalid_values_preserve_the_existing_capture_mode() {
        for value in [
            None,
            Some(""),
            Some("0"),
            Some("65"),
            Some("-1"),
            Some("1.5"),
            Some("world"),
        ] {
            let cadence = WorldCadence::new(true, value);
            assert!(!cadence.png_only);
            assert_eq!(cadence.interval, 8);
        }
        let full_trace = WorldCadence::new(false, Some("1"));
        assert!(!full_trace.png_only);
        assert_eq!(full_trace.interval, 8);
        let frames: Vec<_> = (10..=26).filter(|&n| full_trace.scheduled(n, 10)).collect();
        assert_eq!(frames, [10, 18, 26]);
    }
}
