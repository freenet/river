//! Detecting display names that are visually confusable with a privileged
//! member's.
//!
//! ## Why this exists
//!
//! [`crate::util::display_name`] stops a nickname from *forging* the 🛡 shield,
//! and its module header states plainly that homoglyphs are out of scope for
//! it. That leaves the other half of the same attack: a member who cannot draw
//! a shield can still call themselves `lan Clarke` (lowercase L for capital i)
//! and be taken for the moderator `Ian Clarke` by every reader who does not
//! notice the shield is missing.
//!
//! **Absence of a shield is a weak signal.** Most legitimate members have no
//! shield either, so an unbadged `Ian Clarke` reads as an ordinary member
//! unless the viewer already knows that Ian should be badged. This module
//! inverts that: instead of relying on the reader to notice something missing,
//! the impostor is marked with something present.
//!
//! This is not hypothetical. On 2026-07-25 the Freenet Official room banned
//! nine `Ian Clarke` impersonators, and then a tenth joined as `lan Clarke`,
//! which walked straight past the exact-match check that caught the first nine.
//!
//! ## Two tiers
//!
//! Ported from the Python engine in `~/bin/river-official-autoban.sh`, which
//! was validated against a live impersonation fixture before this port existed.
//! The tiering is the load-bearing part:
//!
//! * [`ConfusableTier::Identical`] — the two names fold to the same
//!   [`skeleton`]. They are visually the *same string*, in the spirit of
//!   Unicode TR39 skeletons.
//! * [`ConfusableTier::NearMiss`] — the skeletons are within a small,
//!   length-scaled Damerau-Levenshtein distance (transpositions count as one
//!   edit). Catches `Ian Clark`, `Ian Clrake`, `Ian Clarkee`, which no skeleton
//!   can.
//!
//! In the autoban script the tiers have wildly different error budgets: tier 1
//! *bans* (a false positive removes an innocent person) and tier 2 only
//! *reports*.
//!
//! **This UI renders tier 1 only.** Tier 2 is computed, returned and tested, but
//! [`crate::components::members::impersonation_warning_for_display`] — the one
//! function every render surface goes through — drops it. The reason is
//! measured, not assumed: River assigns every member one of 10,000 generated
//! handles ([`crate::nickname`]), and two of them, `Amber Worm` and
//! `Ember Worm`, are one edit apart. No two generated handles share a skeleton
//! (tier 1 is clean on them, and
//! `generated_handles_never_fold_to_the_same_skeleton` pins that), but the tier-2
//! sweep flags a pair River itself hands out. A tier that accuses innocent
//! members *by default*, before any attacker does anything, is not a tier this
//! UI can render — so the badge fires only when two names fold to the **same**
//! skeleton, i.e. when they are visually the same string.
//!
//! ## Deciding who is warned about
//!
//! The check is **identity-first, name-second**: [`ImpersonationChecker`] holds
//! the set of member IDs that legitimately hold privilege, and a member whose
//! own ID is in that set is *never* warned about, whatever they are called.
//! Member IDs are derived from a keypair, so they cannot be chosen or forged.
//! Flagging the real moderator instead of the impostor would be worse than
//! shipping nothing, and that ordering is what makes it impossible.
//!
//! ## Honest limits
//!
//! * Only collisions with *privileged* names fire (the room owner, and deputies
//!   the viewer would see a shield for). Two ordinary members with similar names
//!   are not flagged, because a warning on every near-duplicate would train
//!   people to ignore the warning — and only the privileged names carry the
//!   authority worth stealing.
//! * A member who legitimately shares a moderator's name **is** warned about.
//!   That is intended: the viewer genuinely cannot tell the two apart on sight,
//!   which is the fact the warning exists to convey.
//! * The folds are deliberately small. A fold only matters when the result
//!   lands on (or very near) a protected name, so the false-positive surface is
//!   the set of ordinary names that fold onto a moderator's — tiny. The
//!   `no_false_positives_on_ordinary_names` and
//!   `generated_handles_never_fold_to_the_same_skeleton` tests are the guard.
//! * **Latin accents are stripped, which is more aggressive than TR39** and is
//!   an accepted trade rather than an oversight. `Müller`/`Muller`,
//!   `Böll`/`Boll` and `Möller`/`Moller` are distinct family names and DO
//!   collide; stripping is what catches `Ìan Clarke`. Likewise `rn -> m` is a
//!   real confusable that also collides `Marnie`/`Mamie` and `Lorna`/`Loma`.
//!   Both are listed in `documented_accepted_collisions`, which fails if either
//!   silently stops holding — the point is that the accepted list is short,
//!   named, and testable rather than discovered by an accused member.
//! * **The accent strip is Latin-only.** `is_combining_mark` covers the Latin
//!   combining ranges but NOT Hebrew niqqud (U+0591..U+05C7), Arabic harakat
//!   (U+064B..U+065F), or Devanagari/Thai vowel signs. So a Hebrew or Arabic
//!   name is compared with its marks intact. That is inconsistent, and it is
//!   also a bypass in principle (marks a reader barely sees are not folded
//!   away). It is left as-is deliberately: those marks are far more often
//!   meaning-bearing than the Latin ones, and widening the strip is exactly the
//!   kind of change that starts flagging real people in scripts this codebase
//!   has no test corpus for. Revisit only with a corpus.
//! * This module compares names. It does not, and cannot, tell you whether the
//!   person behind an unflagged name is who they say they are.

use crate::util::display_name::is_display_hidden;
use river_core::room_state::member::MemberId;
use std::collections::HashSet;

/// How close a name is to a protected one.
///
/// **The UI renders [`Identical`](ConfusableTier::Identical) only** — see the
/// module header, and
/// [`crate::components::members::impersonation_warning_for_display`], which is
/// where that decision lives. Both tiers are computed and returned so the
/// distinction stays testable: a future change that silently promoted every
/// near-miss to an identical match (or the reverse) would otherwise be
/// invisible, and reversing the tier decision needs the measurement to still be
/// there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ConfusableTier {
    /// Folds to the identical skeleton — visually the same string.
    Identical,
    /// Within a small, length-scaled edit distance of the skeleton.
    NearMiss,
}

/// What kind of privilege a protected name carries.
///
/// Drives the tooltip's remedy clause, because the two roles are marked with
/// different glyphs in different places: a deputy shows 🛡 next to their name
/// everywhere, whereas the room owner's 👑 appears only in the member list.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProtectedRole {
    /// The room owner.
    Owner,
    /// A member holding a deputy (moderator) grant the viewer would see a
    /// shield for.
    Deputy,
}

/// A privileged name that must not be impersonated.
///
/// Carries BOTH folds (see [`Fold`]) plus their space-stripped forms, computed
/// once at construction. `check_name` is the per-member hot path and must not
/// re-fold or re-allocate per candidate.
#[derive(Clone, PartialEq, Debug)]
pub struct ProtectedName {
    pub role: ProtectedRole,
    /// The privileged member's display name, already sanitised.
    pub display_name: String,
    visual: String,
    visual_no_space: String,
    case_insensitive: String,
    case_insensitive_no_space: String,
}

impl ProtectedName {
    pub fn new(role: ProtectedRole, display_name: impl Into<String>) -> Self {
        let display_name = display_name.into();
        let visual = skeleton_with(&display_name, Fold::Visual);
        let case_insensitive = skeleton_with(&display_name, Fold::CaseInsensitive);
        Self {
            role,
            visual_no_space: strip_spaces(&visual),
            case_insensitive_no_space: strip_spaces(&case_insensitive),
            visual,
            case_insensitive,
            display_name,
        }
    }
}

fn strip_spaces(s: &str) -> String {
    s.chars().filter(|c| *c != ' ').collect()
}

