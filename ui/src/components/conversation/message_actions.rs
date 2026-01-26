use dioxus::prelude::*;
use river_core::room_state::member::MemberId;
use river_core::room_state::message::MessageId;

use super::emoji_picker::FREQUENT_EMOJIS;

/// Props for the message actions component
#[derive(Props, Clone, PartialEq)]
pub struct MessageActionsProps {
    /// The message ID for this message
    pub message_id: MessageId,
    /// Whether the current user is the author of this message
    pub is_own_message: bool,
    /// Current user's ID for checking existing reactions
    pub self_member_id: MemberId,
    /// Callback when edit is clicked
    pub on_edit: EventHandler<MessageId>,
    /// Callback when delete is clicked
    pub on_delete: EventHandler<MessageId>,
    /// Callback when a reaction is toggled (message_id, emoji)
    pub on_toggle_reaction: EventHandler<(MessageId, String)>,
}

/// Message actions component - shows on hover/click
#[component]
pub fn MessageActions(props: MessageActionsProps) -> Element {
    let mut show_emoji_picker = use_signal(|| false);
    let message_id = props.message_id.clone();
    let message_id_for_edit = message_id.clone();
    let message_id_for_delete = message_id.clone();

    rsx! {
        div {
            class: "flex items-center gap-0.5 bg-panel rounded-lg shadow-lg border border-border p-0.5",
            // Emoji reaction button
            div { class: "relative",
                button {
                    class: "p-1.5 rounded hover:bg-surface transition-colors text-text-muted hover:text-text",
                    title: "Add reaction",
                    onclick: move |_| {
                        show_emoji_picker.set(!show_emoji_picker());
                    },
                    "😀"
                }
                // Emoji picker dropdown (appears below to avoid header clipping)
                if show_emoji_picker() {
                    div {
                        class: "absolute top-full left-0 mt-1 bg-panel rounded-lg shadow-lg border border-border p-2 z-50",
                        div { class: "flex flex-wrap gap-1 max-w-[200px]",
                            {FREQUENT_EMOJIS.iter().map(|emoji| {
                                let emoji_str = emoji.to_string();
                                let msg_id = message_id.clone();
                                rsx! {
                                    button {
                                        key: "{emoji}",
                                        class: "p-1.5 rounded hover:bg-surface transition-colors text-lg",
                                        title: "React with {emoji}",
                                        onclick: move |_| {
                                            props.on_toggle_reaction.call((msg_id.clone(), emoji_str.clone()));
                                            show_emoji_picker.set(false);
                                        },
                                        "{emoji}"
                                    }
                                }
                            })}
                        }
                    }
                }
            }
            // Edit button (only for own messages)
            if props.is_own_message {
                button {
                    class: "p-1.5 rounded hover:bg-surface transition-colors text-text-muted hover:text-text",
                    title: "Edit message",
                    onclick: move |_| {
                        props.on_edit.call(message_id_for_edit.clone());
                    },
                    "✏️"
                }
            }
            // Delete button (only for own messages)
            if props.is_own_message {
                button {
                    class: "p-1.5 rounded hover:bg-error-bg hover:text-red-600 transition-colors text-text-muted",
                    title: "Delete message",
                    onclick: move |_| {
                        props.on_delete.call(message_id_for_delete.clone());
                    },
                    "🗑️"
                }
            }
        }
    }
}

/// Props for an inline reaction button (shown when hovering a message)
#[derive(Props, Clone, PartialEq)]
pub struct QuickReactionProps {
    pub message_id: MessageId,
    pub on_toggle_reaction: EventHandler<(MessageId, String)>,
}

/// Quick reaction button - a simpler inline option
#[component]
pub fn QuickReactionButton(props: QuickReactionProps) -> Element {
    let mut show_picker = use_signal(|| false);

    rsx! {
        div { class: "relative inline-block",
            button {
                class: "opacity-0 group-hover:opacity-100 transition-opacity p-1 rounded hover:bg-surface text-text-muted hover:text-text text-sm",
                onclick: move |_| show_picker.set(!show_picker()),
                "+"
            }
            if show_picker() {
                div {
                    class: "absolute top-full right-0 mt-1 bg-panel rounded-lg shadow-lg border border-border p-1 flex gap-0.5 z-50",
                    {FREQUENT_EMOJIS.iter().take(6).map(|emoji| {
                        let emoji_str = emoji.to_string();
                        let message_id = props.message_id.clone();
                        rsx! {
                            button {
                                key: "{emoji}",
                                class: "p-1 rounded hover:bg-surface transition-colors",
                                onclick: move |_| {
                                    props.on_toggle_reaction.call((message_id.clone(), emoji_str.clone()));
                                    show_picker.set(false);
                                },
                                "{emoji}"
                            }
                        }
                    })}
                }
            }
        }
    }
}
