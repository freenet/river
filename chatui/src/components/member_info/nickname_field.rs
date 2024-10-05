use dioxus::prelude::*;
use common::state::member::AuthorizedMember;
use common::state::member_info::AuthorizedMemberInfo;

#[component]
pub fn NicknameField(
    member: AuthorizedMember,
    member_info: AuthorizedMemberInfo
) -> Element {
    rsx! {
        h1 { "Member Info" }
        div { class: "field",
            label { class: "label", "Nickname" }
            div { class: "control",
                input {
                    class: "input",
                    value: member_info.member_info.preferred_nickname,
                    readonly: true
                }
            }
        }
    }
}