/// The two skeletons a name folds to. **Both are needed, and neither alone is
/// sufficient** — this is the fix for a false-positive class found in review.
///
/// The bar-shaped characters (`I`, `l`, `1`, `|`, `!`) render identically, and
/// lowercase `i` does NOT (it has a dot). But case-insensitivity says `I` ≡ `i`.
/// Composing the two transitively gives `i` ≡ `l`, and then any two names
/// differing only in `i` vs `l` collide:
///
/// * `Ilan` (Hebrew) / `Lian` (Chinese)
/// * `Alia` (Arabic) / `Alla` (Russian), `Ilya` / `Liya`, `Ila` / `Lia`
///
/// That transitive collapse is unavoidable in ONE skeleton: `Ian` ≡ `ian` (case)
/// and `Ian` ≡ `lan` (the live attack) force `i` ≡ `l`. Verified exhaustively —
/// every single-skeleton variant either keeps those false positives or breaks a
/// row of the validated table (moving the fold before the case step drops
/// `IAN CLARKE`, because capital `L` is then no longer folded).
///
/// So the collapse is split across two skeletons and a name matches if EITHER
/// agrees. Each keeps the bar class as a `'1'` sentinel that never merges with
/// the letter `i`, so neither one alone equates them:
///
/// * [`Fold::Visual`] folds the bar class BEFORE case, so `I` and `l` merge and
///   lowercase `i` stays distinct. Catches `lan Clarke`, `1an Clarke`,
///   `Ian CIarke`.
/// * [`Fold::CaseInsensitive`] folds it AFTER case, so `L`/`l` and `I`/`i` merge
///   as letters. Catches `IAN CLARKE`, `ian clarke`.
///
/// `Ilan`/`Lian` agree under neither, so they are no longer flagged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fold {
    Visual,
    CaseInsensitive,
}

/// The warning shown next to an impersonating member's name.
#[derive(Clone, PartialEq, Debug)]
pub struct ImpersonationWarning {
    /// Who is being imitated.
    pub impersonated: ProtectedName,
    /// How close the resemblance is. **Callers must not render a warning
    /// straight from this value** — go through
    /// [`crate::components::members::impersonation_warning_for_display`], which
    /// applies the tier decision (only `Identical` is shown).
    pub tier: ConfusableTier,
}

/// The glyph rendered for a warning.
///
/// U+26A0 sits inside the Miscellaneous Symbols range that
/// [`crate::util::display_name::is_display_hidden`] strips, so a nickname can
/// never contain this character — the warning cannot be forged, and an impostor
/// cannot pre-empt it by putting one in their own name to make the real signal
/// look like decoration. `badge_glyphs_cannot_survive_a_nickname` in that
/// module pins the strip; `the_warning_glyph_cannot_appear_in_a_nickname`
/// below pins that this constant is one of them.
pub const WARNING_GLYPH: &str = "\u{26a0}";

impl ImpersonationWarning {
    /// The `title=` / `aria-label` text, built **ONLY from trusted literals**.
    ///
    /// Three things, because a bare ⚠ teaches nobody anything: what is wrong,
    /// that this member is *not* who they resemble, and what to look for
    /// instead. One definition for every surface, so the warning cannot say
    /// different things in different places — the same rule
    /// [`crate::components::members::DeputyBadge::tooltip`] follows for the
    /// shield.
    ///
    /// ## No nickname reaches this string, and none may be added
    ///
    /// `impersonated.display_name` is right there on the struct and naming the
    /// imitated member reads like an improvement. It is not, and it must not be
    /// re-attempted:
    ///
    /// * **The name is attacker-choosable.** Nothing in `MemberInfoV1::verify`
    ///   validates a `deputies` list, so any member who is a strict ancestor of
    ///   the viewer can deputise a sockpuppet and name it whatever they like.
    ///   That sockpuppet is then a protected name in the viewer's view.
    /// * **A `title=` attribute is a flat string, so quoting is not a defense.**
    ///   `DeputyBadge::tooltip` learned this the hard way (#488): the forging
    ///   primitive is the COMMA, not the quote, and a payload like
    ///   `Bob, the room owner, Carol` needs no quote character at all. Putting
    ///   the name last and stripping its quotes narrows the hole but does not
    ///   close it — the reader still cannot tell our sentence from theirs at
    ///   tooltip size.
    /// * **Naming it buys almost nothing here.** Only
    ///   [`ConfusableTier::Identical`] is rendered, which means the imitated
    ///   name folds to the *same skeleton* as the name beside the badge — the
    ///   reader is already looking at it. What they cannot see, and what this
    ///   string supplies, is the ROLE being imitated and the badge to check for.
    ///
    /// If a surface can render separate DOM nodes, it may show the name from
    /// [`ImpersonationWarning::impersonated`] as its own element, where a comma
    /// inside one node cannot span two — the approach `DeputyBadge::appointer_names`
    /// takes for the member-info modal. Never join it into this string.
    ///
    /// Pinned by `tooltip_contains_no_nickname_content`.
    pub fn tooltip(&self) -> String {
        match self.impersonated.role {
            ProtectedRole::Owner => "Impersonation warning: this member is NOT the room owner, \
                 but their name closely resembles the owner's. The real owner is marked \
                 \u{1f451} in the member list."
                .to_string(),
            ProtectedRole::Deputy => "Impersonation warning: this member is NOT a moderator, \
                 but their name closely resembles one. A real moderator shows a \u{1f6e1} \
                 shield next to their name."
                .to_string(),
        }
    }
}

/// Decides which members' names are confusable with a privileged member's.
///
/// Built once per render pass from the viewer's room state (see
/// [`crate::components::members::impersonation_checker_for_viewer`]) and then
/// asked about each rendered name. Construction folds the protected names once;
/// [`ImpersonationChecker::check`] is the per-name hot path.
#[derive(Clone, PartialEq, Debug, Default)]
pub struct ImpersonationChecker {
    protected: Vec<ProtectedName>,
    /// IDs that legitimately hold privilege. A member in this set is never
    /// warned about — see the module header on why identity comes first.
    privileged_ids: HashSet<MemberId>,
}

impl ImpersonationChecker {
    pub fn new(protected: Vec<ProtectedName>, privileged_ids: HashSet<MemberId>) -> Self {
        Self {
            protected,
            privileged_ids,
        }
    }

    /// Whether this checker can ever produce a warning.
    ///
    /// Lets a caller skip a per-member sweep in a room with no protected names
    /// at all.
    pub fn is_empty(&self) -> bool {
        self.protected.is_empty()
    }

    /// Whether `id` legitimately holds privilege, and so is never warned about.
    pub fn is_privileged(&self, id: MemberId) -> bool {
        self.privileged_ids.contains(&id)
    }

    /// The warning for `display_name` as shown for member `id`, if any.
    ///
    /// `display_name` must be the text actually rendered — i.e. the output of
    /// [`crate::util::display_name::display_nickname`]. Checking the raw
    /// nickname instead would compare something the reader never sees.
    pub fn check(&self, id: MemberId, display_name: &str) -> Option<ImpersonationWarning> {
        // Identity first. A real moderator must never be flagged, and the only
        // way to guarantee that is to answer "is this actually them?" before
        // looking at the name at all.
        if self.privileged_ids.contains(&id) {
            return None;
        }
        self.check_name(display_name)
    }

    /// [`check`](Self::check) restricted to [`ConfusableTier::Identical`].
    ///
    /// **This is what the UI renders** (see
    /// [`crate::components::members::impersonation_warning_for_display`]), and
    /// it exists so the render path never pays for the tier it discards: it
    /// returns before the Damerau-Levenshtein sweep, which is `O(protected x
    /// len^2)` with a `Vec` allocation per DP row and runs for every
    /// NON-matching member on every render — i.e. almost all of them.
    pub fn check_identical(
        &self,
        id: MemberId,
        display_name: &str,
    ) -> Option<ImpersonationWarning> {
        if self.privileged_ids.contains(&id) {
            return None;
        }
        let folds = self.candidate_folds(display_name)?;
        self.tier_one(&folds)
    }

    /// The name half of [`check`](Self::check), with no identity check.
    ///
    /// Exposed for tests. Production callers must use `check` or
    /// `check_identical`, so the identity-first rule cannot be skipped.
    pub fn check_name(&self, display_name: &str) -> Option<ImpersonationWarning> {
        let Some(folds) = self.candidate_folds(display_name) else {
            return None;
        };
        // Tier 1 across the WHOLE protected set before tier 2, so an exact
        // skeleton match is always reported as `Identical` even when some other
        // protected name is a near-miss. Otherwise the reported tier would
        // depend on the order the protected set happened to be built in.
        if let Some(hit) = self.tier_one(&folds) {
            return Some(hit);
        }
        self.tier_two(&folds)
    }

    /// Both folds of a candidate name, or `None` when no match is possible.
    fn candidate_folds(&self, display_name: &str) -> Option<CandidateFolds> {
        if self.protected.is_empty() {
            return None;
        }
        let visual = skeleton_with(display_name, Fold::Visual);
        if visual.is_empty() {
            return None;
        }
        let case_insensitive = skeleton_with(display_name, Fold::CaseInsensitive);
        Some(CandidateFolds {
            visual_no_space: strip_spaces(&visual),
            case_insensitive_no_space: strip_spaces(&case_insensitive),
            visual_chars: visual.chars().collect(),
            visual,
            case_insensitive,
        })
    }

