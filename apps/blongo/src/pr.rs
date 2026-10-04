//! How a thread's pull request looks in the UI: the badge, its colour and
//! which status changes are worth a notification.

use blongo_protocol::{ChecksState, Mergeable, PrBadge, PrState, PrStatus, ReviewDecision, Thread};
use gpui::Hsla;

use crate::theme;

/// Colour of a badge.
pub fn color(badge: PrBadge) -> Hsla {
    match badge {
        PrBadge::Merged => theme::accent(),
        PrBadge::ReadyToMerge => theme::success().into(),
        PrBadge::Conflict | PrBadge::ChecksFailing | PrBadge::ChangesRequested => {
            theme::danger().into()
        }
        PrBadge::ChecksRunning => theme::warning().into(),
        PrBadge::Closed | PrBadge::Draft | PrBadge::Open => theme::text_faint(),
    }
}

/// One glyph next to the number in the sidebar.
pub fn glyph(badge: PrBadge) -> &'static str {
    match badge {
        PrBadge::Merged => "⇡",
        PrBadge::Closed => "×",
        PrBadge::Conflict => "!",
        PrBadge::ChecksFailing => "✗",
        PrBadge::ChangesRequested => "±",
        PrBadge::ChecksRunning => "…",
        PrBadge::ReadyToMerge => "✓",
        PrBadge::Draft => "◌",
        PrBadge::Open => "",
    }
}

/// The badge of a thread's pull request (`None`: not linked).
pub fn badge(thread: &Thread) -> Option<PrBadge> {
    thread.pr.as_ref()?;
    Some(
        thread
            .pr_status
            .as_ref()
            .map_or(PrBadge::Open, PrStatus::badge),
    )
}

/// `#12 ✓` for the sidebar.
pub fn sidebar_label(thread: &Thread) -> Option<String> {
    let pr = thread.pr.as_ref()?;
    let badge = badge(thread)?;
    let glyph = glyph(badge);
    Some(if glyph.is_empty() {
        format!("#{}", pr.number)
    } else {
        format!("#{} {glyph}", pr.number)
    })
}

/// What changed between two polls that the user wants to hear about, as
/// a notification line. `None`: nothing worth interrupting for (the first
/// status after linking counts as nothing).
pub fn transition(old: Option<&PrStatus>, new: &PrStatus) -> Option<&'static str> {
    let old = old?;
    if new.error.is_some() {
        return None;
    }
    if old.state != new.state {
        return match new.state {
            PrState::Merged => Some("was merged"),
            PrState::Closed => Some("was closed"),
            PrState::Open if old.state == PrState::Draft => Some("is ready for review"),
            _ => None,
        };
    }
    if new.mergeable == Mergeable::Conflicting && old.mergeable != Mergeable::Conflicting {
        return Some("has a merge conflict");
    }
    if new.checks.state != old.checks.state || new.head_sha != old.head_sha {
        match new.checks.state {
            ChecksState::Failure if old.checks.state != ChecksState::Failure => {
                return Some("has failing checks");
            }
            ChecksState::Success if old.checks.state != ChecksState::Success => {
                return Some("passed its checks");
            }
            _ => {}
        }
    }
    if new.review != old.review {
        return match new.review {
            ReviewDecision::Approved => Some("was approved"),
            ReviewDecision::ChangesRequested => Some("has changes requested"),
            _ => None,
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transitions_worth_a_notification() {
        let base = PrStatus::default();
        assert_eq!(transition(None, &base), None);
        let mut failing = base.clone();
        failing.checks.state = ChecksState::Failure;
        assert_eq!(
            transition(Some(&base), &failing),
            Some("has failing checks")
        );
        assert_eq!(transition(Some(&failing), &failing), None);
        let mut passed = base.clone();
        passed.checks.state = ChecksState::Success;
        assert_eq!(
            transition(Some(&failing), &passed),
            Some("passed its checks")
        );
        let mut merged = passed.clone();
        merged.state = PrState::Merged;
        assert_eq!(transition(Some(&passed), &merged), Some("was merged"));
        let mut approved = passed.clone();
        approved.review = ReviewDecision::Approved;
        assert_eq!(transition(Some(&passed), &approved), Some("was approved"));
        let mut errored = approved.clone();
        errored.error = Some("offline".into());
        assert_eq!(transition(Some(&passed), &errored), None);
    }
}
