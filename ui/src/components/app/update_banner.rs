//! The banner shown when River re-keyed while this tab was open
//! (freenet/river#580). The decision of when to reload lives in
//! `freenet_api::generation_watch`; this only renders it.

use super::freenet_api::generation_watch::{reload_now, snooze, Notice, UPDATE_NOTICE};
use dioxus::prelude::*;

/// `(message, show "Not now", show "Reload")` for a notice.
fn banner_text(notice: Notice) -> (String, bool, bool) {
    match notice {
        // A first try counts down seconds; only a retry waits minutes, and a
        // retry means the new version has not reached this node yet.
        Notice::Countdown { secs } if secs > 60 => (
            format!(
                "River has been updated, but the new version hasn't reached your node yet. \
                 Trying again in {}.",
                countdown_text(secs)
            ),
            true,
            true,
        ),
        Notice::Countdown { secs } => (
            format!(
                "River has been updated. Reloading in {} so you keep receiving messages.",
                countdown_text(secs)
            ),
            true,
            true,
        ),
        Notice::WaitingForReader => (
            "River has been updated and will reload as soon as it's safe to.".to_string(),
            false,
            true,
        ),
        // The Reload button would fail the same way the automatic one did.
        Notice::RefreshTab => (
            "River has been updated but could not reload itself. Refresh this tab to keep \
             receiving messages."
                .to_string(),
            false,
            false,
        ),
        Notice::Manual => (
            "River has been updated. Reload to keep receiving messages.".to_string(),
            false,
            true,
        ),
        Notice::Reloading => ("Reloading River…".to_string(), false, false),
    }
}

fn countdown_text(secs: u64) -> String {
    if secs >= 120 {
        format!("{} min", secs.div_ceil(60))
    } else {
        format!("{secs}s")
    }
}

#[component]
pub fn UpdateBanner() -> Element {
    // Anchor before the fallible read so a contended pass still re-renders
    // (freenet/river#555, .claude/rules/dioxus-signal-safety.md).
    crate::util::signal_guard::anchor();
    let Ok(notice) = UPDATE_NOTICE.try_read().map(|n| *n) else {
        crate::util::signal_guard::schedule_nudge();
        return rsx! {};
    };
    let Some(notice) = notice else {
        return rsx! {};
    };

    let (message, show_not_now, show_reload) = banner_text(notice);

    rsx! {
        div {
            role: "status",
            "aria-live": "polite",
            "data-testid": "update-banner",
            // Below the conversation header, so it does not hide the room title.
            class: "fixed top-[4.5rem] left-1/2 -translate-x-1/2 z-50 w-[calc(100%-1rem)] max-w-xl \
                    px-4 py-3 rounded-lg shadow-lg bg-surface border border-border text-text text-sm \
                    flex flex-wrap items-center gap-3",
            span { class: "flex-1 min-w-[12rem]", "{message}" }
            div { class: "flex gap-2",
                if show_not_now {
                    button {
                        "data-testid": "update-banner-later",
                        class: "px-3 py-1.5 rounded-md text-text-muted hover:bg-surface-hover",
                        onclick: move |_| snooze(),
                        "Not now"
                    }
                }
                if show_reload {
                    button {
                        "data-testid": "update-banner-reload",
                        class: "px-3 py-1.5 rounded-md bg-accent text-white font-medium",
                        onclick: move |_| reload_now(),
                        "Reload"
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{banner_text, countdown_text, Notice};

    #[test]
    fn a_long_wait_explains_itself_and_reloading_offers_no_buttons() {
        let (retry, ..) = banner_text(Notice::Countdown { secs: 300 });
        assert!(retry.contains("hasn't reached your node"), "{retry}");
        let (first, ..) = banner_text(Notice::Countdown { secs: 15 });
        assert!(first.contains("Reloading in 15s"), "{first}");
        assert_eq!(banner_text(Notice::Reloading).1, false);
        assert_eq!(banner_text(Notice::Reloading).2, false);
        assert!(banner_text(Notice::Manual).2);
        let (refresh, not_now, reload) = banner_text(Notice::RefreshTab);
        assert!(refresh.contains("Refresh this tab"), "{refresh}");
        assert!(
            !not_now && !reload,
            "a failed reload must not offer the same reload again"
        );
    }

    #[test]
    fn short_countdowns_in_seconds_long_ones_in_minutes() {
        assert_eq!(countdown_text(15), "15s");
        assert_eq!(countdown_text(119), "119s");
        assert_eq!(countdown_text(120), "2 min");
        assert_eq!(countdown_text(300), "5 min");
        assert_eq!(countdown_text(301), "6 min");
    }
}