    /// An exact match under EITHER fold. See [`Fold`] for why one is not enough.
    fn tier_one(&self, c: &CandidateFolds) -> Option<ImpersonationWarning> {
        for p in &self.protected {
            let hit = c.visual == p.visual
                || c.visual_no_space == p.visual_no_space
                || c.case_insensitive == p.case_insensitive
                || c.case_insensitive_no_space == p.case_insensitive_no_space;
            if hit {
                return Some(ImpersonationWarning {
                    impersonated: p.clone(),
                    tier: ConfusableTier::Identical,
                });
            }
        }
        None
    }

    /// A near-miss under the VISUAL fold. Not rendered by this UI — see
    /// [`check_identical`](Self::check_identical) — but kept, computed and
    /// tested so the tier decision stays reversible and measurable.
    fn tier_two(&self, c: &CandidateFolds) -> Option<ImpersonationWarning> {
        let budget = edit_budget(c.visual_chars.len());
        if budget == 0 {
            return None;
        }
        let mut best: Option<ImpersonationWarning> = None;
        for p in &self.protected {
            let p_chars: Vec<char> = p.visual.chars().collect();
            if damerau_within(&c.visual_chars, &p_chars, budget) <= budget {
                // Owner outranks deputy so the more severe impersonation is the
                // one named, and the result does not depend on set order.
                let better = best.as_ref().is_none_or(|b| {
                    p.role == ProtectedRole::Owner && b.impersonated.role != ProtectedRole::Owner
                });
                if better {
                    best = Some(ImpersonationWarning {
                        impersonated: p.clone(),
                        tier: ConfusableTier::NearMiss,
                    });
                }
            }
        }
        best
    }
}

/// A candidate name's folds, computed once per `check`.
struct CandidateFolds {
    visual: String,
    visual_no_space: String,
    visual_chars: Vec<char>,
    case_insensitive: String,
    case_insensitive_no_space: String,
}

/// How many edits away a name may be and still count as a near-miss.
///
/// Scaled by length so short names do not over-match: at four characters or
/// fewer, one edit turns most names into most other names, so tier 2 is
/// switched off entirely and only an identical skeleton counts.
fn edit_budget(len: usize) -> usize {
    match len {
        0..=4 => 0,
        5..=12 => 1,
        _ => 2,
    }
}

/// Fold a display name to its visual skeleton.
///
/// Two names with the same skeleton render as the same string to a reader.
/// The pipeline mirrors the validated Python engine
/// (`~/bin/river-official-autoban.sh`), with the Unicode normalisation steps
/// replaced by explicit tables — this crate deliberately carries no Unicode
/// property or normalisation dependency (the wasm bundle size is a standing
/// concern, and [`crate::util::display_name`] made the same trade for the same
/// reason).
///
/// Order is load-bearing and is pinned by `skeleton_folds_in_reference_order`.
/// In particular the single-character ASCII map runs BEFORE the multi-character
/// one, so `"cl"` has already become `"ci"` by the time `cl -> d` is
/// considered; that rule is inert, and is kept only so the fold table stays a
/// faithful copy of the engine these results were validated against.
pub fn skeleton(name: &str) -> String {
    skeleton_with(name, Fold::Visual)
}

/// [`skeleton`] under a chosen [`Fold`]. See that type for why there are two.
fn skeleton_with(name: &str, fold: Fold) -> String {
    // 1. Drop what a reader cannot see, and fold presentation-only variants of
    //    ASCII. The invisible set is reused from the display-name table rather
    //    than re-listed here, so the two cannot drift. A hidden character that
    //    WAS whitespace becomes a space, matching `sanitize_display_name`.
    //
    //    In practice the input is already sanitised (callers pass rendered
    //    text), so the invisible strip is defence in depth — and it is what
    //    lets the unit tests feed raw attacker strings.
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if is_display_hidden(c) {
            if c.is_whitespace() {
                out.push(' ');
            }
            continue;
        }
        // 2. Fullwidth forms, and the "mathematical alphanumeric" blocks that
        //    every online fancy-text generator emits (`𝐈𝐚𝐧`, `𝗜𝗮𝗻`, `𝙸𝚊𝚗` all
        //    read as "Ian"). NFKC does this in the reference; these are the
        //    ranges that matter for names.
        if let Some(ascii) = fold_presentation_form(c) {
            out.push(ascii);
            continue;
        }
        // 3. Combining marks. `"I" + U+0300 + "n"` must fold like `"Ìn"`.
        if is_combining_mark(c) {
            continue;
        }
        // 4. Precomposed Latin letters, which carry no combining mark to strip
        //    (`à` is one codepoint). This is the NFD-then-drop-marks step.
        out.push(strip_latin_accent(c).unwrap_or(c));
    }

    // 5. Cross-script homoglyphs, BEFORE case folding. Case matters here: `Η`
    //    (Greek capital eta) looks like `H`, but its lowercase `η` looks like
    //    `n`, so one case-insensitive rule would be wrong for both. (The Python
    //    engine case-folds first, which silently disables its Greek entries;
    //    handling case properly is the one place this port deliberately
    //    improves on it, and it cannot affect that engine's validated results,
    //    which are all Latin or Cyrillic.)
    let out: String = out.chars().map(fold_homoglyph).collect();

    // 6/7. The bar class and case, in the order this [`Fold`] calls for.
    //
    //   * `Visual` folds bars FIRST, so capital `I` joins lowercase `l` (both
    //     are a plain vertical stroke) while lowercase `i` — which has a dot —
    //     stays a separate letter.
    //   * `CaseInsensitive` lowercases first, so capital `L` has become `l` and
    //     capital `I` has become `i` before the bar fold runs; that gives the
    //     case-variant match without ever equating `i` with `l`.
    //
    // Both map the bar class to the SENTINEL `'1'`, never to the letter `i`.
    // That is the whole point: folding to `i` is what made `Alia` and `Alla`
    // collide. See [`Fold`].
    let out = match fold {
        Fold::Visual => fold_bar_class(&out).to_lowercase(),
        Fold::CaseInsensitive => fold_bar_class(&out.to_lowercase()),
    };

    // 7b. The remaining ASCII confusables, which are case-agnostic (they all
    //     produce a lowercase letter, which the case step above leaves alone).
    let out: String = out.chars().map(fold_ascii_confusable).collect();

    // 8. Multi-character confusables: `rn` reads as `m`, `vv` as `w`.
    let mut out = out;
    for (from, to) in MULTI_CONFUSABLES {
        if out.contains(from) {
            out = out.replace(from, to);
        }
    }

    // 9. Collapse whitespace runs and trim, so `"Ian  Clarke"` folds onto
    //    `"Ian Clarke"`. Every whitespace character becomes a plain space
    //    first: an impostor who separates the words with U+3000 renders a
    //    visibly different name, but is one keystroke from the real thing and
    //    is worth catching.
    let mut collapsed = String::with_capacity(out.len());
    let mut last_was_space = false;
    for c in out.chars() {
        let c = if c.is_whitespace() { ' ' } else { c };
        let is_space = c == ' ';
        if !(is_space && last_was_space) {
            collapsed.push(c);
        }
        last_was_space = is_space;
    }
    collapsed.trim().to_string()
}

/// Multi-character confusables, applied after the single-character map.
///
/// `cl -> d` is inert: the bar fold has already rewritten `l`. It is retained
/// for fidelity with the upstream engine, and `skeleton_folds_in_reference_order`
/// pins that it stays inert.
///
/// **`nn -> m` was here and was REMOVED.** `rn` and `vv` are genuine visual
/// confusables — `rn` really does read as `m` at UI sizes, which is what catches
/// `Roorn Owner`. `nn` does not: `nn` and `m` are different shapes in every
/// font, joined at the shoulder or not. And doubled-n is one of the commonest
/// pairs in given names, so the rule accused a long tail of real people —
/// `Annie`/`Amie`, `Anna`/`Ama` (Akan), `Hanna`/`Hama`, `Donna`/`Doma`,
/// `Jenna`/`Jema`. It was a straight loss: no attack needs it, and it fired on
/// names nobody chose to make confusable. Do not re-add it.
const MULTI_CONFUSABLES: [(&str, &str); 3] = [("rn", "m"), ("cl", "d"), ("vv", "w")];

