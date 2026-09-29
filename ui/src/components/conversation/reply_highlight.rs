//! Jump from a reply's quote strip to the quoted row and light it for 2s.

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen::prelude::wasm_bindgen(inline_js = r##"
let live = null;
export function jump_to_reply_target(id) {
  const row = document.getElementById(id);
  if (!row) return;
  const view = row.closest("#chat-scroll-container");
  const tall = view && row.getBoundingClientRect().height + 24 > view.clientHeight;
  row.scrollIntoView({ block: tall ? "start" : "center" });
  live?.cancel();
  const bg = getComputedStyle(row).getPropertyValue("--color-surface").trim();
  live = row.animate([{ backgroundColor: bg }, { backgroundColor: bg }], { duration: 2000, id: "reply-highlight" });
}
"##)]
extern "C" {
    pub(super) fn jump_to_reply_target(row_id: &str);
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn jump_to_reply_target(_row_id: &str) {}
