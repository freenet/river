//! The banner shown when River re-keyed while this tab was open
//! (freenet/river#580). The decision of when to reload lives in
//! `freenet_api::generation_watch`; this only renders it.

use super::freenet_api::generation_watch::{reload_now, snooze, Notice, UPDATE_NOTICE};
use dioxus::prelude::*;

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

    let (message, show_not_now) = match notice {
        Notice::Countdown { secs } => (
            format!(
                "River has been updated. Reloading in {} so you keep receiving messages.",
                countdown_text(secs)
            ),
            true,
        ),
        Notice::WaitingForDraft => (
            "River has been updated. It will reload once you send or clear your message."
                .to_string(),
            false,
        ),
        Notice::Manual => (
            "River has been updated. Reload to keep receiving messages.".to_string(),
            false,
        ),
    };

    rsx! {
        div {
            role: "status",
            "aria-live": "polite",
            "data-testid": "update-banner",
            class: "fixed top-2 left-1/2 -translate-x-1/2 z-50 w-[calc(100%-1rem)] max-w-xl \
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

#[cfg(test)]
mod tests {
    use super::countdown_text;

    #[test]
    fn short_countdowns_in_seconds_long_ones_in_minutes() {
        assert_eq!(countdown_text(15), "15s");
        assert_eq!(countdown_text(119), "119s");
        assert_eq!(countdown_text(120), "2 min");
        assert_eq!(countdown_text(300), "5 min");
        assert_eq!(countdown_text(301), "6 min");
    }
}