/// The bar-shaped characters, folded to a `'1'` sentinel.
///
/// Deliberately NOT folded to the letter `i`: `i` has a dot and is a different
/// shape, and merging them is what made `Alia`/`Alla` and `Ilan`/`Lian` collide.
/// See [`Fold`]. Which characters are in the class depends on whether the case
/// step has already run — the caller decides by choosing the [`Fold`].
fn fold_bar_class(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            // `I` is here for the pre-case pass; after `to_lowercase` it can no
            // longer appear, so the same table serves both directions.
            'l' | 'I' | '1' | '|' | '!' => '1',
            other => other,
        })
        .collect()
}

/// Single-character ASCII lookalikes OTHER than the bar class.
fn fold_ascii_confusable(c: char) -> char {
    match c {
        '0' => 'o',
        '5' | '$' => 's',
        '3' => 'e',
        '@' | '4' => 'a',
        '7' => 't',
        other => other,
    }
}

/// Cyrillic and Greek letters that are pixel-identical (or near enough) to a
/// Latin letter in a normal UI font.
///
/// Deliberately conservative, and deliberately case-aware. Adding a letter that
/// merely *resembles* a Latin one (Greek `α`, `η`, `ι`) would start folding
/// real Greek and Cyrillic names toward Latin skeletons, which is how a
/// confusable check acquires false positives on exactly the users least able to
/// argue with it.
fn fold_homoglyph(c: char) -> char {
    match c {
        // Cyrillic, uppercase.
        '\u{0410}' => 'A', // А
        '\u{0412}' => 'B', // В
        '\u{0415}' => 'E', // Е
        '\u{041A}' => 'K', // К
        '\u{041C}' => 'M', // М
        '\u{041D}' => 'H', // Н
        '\u{041E}' => 'O', // О
        '\u{0420}' => 'P', // Р
        '\u{0421}' => 'C', // С
        '\u{0422}' => 'T', // Т
        '\u{0423}' => 'Y', // У
        '\u{0425}' => 'X', // Х
        '\u{0405}' => 'S', // Ѕ
        '\u{0406}' => 'I', // І
        '\u{0408}' => 'J', // Ј
        '\u{04AE}' => 'Y', // Ү
        '\u{04BA}' => 'H', // Һ
        // Cyrillic, lowercase.
        '\u{0430}' => 'a', // а
        '\u{0435}' => 'e', // е
        '\u{043E}' => 'o', // о
        '\u{0440}' => 'p', // р
        '\u{0441}' => 'c', // с
        '\u{0443}' => 'y', // у
        '\u{0445}' => 'x', // х
        '\u{0455}' => 's', // ѕ
        '\u{0456}' => 'i', // і
        '\u{0458}' => 'j', // ј
        '\u{04BB}' => 'h', // һ
        '\u{04CF}' => 'l', // ӏ
        '\u{0501}' => 'd', // ԁ
        '\u{0475}' => 'v', // ѵ
        // Greek, uppercase — identical to the Latin capital in every common
        // font.
        '\u{0391}' => 'A', // Α
        '\u{0392}' => 'B', // Β
        '\u{0395}' => 'E', // Ε
        '\u{0396}' => 'Z', // Ζ
        '\u{0397}' => 'H', // Η
        '\u{0399}' => 'I', // Ι
        '\u{039A}' => 'K', // Κ
        '\u{039C}' => 'M', // Μ
        '\u{039D}' => 'N', // Ν
        '\u{039F}' => 'O', // Ο
        '\u{03A1}' => 'P', // Ρ
        '\u{03A4}' => 'T', // Τ
        '\u{03A5}' => 'Y', // Υ
        '\u{03A7}' => 'X', // Χ
        // Greek, lowercase — only the three that are genuinely
        // indistinguishable.
        '\u{03BF}' => 'o', // ο
        '\u{03C1}' => 'p', // ρ
        '\u{03BD}' => 'v', // ν
        other => other,
    }
}

/// Combining marks, dropped so `"I" + U+0300` folds like `"Ì"`.
fn is_combining_mark(c: char) -> bool {
    matches!(u32::from(c),
        0x0300..=0x036F   // Combining Diacritical Marks
        | 0x1AB0..=0x1AFF // Combining Diacritical Marks Extended
        | 0x1DC0..=0x1DFF // Combining Diacritical Marks Supplement
        | 0x20D0..=0x20F0 // Combining Diacritical Marks for Symbols
        | 0xFE20..=0xFE2F // Combining Half Marks
    )
}

/// Presentation-only variants of ASCII: fullwidth forms, the Letterlike
/// Symbols, and the mathematical alphanumeric blocks.
///
/// All are what NFKC folds, and all are trivially reachable — a "fancy text"
/// web page turns `Ian Clarke` into `𝗜𝗮𝗻 𝗖𝗹𝗮𝗿𝗸𝗲` in one click, and the result is
/// legible as the original to any reader.
fn fold_presentation_form(c: char) -> Option<char> {
    let cp = u32::from(c);
    // Fullwidth ASCII: U+FF01..U+FF5E maps to U+0021..U+007E.
    if (0xFF01..=0xFF5E).contains(&cp) {
        return char::from_u32(cp - 0xFEE0);
    }
    if let Some(letter) = fold_letterlike(cp) {
        return Some(letter);
    }
    // Mathematical Alphanumeric Symbols: consecutive 52-letter (A-Z then a-z)
    // blocks.
    //
    // The range contains reserved holes (e.g. U+1D455, whose letter lives in
    // Letterlike Symbols above). A hole maps to a letter here, which is
    // harmless: an unassigned codepoint renders as tofu and cannot appear in a
    // name a reader would mistake for anything.
    if (0x1D400..=0x1D7CB).contains(&cp) {
        let idx = (cp - 0x1D400) % 52;
        return Some(if idx < 26 {
            (b'A' + idx as u8) as char
        } else {
            (b'a' + (idx - 26) as u8) as char
        });
    }
    // Mathematical digits: five consecutive blocks of ten.
    if (0x1D7CE..=0x1D7FF).contains(&cp) {
        return Some((b'0' + ((cp - 0x1D7CE) % 10) as u8) as char);
    }
    None
}

/// Letterlike Symbols (U+2100..U+214F) that NFKC folds to a plain letter.
fn fold_letterlike(cp: u32) -> Option<char> {
    Some(match cp {
        0x2102 | 0x212D => 'C',
        0x2107 => 'E',
        0x210A => 'g',
        0x210B | 0x210C | 0x210D => 'H',
        0x210E | 0x210F => 'h',
        0x2110 | 0x2111 => 'I',
        0x2112 => 'L',
        0x2113 => 'l',
        0x2115 => 'N',
        0x2119 => 'P',
        0x211A => 'Q',
        0x211B | 0x211C | 0x211D => 'R',
        0x2124 | 0x2128 => 'Z',
        0x212C => 'B',
        0x212F | 0x2130 => 'E',
        0x2131 => 'F',
        0x2133 => 'M',
        0x2134 => 'o',
        _ => return None,
    })
}

/// Latin-1 Supplement and Latin Extended-A, folded to their unaccented base.
///
/// This is the NFD-then-drop-combining-marks step for the precomposed letters,
/// which carry no separate mark to strip. `'\0'` in the tables means "no
/// decomposition" — `Æ`, `Ø`, `Þ`, `ß`, `Đ`, `Ł` and friends are letters in
/// their own right and are left alone, exactly as NFD leaves them.
fn strip_latin_accent(c: char) -> Option<char> {
    let cp = u32::from(c);
    let table_char = |table: &str, offset: u32| -> Option<char> {
        table
            .chars()
            .nth((cp - offset) as usize)
            .filter(|f| *f != '\0')
    };
    match cp {
        0x00C0..=0x00FF => table_char(LATIN1_FOLD, 0x00C0),
        0x0100..=0x017F => table_char(LATIN_EXT_A_FOLD, 0x0100),
        _ => None,
    }
}

/// U+00C0..U+00FF, in order. `\0` = leave the character alone.
///
/// The tables are positional, so `latin_fold_tables_are_aligned` checks both
/// their length and a spread of individual entries — a miscounted `\0` would
/// otherwise shift every later letter onto the wrong base.
const LATIN1_FOLD: &str = concat!(
    "AAAAAA\0C",   // À Á Â Ã Ä Å Æ Ç
    "EEEEIIII",    // È É Ê Ë Ì Í Î Ï
    "\0NOOOOO\0",  // Ð Ñ Ò Ó Ô Õ Ö ×
    "\0UUUUY\0\0", // Ø Ù Ú Û Ü Ý Þ ß
    "aaaaaa\0c",   // à á â ã ä å æ ç
    "eeeeiiii",    // è é ê ë ì í î ï
    "\0nooooo\0",  // ð ñ ò ó ô õ ö ÷
    "\0uuuuy\0y",  // ø ù ú û ü ý þ ÿ
);

