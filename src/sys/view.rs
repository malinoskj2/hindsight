//! Whether the monitor recorded for an app shows that app.

use std::time::{Duration, Instant};

/// How long an app must be back in front before its monitor is recorded again, so that a window
/// minimizing, restoring or fading out over it is left out of the clip.
const RETURN_DELAY: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Sighting {
    Closed,
    Hidden,
    Covered,
    Uncovered,
    /// No window has the foreground, as briefly while it changes hands.
    Unsettled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Record,
    Hold,
    Closed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum View {
    /// Where a session starts, since an app is recorded only once it has settled in front.
    #[default]
    Shown,
    Away,
    Returning {
        since: Instant,
    },
}

impl View {
    pub(crate) fn update(&mut self, sighting: Sighting, now: Instant) -> Decision {
        let (next, decision) = match (*self, sighting) {
            (view, Sighting::Closed) => (view, Decision::Closed),
            (_, Sighting::Hidden | Sighting::Covered) => (Self::Away, Decision::Hold),
            (Self::Shown, Sighting::Uncovered | Sighting::Unsettled) => {
                (Self::Shown, Decision::Record)
            }
            (Self::Away, Sighting::Uncovered) => (Self::Returning { since: now }, Decision::Hold),
            (Self::Returning { since }, Sighting::Uncovered)
                if now.saturating_duration_since(since) >= RETURN_DELAY =>
            {
                (Self::Shown, Decision::Record)
            }
            (view @ (Self::Away | Self::Returning { .. }), _) => (view, Decision::Hold),
        };
        *self = next;
        decision
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decisions(view: &mut View, start: Instant, steps: &[(Sighting, u64)]) -> Vec<Decision> {
        steps
            .iter()
            .map(|&(sighting, millis)| view.update(sighting, start + Duration::from_millis(millis)))
            .collect()
    }

    #[test]
    fn an_app_in_front_is_recorded_through_a_foreground_hand_over() {
        let mut view = View::default();
        let decided = decisions(
            &mut view,
            Instant::now(),
            &[
                (Sighting::Uncovered, 0),
                (Sighting::Unsettled, 4),
                (Sighting::Uncovered, 8),
            ],
        );
        assert_eq!(decided, [Decision::Record; 3]);
        assert_eq!(view, View::Shown);
    }

    #[test]
    fn an_app_out_of_view_is_held_at_once() {
        for sighting in [Sighting::Hidden, Sighting::Covered] {
            let mut view = View::default();
            assert_eq!(view.update(sighting, Instant::now()), Decision::Hold);
            assert_eq!(view, View::Away);
        }
    }

    #[test]
    fn an_app_is_recorded_again_once_it_has_been_back_in_front_for_the_delay() {
        let mut view = View::Away;
        let start = Instant::now();
        let delay = u64::try_from(RETURN_DELAY.as_millis()).unwrap();
        let decided = decisions(
            &mut view,
            start,
            &[
                (Sighting::Uncovered, 0),
                (Sighting::Uncovered, delay - 1),
                (Sighting::Uncovered, delay),
            ],
        );
        assert_eq!(decided, [Decision::Hold, Decision::Hold, Decision::Record]);
        assert_eq!(view, View::Shown);
    }

    #[test]
    fn covering_the_app_again_while_it_returns_restarts_the_delay() {
        let mut view = View::Away;
        let start = Instant::now();
        let delay = u64::try_from(RETURN_DELAY.as_millis()).unwrap();
        let decided = decisions(
            &mut view,
            start,
            &[
                (Sighting::Uncovered, 0),
                (Sighting::Covered, 10),
                (Sighting::Uncovered, 20),
                (Sighting::Uncovered, delay + 10),
                (Sighting::Uncovered, delay + 20),
            ],
        );
        assert_eq!(
            decided,
            [
                Decision::Hold,
                Decision::Hold,
                Decision::Hold,
                Decision::Hold,
                Decision::Record
            ]
        );
    }

    #[test]
    fn no_foreground_window_neither_holds_nor_resumes() {
        let mut view = View::Away;
        let start = Instant::now();
        assert_eq!(view.update(Sighting::Unsettled, start), Decision::Hold);
        assert_eq!(view, View::Away);

        let since = start;
        let mut view = View::Returning { since };
        assert_eq!(
            view.update(Sighting::Unsettled, start + RETURN_DELAY),
            Decision::Hold
        );
        assert_eq!(view, View::Returning { since });
    }

    #[test]
    fn a_closed_window_ends_the_recording_from_any_view() {
        let now = Instant::now();
        for mut view in [View::Shown, View::Away, View::Returning { since: now }] {
            assert_eq!(view.update(Sighting::Closed, now), Decision::Closed);
        }
    }
}