/// U+0100..U+017F, in order. `\0` = leave the character alone.
const LATIN_EXT_A_FOLD: &str = concat!(
    "AaAaAaCc",    // Ā ā Ă ă Ą ą Ć ć
    "CcCcCcDd",    // Ĉ ĉ Ċ ċ Č č Ď ď
    "\0\0EeEeEe",  // Đ đ Ē ē Ĕ ĕ Ė ė
    "EeEeGgGg",    // Ę ę Ě ě Ĝ ĝ Ğ ğ
    "GgGgHh\0\0",  // Ġ ġ Ģ ģ Ĥ ĥ Ħ ħ
    "IiIiIiIi",    // Ĩ ĩ Ī ī Ĭ ĭ Į į
    "I\0\0\0JjKk", // İ ı Ĳ ĳ Ĵ ĵ Ķ ķ
    "\0LlLlLlL",   // ĸ Ĺ ĺ Ļ ļ Ľ ľ Ŀ
    "l\0\0NnNnN",  // ŀ Ł ł Ń ń Ņ ņ Ň
    "n\0\0\0OoOo", // ň ŉ Ŋ ŋ Ō ō Ŏ ŏ
    "Oo\0\0RrRr",  // Ő ő Œ œ Ŕ ŕ Ŗ ŗ
    "RrSsSsSs",    // Ř ř Ś ś Ŝ ŝ Ş ş
    "SsTtTt\0\0",  // Š š Ţ ţ Ť ť Ŧ ŧ
    "UuUuUuUu",    // Ũ ũ Ū ū Ŭ ŭ Ů ů
    "UuUuWwYy",    // Ű ű Ų ų Ŵ ŵ Ŷ ŷ
    "YZzZzZzs",    // Ÿ Ź ź Ż ż Ž ž ſ
);

/// Damerau-Levenshtein distance between `a` and `b`, giving up once it exceeds
/// `cap`.
///
/// Damerau rather than plain Levenshtein so a transposition (`Clrake` for
/// `Clarke`) counts as one edit rather than two — a typo-squat is far more
/// often a swap than two independent substitutions.
///
/// Returns `cap + 1` for "further away than `cap`", which is all a caller with
/// a budget needs to know.
fn damerau_within(a: &[char], b: &[char], cap: usize) -> usize {
    if a.len().abs_diff(b.len()) > cap {
        return cap + 1;
    }
    let mut prev2: Option<Vec<usize>> = None;
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut cur = vec![0usize; b.len() + 1];
        cur[0] = i;
        let mut best = cur[0];
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
            if let Some(p2) = prev2.as_ref() {
                if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                    v = v.min(p2[j - 2] + 1);
                }
            }
            cur[j] = v;
            best = best.min(v);
        }
        if best > cap {
            // Every cell in this row already exceeds the budget, and no later
            // row can lower it.
            return cap + 1;
        }
        prev2 = Some(prev);
        prev = cur;
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mid(n: i64) -> MemberId {
        MemberId(freenet_scaffold::util::FastHash(n))
    }

    /// The protected set used by the upstream engine's validation fixture.
    fn fixture() -> ImpersonationChecker {
        ImpersonationChecker::new(
            vec![
                ProtectedName::new(ProtectedRole::Deputy, "Ian Clarke"),
                ProtectedName::new(ProtectedRole::Owner, "Room Owner"),
                ProtectedName::new(ProtectedRole::Deputy, "Invite Bot"),
            ],
            HashSet::new(),
        )
    }

    /// **The ported test table.** Every row was validated against the live
    /// impersonation fixture by the Python engine in
    /// `~/bin/river-official-autoban.sh` before this port existed; a
    /// disagreement here means the port is wrong, not the table.
    #[test]
    fn tier_one_catches_identical_skeletons() {
        let checker = fixture();
        for (name, why) in [
            (
                "lan Clarke",
                "lowercase L for capital i (the live 2026-07-25 attack)",
            ),
            ("1an Clarke", "digit one for capital i"),
            ("Ian CIarke", "capital i for lowercase L"),
            ("IAN CLARKE", "case"),
            ("Ian Clark\u{0435}", "Cyrillic small ie"),
            ("Ian\u{200B} Clarke", "zero-width space"),
            ("I\u{00E0}n Clarke", "precomposed accented a"),
            ("Ia\u{0300}n Clarke", "combining grave accent"),
            ("Roorn Owner", "rn reads as m"),
            ("lnvite Bot", "lowercase L for capital i"),
            ("Ian  Clarke", "doubled space"),
            ("IanClarke", "no space at all"),
            ("!an Clarke", "exclamation mark for capital i"),
            ("\u{1D408}\u{1D41A}\u{1D427} Clarke", "mathematical bold"),
            ("\u{FF29}\u{FF41}\u{FF4E} Clarke", "fullwidth forms"),
        ] {
            let got = checker.check_name(name);
            assert_eq!(
                got.as_ref().map(|w| w.tier),
                Some(ConfusableTier::Identical),
                "{name:?} ({why}) should fold to an identical skeleton, got {got:?}"
            );
        }
    }

    #[test]
    fn tier_two_catches_near_misses() {
        let checker = fixture();
        for (name, why) in [
            ("Ian Clark", "one character short"),
            ("Ian Clrake", "transposition"),
            ("Ian Clarkee", "one character long"),
        ] {
            let got = checker.check_name(name);
            assert_eq!(
                got.as_ref().map(|w| w.tier),
                Some(ConfusableTier::NearMiss),
                "{name:?} ({why}) should be a near-miss, got {got:?}"
            );
        }
    }

    /// The other half of the table, and the half that matters most: a warning
    /// on an ordinary name is what trains people to ignore warnings.
    #[test]
    fn no_false_positives_on_ordinary_names() {
        let checker = fixture();
        for name in [
            "Linus Clarke",
            "Ian",
            "HostFat",
            "Alice",
            "Ivvor",
            "ofansifkapital-xmpp",
            "Ian Clarke's Dad",
            "Bob",
            "Clark Kent",
            "Owner",
            "Bot",
            "Invited",
        ] {
            assert_eq!(
                checker.check_name(name),
                None,
                "{name:?} is an ordinary name and must not be flagged"
            );
        }
    }

    /// Requirement one, and the thing that would be worse to get wrong than to
    /// ship nothing: the real moderator is resolved by member ID and is never
    /// the one flagged.
    #[test]
    fn the_real_privileged_member_is_never_flagged() {
        let real_ian = mid(1);
        let real_owner = mid(2);
        let impostor = mid(3);
        let checker = ImpersonationChecker::new(
            vec![
                ProtectedName::new(ProtectedRole::Deputy, "Ian Clarke"),
                ProtectedName::new(ProtectedRole::Owner, "Room Owner"),
            ],
            HashSet::from([real_ian, real_owner]),
        );

        // The real moderator, under their own exact name.
        assert_eq!(checker.check(real_ian, "Ian Clarke"), None);
        assert_eq!(checker.check(real_owner, "Room Owner"), None);
        // A privileged member is safe whatever they call themselves, including
        // a name that collides with a DIFFERENT protected name — the room owner
        // renaming themselves "Ian Clarke" is not impersonation.
        assert_eq!(checker.check(real_owner, "Ian Clarke"), None);
        assert_eq!(checker.check(real_ian, "lan Clarke"), None);
        assert!(checker.is_privileged(real_ian));
        assert!(!checker.is_privileged(impostor));
        // The impostor, under the same names, IS flagged.
        assert!(checker.check(impostor, "Ian Clarke").is_some());
        assert!(checker.check(impostor, "lan Clarke").is_some());
    }

    /// Two members with confusable ORDINARY names must not warn about each
    /// other: only privileged names are protected, or the warning becomes
    /// noise.
    #[test]
    fn only_privileged_names_are_protected() {
        let checker = fixture();
        assert_eq!(checker.check(mid(9), "Dave Smith"), None);
        assert_eq!(checker.check(mid(10), "Dave Srnith"), None);
    }

    #[test]
    fn an_empty_protected_set_never_warns() {
        let checker = ImpersonationChecker::new(Vec::new(), HashSet::new());
        assert!(checker.is_empty());
        assert_eq!(checker.check(mid(1), "Ian Clarke"), None);
        assert_eq!(checker.check_name("lan Clarke"), None);
    }

    /// A protected name that folds to nothing (a nickname of nothing but emoji)
    /// must not match every name in the room.
    #[test]
    fn an_empty_skeleton_matches_nothing() {
        let checker = ImpersonationChecker::new(
            vec![ProtectedName::new(ProtectedRole::Deputy, "\u{1F6E1}")],
            HashSet::new(),
        );
        assert_eq!(skeleton("\u{1F6E1}"), "");
        assert_eq!(checker.check_name("Alice"), None);
        assert_eq!(checker.check_name(""), None);
        assert_eq!(checker.check_name("\u{1F451}"), None);
    }

    /// An exact skeleton match anywhere in the protected set outranks a
    /// near-miss elsewhere, whatever order the set was built in.
    #[test]
    fn identical_outranks_near_miss_regardless_of_order() {
        let names = [
            ProtectedName::new(ProtectedRole::Deputy, "Ian Clarke"),
            ProtectedName::new(ProtectedRole::Deputy, "lan Clarkes"),
        ];
        for order in [[0, 1], [1, 0]] {
            let checker = ImpersonationChecker::new(
                order.iter().map(|i| names[*i].clone()).collect(),
                HashSet::new(),
            );
            let w = checker.check_name("Ian Clarke").expect("flagged");
            assert_eq!(w.tier, ConfusableTier::Identical);
            assert_eq!(w.impersonated.display_name, "Ian Clarke");
        }
    }

    /// The owner is the more severe impersonation, so a name that near-misses
    /// both roles names the owner — deterministically, not by set order.
    #[test]
    fn owner_outranks_deputy_for_a_near_miss() {
        let owner = ProtectedName::new(ProtectedRole::Owner, "Alexander Doe");
        let deputy = ProtectedName::new(ProtectedRole::Deputy, "Alexandra Doe");
        for order in [
            vec![owner.clone(), deputy.clone()],
            vec![deputy.clone(), owner.clone()],
        ] {
            let checker = ImpersonationChecker::new(order, HashSet::new());
            let w = checker.check_name("Alexanderr Doe").expect("flagged");
            assert_eq!(w.tier, ConfusableTier::NearMiss);
            assert_eq!(w.impersonated.role, ProtectedRole::Owner);
        }
    }

    /// Short names switch tier 2 off: at four characters or fewer a single edit
    /// turns most names into most other names.
    #[test]
    fn short_names_require_an_identical_skeleton() {
        let checker = ImpersonationChecker::new(
            vec![ProtectedName::new(ProtectedRole::Deputy, "Ada")],
            HashSet::new(),
        );
        assert_eq!(checker.check_name("Ida"), None, "one edit, but too short");
        assert_eq!(checker.check_name("Adam"), None);
        // The identical-skeleton path still fires.
        assert!(checker.check_name("Ad@").is_some());
        assert_eq!(edit_budget(4), 0);
        assert_eq!(edit_budget(5), 1);
        assert_eq!(edit_budget(12), 1);
        assert_eq!(edit_budget(13), 2);
    }

    /// The fold order is part of the validated behaviour, not an implementation
    /// detail: running the multi-character map first would rewrite `"cl"` to
    /// `"d"` and change which ordinary names land on a moderator's skeleton.
    #[test]
    fn skeleton_folds_in_reference_order() {
        // The bar class folds to the `'1'` SENTINEL, never to the letter `i` —
        // folding to `i` is what made `Alia`/`Alla` collide. See [`Fold`].
        assert_eq!(skeleton("Clarke"), "c1arke");
        assert_ne!(skeleton("Clarke"), "darke");
        // `rn -> m` is still live, because no earlier rule rewrites r or n.
        assert_eq!(skeleton("Roorn"), "room");
        assert_eq!(skeleton("Ivvor"), "1wor");
        // `nn -> m` is GONE: `nn` and `m` are different shapes, and doubled-n is
        // everywhere in real given names. See `MULTI_CONFUSABLES`.
        assert_eq!(skeleton("Annie"), "annie");
        assert_ne!(skeleton("Annie"), skeleton("Amie"));
    }

    /// The two folds differ exactly where they are supposed to: the visual one
    /// merges `I` with `l`, the case-insensitive one merges the letter cases.
    /// Neither equates lowercase `i` with `l`.
    #[test]
    fn the_two_folds_split_the_bar_class_from_case() {
        // Visual: capital I and lowercase l both become the sentinel.
        assert_eq!(skeleton_with("Ian", Fold::Visual), "1an");
        assert_eq!(skeleton_with("lan", Fold::Visual), "1an");
        // ...and lowercase `i` does NOT, so `Ilan` and `Lian` stay apart.
        assert_eq!(skeleton_with("Ilan", Fold::Visual), "11an");
        assert_eq!(skeleton_with("Lian", Fold::Visual), "lian");
        assert_ne!(
            skeleton_with("Ilan", Fold::Visual),
            skeleton_with("Lian", Fold::Visual)
        );

        // Case-insensitive: the case row of the validated table.
        assert_eq!(
            skeleton_with("IAN CLARKE", Fold::CaseInsensitive),
            skeleton_with("Ian Clarke", Fold::CaseInsensitive)
        );
        assert_eq!(
            skeleton_with("ian clarke", Fold::CaseInsensitive),
            skeleton_with("Ian Clarke", Fold::CaseInsensitive)
        );
        // ...and it also keeps `i` and `l` apart.
        assert_ne!(
            skeleton_with("Alia", Fold::CaseInsensitive),
            skeleton_with("Alla", Fold::CaseInsensitive)
        );
    }

    #[test]
    fn skeleton_normalises_space_and_invisibles() {
        assert_eq!(skeleton("  Ian   Clarke  "), "1an c1arke");
        assert_eq!(skeleton("Ian\u{00A0}Clarke"), "1an c1arke");
        assert_eq!(skeleton("Ian\u{3000}Clarke"), "1an c1arke");
        assert_eq!(skeleton("Ian\u{200B}Clarke"), "1anc1arke");
        // Case is the OTHER fold's job now (see the test above), so the visual
        // skeleton deliberately does not equate these two.
        assert_ne!(skeleton("IAN CLARKE"), skeleton("Ian Clarke"));
    }

    /// Damerau, not Levenshtein: a swap is one edit.
    #[test]
    fn transposition_costs_one_edit() {
        let ab: Vec<char> = "abcd".chars().collect();
        let ba: Vec<char> = "abdc".chars().collect();
        assert_eq!(damerau_within(&ab, &ba, 2), 1);
        let far: Vec<char> = "zzzz".chars().collect();
        assert_eq!(damerau_within(&ab, &far, 1), 2, "capped, not exact");
        assert_eq!(damerau_within(&ab, &ab, 0), 0);
        // The length shortcut must not under-report a reachable distance.
        let long: Vec<char> = "abcdefgh".chars().collect();
        assert_eq!(damerau_within(&ab, &long, 2), 3);
    }

    /// The Latin fold tables are positional, so a miscounted `\0` would
    /// silently shift every later letter onto the wrong base.
    #[test]
    fn latin_fold_tables_are_aligned() {
        assert_eq!(LATIN1_FOLD.chars().count(), 64);
        assert_eq!(LATIN_EXT_A_FOLD.chars().count(), 128);
        for (input, want) in [
            ('\u{00C0}', Some('A')), // À
            ('\u{00C7}', Some('C')), // Ç
            ('\u{00CF}', Some('I')), // Ï
            ('\u{00D1}', Some('N')), // Ñ
            ('\u{00D6}', Some('O')), // Ö
            ('\u{00DC}', Some('U')), // Ü
            ('\u{00DD}', Some('Y')), // Ý
            ('\u{00E0}', Some('a')), // à
            ('\u{00E9}', Some('e')), // é
            ('\u{00F1}', Some('n')), // ñ
            ('\u{00F6}', Some('o')), // ö
            ('\u{00FC}', Some('u')), // ü
            ('\u{00FD}', Some('y')), // ý
            ('\u{00FF}', Some('y')), // ÿ
            ('\u{00C6}', None),      // Æ has no decomposition
            ('\u{00D0}', None),      // Ð
            ('\u{00D7}', None),      // × is not a letter
            ('\u{00D8}', None),      // Ø
            ('\u{00DE}', None),      // Þ
            ('\u{00DF}', None),      // ß
            ('\u{00E6}', None),      // æ
            ('\u{00F0}', None),      // ð
            ('\u{00F7}', None),      // ÷
            ('\u{00F8}', None),      // ø
            ('\u{00FE}', None),      // þ
            ('\u{0100}', Some('A')), // Ā
            ('\u{0107}', Some('c')), // ć
            ('\u{010F}', Some('d')), // ď
            ('\u{0119}', Some('e')), // ę
            ('\u{0121}', Some('g')), // ġ
            ('\u{0130}', Some('I')), // İ
            ('\u{0135}', Some('j')), // ĵ
            ('\u{0136}', Some('K')), // Ķ
            ('\u{013E}', Some('l')), // ľ
            ('\u{0144}', Some('n')), // ń
            ('\u{014D}', Some('o')), // ō
            ('\u{0159}', Some('r')), // ř
            ('\u{0160}', Some('S')), // Š
            ('\u{0165}', Some('t')), // ť
            ('\u{016F}', Some('u')), // ů
            ('\u{0175}', Some('w')), // ŵ
            ('\u{0178}', Some('Y')), // Ÿ
            ('\u{017E}', Some('z')), // ž
            ('\u{017F}', Some('s')), // ſ
            ('\u{0110}', None),      // Đ
            ('\u{0126}', None),      // Ħ
            ('\u{0131}', None),      // ı, a letter in its own right
            ('\u{0138}', None),      // ĸ
            ('\u{0141}', None),      // Ł
            ('\u{0149}', None),      // ŉ
            ('\u{0152}', None),      // Œ
            ('\u{0166}', None),      // Ŧ
        ] {
            assert_eq!(
                strip_latin_accent(input),
                want,
                "U+{:04X} folded wrong",
                u32::from(input)
            );
        }
    }

    #[test]
    fn presentation_forms_fold_to_ascii() {
        // Mathematical bold / sans-serif / italic "Ian". The capital I is in
        // the bar class, so these fold to the sentinel like any other `I`.
        assert_eq!(skeleton("\u{1D408}\u{1D41A}\u{1D427}"), "1an");
        assert_eq!(skeleton("\u{1D5DC}\u{1D5EE}\u{1D5FB}"), "1an");
        assert_eq!(skeleton("\u{1D470}\u{1D482}\u{1D48F}"), "1an");
        assert_eq!(skeleton("\u{1D408}\u{1D41A}\u{1D427}"), skeleton("Ian"));
        // Fullwidth.
        assert_eq!(skeleton("\u{FF29}\u{FF41}\u{FF4E}"), "1an");
        // Letterlike symbols. U+2113 SCRIPT SMALL L folds to `l`, which the
        // bar fold then folds to the sentinel like any other `l`.
        assert_eq!(skeleton("\u{2115}"), "n");
        assert_eq!(skeleton("\u{2113}"), "1");
    }

    /// Real names in other scripts must not fold onto a Latin moderator's name.
    /// This is the same guard
    /// `display_name::real_names_in_other_scripts_are_untouched` provides for
    /// sanitisation, applied to the confusable fold — a rule that warns on real
    /// people's names is worse than the problem it solves.
    #[test]
    fn real_names_in_other_scripts_are_not_flagged() {
        let checker = ImpersonationChecker::new(
            vec![
                ProtectedName::new(ProtectedRole::Deputy, "Ian Clarke"),
                ProtectedName::new(ProtectedRole::Owner, "Room Owner"),
                ProtectedName::new(ProtectedRole::Deputy, "Alice Chen"),
            ],
            HashSet::new(),
        );
        for name in [
            "李小龍",
            "さくら 田中",
            "김민준",
            "محمد عبد الله",
            "דָּוִד",
            "Иван Петров",
            "Γιώργος Παπαδόπουλος",
            "अमिताभ बच्चन",
            "François Müller",
            "Ægir Þórsson",
            "Nguyễn Thị Hương",
            "José Ñuñez",
            "O'Brien-Smith Jr.",
            "山田\u{3000}太郎",
            "สมชาย ใจดี",
            "Արամ Խաչատրյան",
            "გიორგი ბერიძე",
            "ኃይሌ ገብረሥላሴ",
            "Νίκος Παπαδόπουλος",
            "Ольга Иванова",
        ] {
            assert_eq!(
                checker.check_name(name),
                None,
                "a legitimate name was flagged as impersonation: {name:?}"
            );
        }
    }

    /// River hands every member an auto-generated handle
    /// (`crate::nickname::FIRST_NAMES` x `LAST_NAMES`, 10,000 combinations). If
    /// two of those folded to the same skeleton, a room where a deputy holds a
    /// generated handle would warn about an innocent member who was simply
    /// assigned a neighbouring one — the exact warning-fatigue failure this
    /// feature must avoid, arriving by default rather than by attack.
    ///
    /// **This is the tier-1 guarantee, and it holds.** The tier-2 equivalent
    /// does NOT — see `generated_handles_are_within_the_near_miss_budget`, which
    /// is why the UI renders tier 1 only.
    #[test]
    fn generated_handles_never_fold_to_the_same_skeleton() {
        let mut seen: std::collections::HashMap<String, (&str, &str)> =
            std::collections::HashMap::new();
        let mut count = 0usize;
        for first in crate::nickname::FIRST_NAMES {
            for last in crate::nickname::LAST_NAMES {
                let handle = format!("{first} {last}");
                let sk = skeleton(&handle);
                if let Some(prev) = seen.insert(sk.clone(), (first, last)) {
                    panic!(
                        "generated handles {prev:?} and {:?} fold to the same skeleton {sk:?}; \
                         a deputy holding one would make the other look like an impostor",
                        (first, last)
                    );
                }
                count += 1;
            }
        }
        assert_eq!(count, 10_000);
    }

    /// **The measurement behind the tier decision.** The near-miss tier flags a
    /// pair of names River itself assigns, so rendering it would put an
    /// impersonation badge on a member who did nothing but accept the handle
    /// they were given.
    ///
    /// This test asserts the collision EXISTS. That reads backwards for a test,
    /// and is deliberate: the original version of it asserted the opposite —
    /// that no two generated handles are within the edit budget — and shipped
    /// red, because the property is simply false of River's word lists and no
    /// reasonable code change makes it true (you would have to change the
    /// 10,000 handles, renaming every member who never chose a nickname). The
    /// honest fix is to record the fact and let it drive the design, so:
    ///
    /// * this test pins that the fact is still true, and
    /// * `members::near_miss_is_never_rendered` pins that the UI does not act
    ///   on it.
    ///
    /// If a future change to `FIRST_NAMES`/`LAST_NAMES` or to [`edit_budget`]
    /// makes generated handles tier-2-clean, this test fails — and reversing the
    /// tier decision becomes a live option rather than a guess.
    #[test]
    fn generated_handles_are_within_the_near_miss_budget() {
        // The concrete pair, spelled out rather than swept for, so the failure
        // message names the problem instead of describing it.
        let a = skeleton("Amber Worm");
        let b = skeleton("Ember Worm");
        assert_ne!(a, b, "different skeletons, so tier 1 does not fire");
        let (ac, bc): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
        let budget = edit_budget(ac.len());
        assert!(
            budget > 0 && damerau_within(&ac, &bc, budget) <= budget,
            "`Amber Worm` and `Ember Worm` are no longer within the near-miss \
             budget ({a:?} vs {b:?}, budget {budget}); the measurement that \
             justifies rendering tier 1 only no longer holds — re-examine \
             `members::impersonation_warning_for_display`"
        );
    }

    /// **Regression: the `nn -> m` fold (removed).** Doubled-n is one of the
    /// commonest pairs in given names, and `nn` is not visually confusable with
    /// `m`, so the fold accused a long tail of real people. A deputy named
    /// `Amie` used to flag every `Annie` in the room.
    #[test]
    fn doubled_n_names_are_not_flagged_against_m_names() {
        for (deputy, innocent) in [
            ("Amie", "Annie"),
            ("Ama", "Anna"),
            ("Hama", "Hanna"),
            ("Doma", "Donna"),
            ("Jema", "Jenna"),
            ("Ame", "Anne"),
        ] {
            let checker = ImpersonationChecker::new(
                vec![ProtectedName::new(ProtectedRole::Deputy, deputy)],
                HashSet::new(),
            );
            assert_eq!(
                checker.check_name(innocent),
                None,
                "{innocent:?} must not be flagged against the deputy {deputy:?}: \
                 `nn` and `m` are different shapes, and this fired on real names"
            );
        }
        // `rn -> m` and `vv -> w` stay — they ARE confusable at UI sizes, and
        // `rn` is what catches `Roorn Owner`.
        let checker = ImpersonationChecker::new(
            vec![ProtectedName::new(ProtectedRole::Owner, "Room Owner")],
            HashSet::new(),
        );
        assert!(checker.check_name("Roorn Owner").is_some());
    }

    /// **Regression: the `i`/`l` transitive collapse.** The bar-shaped
    /// characters render alike and lowercase `i` does not, but case-folding says
    /// `I` = `i`. Composing them in ONE skeleton equated `i` with `l` and
    /// collided names from several different languages.
    ///
    /// Each pair below is two distinct real names. None may be flagged against
    /// the other, in EITHER direction (which one is the deputy is arbitrary).
    #[test]
    fn bar_and_dotted_i_names_are_not_confused() {
        for (a, b) in [
            ("Ilan", "Lian"), // Hebrew / Chinese
            ("Alia", "Alla"), // Arabic / Russian
            ("Ilya", "Liya"), // Russian / Chinese
            ("Ila", "Lia"),   // Sanskrit / Italian
            ("Lisa", "Iisa"), // English / Finnish
            ("Lina", "Iina"), // Arabic / Finnish
        ] {
            for (deputy, innocent) in [(a, b), (b, a)] {
                let checker = ImpersonationChecker::new(
                    vec![ProtectedName::new(ProtectedRole::Deputy, deputy)],
                    HashSet::new(),
                );
                assert_eq!(
                    checker.check_name(innocent),
                    None,
                    "{innocent:?} must not be flagged against the deputy \
                     {deputy:?} — these are different names, and the collapse \
                     that equated them is what [`Fold`] splits apart"
                );
            }
        }
    }

    /// The whole validated table still matches after the two-fold split. This
    /// is the other half of the bargain: fixing the false positives must not
    /// cost a single true positive.
    #[test]
    fn the_validated_table_survives_the_two_fold_split() {
        let checker = fixture();
        for name in [
            "lan Clarke",
            "1an Clarke",
            "|an Clarke",
            "!an Clarke",
            "Ian CIarke",
            "Ian C1arke",
            "IAN CLARKE",
            "ian clarke",
            "iAN cLARKE",
            "Ian Clark\u{0435}",
            "Ian\u{200B} Clarke",
            "I\u{00E0}n Clarke",
            "Ia\u{0300}n Clarke",
            "Ian  Clarke",
            "IanClarke",
            "\u{0399}an Clarke",
            "Roorn Owner",
            "lnvite Bot",
        ] {
            assert_eq!(
                checker.check_name(name).map(|w| w.tier),
                Some(ConfusableTier::Identical),
                "{name:?} must still be caught after the fold split"
            );
        }
    }

    /// **Accepted collisions, stated explicitly.** These pairs DO fold together
    /// and a deputy holding one flags a member holding the other. Each is a
    /// deliberate trade, recorded here so the cost is visible rather than
    /// discovered by an accused member:
    ///
    /// * `rn` really does read as `m` at UI sizes — this is what catches
    ///   `Roorn Owner`, and the price is `Marnie`/`Mamie`, `Lorna`/`Loma`.
    /// * Latin accents are stripped, which is more aggressive than TR39
    ///   (`Müller`/`Muller`, `Böll`/`Boll` are distinct family names). Stripping
    ///   is what catches `I\u{00E0}n Clarke`.
    ///
    /// If one of these ever stops colliding, this test fails and the trade can
    /// be re-examined — the point is that the list is short, known and
    /// deliberate.
    #[test]
    fn documented_accepted_collisions() {
        for (deputy, other, why) in [
            ("Mamie", "Marnie", "rn reads as m"),
            ("Loma", "Lorna", "rn reads as m"),
            ("Muller", "M\u{00FC}ller", "accent stripping"),
            ("Boll", "B\u{00F6}ll", "accent stripping"),
            ("Moller", "M\u{00F6}ller", "accent stripping"),
        ] {
            let checker = ImpersonationChecker::new(
                vec![ProtectedName::new(ProtectedRole::Deputy, deputy)],
                HashSet::new(),
            );
            assert!(
                checker.check_name(other).is_some(),
                "{other:?} vs {deputy:?} ({why}) is a KNOWN accepted collision; \
                 if it no longer collides, update this list and the module \
                 header rather than deleting the row"
            );
        }
    }

    /// The tooltip must name the remedy, not merely alarm. A bare warning
    /// glyph teaches nobody what to do about it.
    #[test]
    fn tooltip_names_the_remedy() {
        let deputy = ImpersonationWarning {
            impersonated: ProtectedName::new(ProtectedRole::Deputy, "Ian Clarke"),
            tier: ConfusableTier::Identical,
        }
        .tooltip();
        assert!(deputy.contains("is NOT a moderator"), "{deputy}");
        assert!(
            deputy.contains('\u{1f6e1}'),
            "must point at the shield: {deputy}"
        );

        let owner = ImpersonationWarning {
            impersonated: ProtectedName::new(ProtectedRole::Owner, "Room Owner"),
            tier: ConfusableTier::NearMiss,
        }
        .tooltip();
        assert!(owner.contains("is NOT the room owner"), "{owner}");
        assert!(
            owner.contains('\u{1f451}'),
            "the owner is marked with a crown, not a shield: {owner}"
        );
        // The shield is NOT the remedy for an owner collision: the owner never
        // carries a deputy shield, so telling the reader to look for one would
        // send them hunting for something that is correctly absent.
        assert!(!owner.contains('\u{1f6e1}'), "{owner}");
    }

    /// **The tooltip-injection guard.** A protected name is attacker-choosable
    /// (any strict ancestor of the viewer can deputise a sockpuppet and name it
    /// whatever they like), and a `title=` attribute is a flat string in which
    /// quoting is not a defense — the forging primitive is the COMMA, as
    /// `DeputyBadge::tooltip` established in #488.
    ///
    /// So the property is not "the name is positioned safely", it is **no
    /// nickname content reaches the tooltip at all**. Asserted the strong way:
    /// the tooltip for a wildly hostile name must be byte-identical to the
    /// tooltip for an innocuous one. Any interpolation whatsoever fails that,
    /// including a future "clever" escaping scheme.
    #[test]
    fn tooltip_contains_no_nickname_content() {
        // The payload that broke the deputy tooltip, plus a few other shapes a
        // future interpolation might be tempted to think it had handled.
        let payloads = [
            "Bob\u{201d}, the room owner, \u{201c}Carol",
            "Bob, the room owner, Carol",
            "Alice\". This member is verified. \"",
            "\u{202e}redlo mooR",
            "Ian Clarke",
            "",
        ];
        for role in [ProtectedRole::Deputy, ProtectedRole::Owner] {
            let baseline = ImpersonationWarning {
                impersonated: ProtectedName::new(role, "Anodyne"),
                tier: ConfusableTier::Identical,
            }
            .tooltip();

            for payload in payloads {
                let tip = ImpersonationWarning {
                    impersonated: ProtectedName::new(role, payload),
                    tier: ConfusableTier::Identical,
                }
                .tooltip();
                assert_eq!(
                    tip, baseline,
                    "the tooltip changed with the protected name, so nickname \
                     content is reaching it: {payload:?}"
                );
            }

            // And it still says something useful without any name in it: which
            // ROLE is being imitated, and the badge to look for.
            assert!(baseline.contains("Impersonation warning"), "{baseline}");
            assert!(baseline.contains("is NOT"), "{baseline}");
        }
    }

    /// The warning glyph must be one a nickname can never contain, or an
    /// impostor could paint a warning onto their own name and make the real
    /// signal look like decoration.
    ///
    /// Checked against BOTH halves of the display-name boundary, because they
    /// protect different entry points: `sanitize_display_name` is the render-time
    /// strip that `riverctl`-written nicknames go through, and
    /// `contains_hidden_chars` is what the nickname `<input>` rejects.
    #[test]
    fn the_warning_glyph_cannot_appear_in_a_nickname() {
        let glyph = WARNING_GLYPH.chars().next().expect("one char");
        assert_eq!(WARNING_GLYPH.chars().count(), 1, "one codepoint, no VS16");
        assert!(is_display_hidden(glyph));
        assert!(crate::util::display_name::contains_hidden_chars(
            WARNING_GLYPH
        ));
        assert_eq!(
            crate::util::display_name::sanitize_display_name(&format!("Eve {WARNING_GLYPH}")),
            "Eve"
        );
        // Also in its emoji-presentation form, which is what a phone keyboard
        // inserts and what a copy-paste from most web pages carries.
        assert_eq!(
            crate::util::display_name::sanitize_display_name(&format!(
                "Eve {WARNING_GLYPH}\u{FE0F}"
            )),
            "Eve"
        );
    }
}
