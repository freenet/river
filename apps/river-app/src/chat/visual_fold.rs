//! The visual fold behind `label_claimed_ids` (see `links.rs`): what a reader SEES of a label.
//!
//! Each item below is a VERBATIM copy from the web UI (`ui/src/util/confusable.rs` and
//! `ui/src/util/display_name.rs`), taken because those files cannot be compiled here whole
//! (their tests and impersonation checker need the web crate). `tests::copies_match_the_web_ui`
//! fails when a copied item drifts from, or disappears out of, the web source. To update:
//! re-copy the changed item. The `//@@` marker lines name each item's source file.
#![allow(dead_code, clippy::all)]

/// Whether two display names are a tier-1 (same skeleton) match: the web UI's own
/// `folds_together`, copied below.
pub(crate) fn names_fold_together(a: &str, b: &str) -> bool {
    folds_together(a, b)
}

/// Whether `name` folds to one of the handles River generates for a member with no nickname,
/// which the web UI never protects (`folds_to_generated_handle`, copied below).
pub(crate) fn names_fold_to_generated_handle(name: &str) -> bool {
    folds_to_generated_handle(name)
}

//@@ confusable.rs
/// Combining marks, dropped so `"I" + U+0300` folds like `"Ì"`.
fn is_combining_mark(c: char) -> bool {
    matches!(u32::from(c),
        // **The Cyrillic combining marks, ALL of them.** U+0483..U+0489 was
        // added because `"Ian Cla\u{0483}rke"` EVADED while `U+0300` was
        // caught, and the rest of the same family was left behind — so
        // `"Ian Cla\u{A66F}rke"` (VZMET, which sits immediately before the
        // U+A670 range `is_display_hidden` already strips) evaded in exactly
        // the same way. Closing a family one codepoint at a time is how the
        // hole reopens; these are now the complete Mn/Me Cyrillic set:
        // U+0483..U+0489, the Combining Cyrillic Letters block
        // (U+2DE0..U+2DFF), and U+A66F plus U+A674..U+A67D.
        //
        // They are historic and liturgical ornaments and superscript letters,
        // not part of how any modern Cyrillic name is spelled, so folding them
        // is safe.
        //
        // Hebrew niqqud, Arabic harakat and Indic/Thai vowel signs are still
        // NOT here, deliberately: in those scripts marks are far more often
        // meaning-bearing, and widening the strip would start flagging real
        // people in scripts this repo has no corpus for. See the module header.
        0x0483..=0x0489
        | 0x2DE0..=0x2DFF // Combining Cyrillic Letters
        | 0xA66F | 0xA674..=0xA67D // vzmet, combining Cyrillic letters, kavyka, payerok
        | 0x0300..=0x036F   // Combining Diacritical Marks
        | 0x1AB0..=0x1AFF // Combining Diacritical Marks Extended
        | 0x1DC0..=0x1DFF // Combining Diacritical Marks Supplement
        | 0x20D0..=0x20F0 // Combining Diacritical Marks for Symbols
        | 0xFE20..=0xFE2F // Combining Half Marks
    )
}

//@@ confusable.rs
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
    if let Some(letter) = fold_nfkc_letter(c) {
        return Some(letter);
    }
    // Mathematical Alphanumeric Symbols, LATIN half: thirteen consecutive
    // 52-letter (A-Z then a-z) blocks, ending at U+1D6A3.
    //
    // **The upper bound is load-bearing.** This used to run to U+1D7CB, and the
    // 52-alignment simply is not true past U+1D6A3: what follows is two dotless
    // letters and then FIVE GREEK blocks of 58, so `% 52` walked out of phase
    // and emitted an arbitrary letter. U+1D75E MATH SANS BOLD CAPITAL IOTA came
    // out as `e`, which meant `𝝞𝗮𝗻 𝗖𝗹𝗮𝗿𝗸𝗲` — pixel-identical to the
    // `𝗜𝗮𝗻 𝗖𝗹𝗮𝗿𝗸𝗲` the test corpus already caught, one codepoint different —
    // walked straight past the check. It was wrong in the other direction too:
    // bold capital Alpha folded to `E`, a false positive on a real Greek name
    // rendered in a fancy-text generator.
    //
    // The range still contains reserved holes (e.g. U+1D455, whose letter lives
    // in Letterlike Symbols above). A hole maps to a letter here, which is
    // harmless: an unassigned codepoint renders as tofu and cannot appear in a
    // name a reader would mistake for anything.
    if (0x1D400..=0x1D6A3).contains(&cp) {
        let idx = (cp - 0x1D400) % 52;
        return Some(if idx < 26 {
            (b'A' + idx as u8) as char
        } else {
            (b'a' + (idx - 26) as u8) as char
        });
    }
    // The two dotless letters that sit between the Latin and Greek halves.
    // U+0131 is the Turkish dotless i the homoglyph table already carries (with
    // its cost recorded in `documented_accepted_collisions`), so routing through
    // it keeps one decision in one place.
    if cp == 0x1D6A4 {
        return Some('\u{0131}');
    }
    if cp == 0x1D6A5 {
        return Some('j');
    }
    // Mathematical Alphanumeric Symbols, GREEK half: five consecutive blocks of
    // 58, all with the identical slot layout (24 capitals, nabla, 25 lowercase,
    // partial differential, then six variant symbols).
    //
    // Each slot folds to the PLAIN GREEK letter, not to a Latin one. Whether
    // that letter then becomes Latin is `fold_homoglyph`'s decision, made once
    // for Greek as a whole and already case-aware — so `𝝞` reaches `Ι` reaches
    // `I`, while `𝝘` reaches `Γ` and stays Greek, because Gamma is not a Latin
    // lookalike. Mapping these to Latin here instead would duplicate that
    // judgement in a second place and let the two drift.
    if (0x1D6A8..=0x1D7C9).contains(&cp) {
        let slot = ((cp - 0x1D6A8) % 58) as usize;
        return MATH_GREEK_FOLD.chars().nth(slot);
    }
    // The two bold digammas that close the block. Folded to plain digamma,
    // which no table turns into Latin `F` — the letterform differs enough, and
    // nothing in a name needs it.
    if cp == 0x1D7CA {
        return Some('\u{03DC}');
    }
    if cp == 0x1D7CB {
        return Some('\u{03DD}');
    }
    // Mathematical digits: five consecutive blocks of ten.
    if (0x1D7CE..=0x1D7FF).contains(&cp) {
        return Some((b'0' + ((cp - 0x1D7CE) % 10) as u8) as char);
    }
    None
}

//@@ confusable.rs
/// Small-capital letters, folded to the capital they are drawn as.
///
/// `ɪᴀɴ ᴄʟᴀʀᴋᴇ` reads as `IAN CLARKE` to anyone looking at it, and the module
/// already asserts that `IAN CLARKE` is an identical skeleton of `Ian Clarke`,
/// so by its own standard these have to fold too. Every one of them survived
/// both folds verbatim before this: they are general category `Ll`, so the case
/// step leaves them alone, and the result was edit distance 8 from the real
/// name — not even a tier-2 near miss. Small-caps generators are one click away
/// on any "fancy text" page, and every one of these renders in DejaVu Sans,
/// Noto Sans, Segoe UI and the macOS system fonts.
///
/// ## What is deliberately NOT here
///
/// Only the letters whose base is a plain ASCII capital. Everything with a
/// stroke, hook, belt, leg, or a turned/reversed/inverted orientation
/// (`ᴃ ᴌ ᴎ ʁ ᴙ ᴚ ᵻ ᵾ`), and the digraphs (`ᴁ ᴓ ꝶ`), are separate letterforms a
/// reader does not see as the plain capital, so folding them would be inventing
/// a confusable rather than recording one. There is no small capital X in
/// Unicode; generators emit plain `x`, which needs nothing here.
///
/// The Greek and Cyrillic small capitals fold to their PLAIN Greek/Cyrillic
/// letter, not to Latin — whether Greek `Ρ` becomes Latin `P` is
/// [`fold_homoglyph`]'s decision, made once, and duplicating it here is how the
/// two tables drift apart.
///
/// False-positive surface is small by construction: these are IPA and phonetic
/// notation, not letters any living orthography spells a personal name with.
/// The corpus sweeps cover them like everything else.
fn fold_small_capital(c: char) -> Option<char> {
    Some(match u32::from(c) {
        0x0262 => 'G',
        0x026A => 'I',
        0x0274 => 'N',
        0x0280 => 'R',
        0x028F => 'Y',
        0x0299 => 'B',
        0x029C => 'H',
        0x029F => 'L',
        0x1D00 => 'A',
        0x1D04 => 'C',
        0x1D05 => 'D',
        0x1D07 => 'E',
        0x1D0A => 'J',
        0x1D0B => 'K',
        0x1D0D => 'M',
        0x1D0F => 'O',
        0x1D18 => 'P',
        0x1D1B => 'T',
        0x1D1C => 'U',
        0x1D20 => 'V',
        0x1D21 => 'W',
        0x1D22 => 'Z',
        0xA730 => 'F',
        0xA731 => 'S',
        0xA7AE => 'I', // LATIN CAPITAL LETTER SMALL CAPITAL I
        0xA7AF => 'Q',
        // Greek and Cyrillic small capitals, to their own script's capital.
        0x1D26 => '\u{0393}', // Γ
        0x1D27 => '\u{039B}', // Λ
        0x1D28 => '\u{03A0}', // Π
        0x1D29 => '\u{03A1}', // Ρ — becomes Latin P via `fold_homoglyph`
        0x1D2A => '\u{03A8}', // Ψ
        0xAB65 => '\u{03A9}', // Ω
        0x1D2B => '\u{041B}', // Л
        _ => return None,
    })
}

//@@ confusable.rs
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
        // ---- Shape homoglyphs that NFKC does NOT equate ----
        //
        // Unlike `fold_nfkc_letter`, these are NOT normalisation-equal to their
        // Latin counterparts — they are separate letters that happen to be
        // drawn the same. So they carry REAL false-positive surface and each is
        // a judgement, not a lookup. Every script here is covered by
        // `real_names_in_other_scripts_are_not_flagged` and by the corpus sweep
        // in `foldable_scripts_do_not_collide_with_latin_names`.
        //
        // Greek lunate sigma: the epigraphic forms of sigma, drawn exactly as
        // Latin C/c.
        '\u{03F9}' => 'C',
        '\u{03F2}' => 'c',
        // Coptic. These capitals are drawn as their Latin lookalikes in every
        // Coptic font, and the lowercase form sits at +1 — so the two are
        // folded as PAIRS. Four lowercase letters were missing while their
        // capitals were folded (`\u{2C89}`, `\u{2C93}`, `\u{2C95}`,
        // `\u{2C9B}`), which is a half-closed block rather than a decision.
        // The entries UTR39 lists and this table did not are here too.
        '\u{2C80}' => 'A',
        '\u{2C81}' => 'a',
        '\u{2C82}' => 'B',
        '\u{2C83}' => 'b',
        '\u{2C85}' => 'r', // small gamma; the CAPITAL is Γ-shaped and is not here
        '\u{2C88}' => 'E',
        '\u{2C89}' => 'e',
        '\u{2C8E}' => 'H',
        '\u{2C8F}' => 'h',
        '\u{2C92}' => 'I',
        '\u{2C93}' => 'i',
        '\u{2C94}' => 'K',
        '\u{2C95}' => 'k',
        '\u{2C98}' => 'M',
        '\u{2C99}' => 'm',
        '\u{2C9A}' => 'N',
        '\u{2C9B}' => 'n',
        '\u{2C9E}' => 'O',
        '\u{2C9F}' => 'o',
        '\u{2CA2}' => 'P',
        '\u{2CA3}' => 'p',
        '\u{2CA4}' => 'C',
        '\u{2CA5}' => 'c',
        '\u{2CA6}' => 'T',
        '\u{2CA7}' => 't',
        '\u{2CA8}' => 'Y',
        '\u{2CA9}' => 'y',
        '\u{2CAC}' => 'X',
        '\u{2CAD}' => 'x',
        '\u{2CBD}' => 'w',
        '\u{2CCE}' => 'P',
        '\u{2CCF}' => 'p',
        '\u{2CD0}' => 'L',
        '\u{2CD1}' => 'l',
        // Armenian. ONLY these two: `\u{0585}` and `\u{0578}` are drawn as
        // Latin `o`/`n`, and BOTH occur in real Armenian names
        // (`\u{0540}\u{578}\u{57E}\u{570}\u{561}\u{576}\u{576}\u{565}\u{57D}`,
        // `\u{0533}\u{578}\u{057C}`), which makes this the highest-risk entry
        // in the file. It is safe only because a collision needs the WHOLE name
        // to fold and every other Armenian letter stays itself — pinned by the
        // corpus sweep, not by assertion.
        '\u{0585}' => 'o',
        '\u{0578}' => 'n',
        // ---- Lisu (the Fraser alphabet) ----
        //
        // Designed FROM Latin capitals, so its letters are Latin letterforms
        // outright — which is why folding two of them (`I` and `W`) and
        // leaving the other 24 made no sense: the justification applied to the
        // whole block, and `ꓲꓮꓠ ꓚꓡꓮꓣꓗꓰ` spelled the moderator's name in
        // characters nothing folded.
        //
        // **Every mapping below is UTR39 `confusables.txt` (17.0), not a
        // guess.** That matters here: several Fraser letters are ROTATED or
        // MIRRORED Latin capitals rather than upright ones, and eyeballing the
        // chart gets those wrong in both directions. UTR39 lists the upright
        // ones; the rotated forms (`ꓒ ꓕ ꓘ ꓛ ꓞ ꓤ ꓥ ꓨ ꓩ ꓭ ꓯ ꓱ ꓵ ꓶ ꓷ`) are
        // absent from it and stay unfolded here.
        '\u{A4D0}' => 'B',
        '\u{A4D1}' => 'P',
        '\u{A4D2}' => 'd',
        '\u{A4D3}' => 'D',
        '\u{A4D4}' => 'T',
        '\u{A4D6}' => 'G',
        '\u{A4D7}' => 'K',
        '\u{A4D9}' => 'J',
        '\u{A4DA}' => 'C',
        '\u{A4DC}' => 'Z',
        '\u{A4DD}' => 'F',
        '\u{A4DF}' => 'M',
        '\u{A4E0}' => 'N',
        '\u{A4E1}' => 'L',
        '\u{A4E2}' => 'S',
        '\u{A4E3}' => 'R',
        '\u{A4E6}' => 'V',
        '\u{A4E7}' => 'H',
        '\u{A4EA}' => 'W',
        '\u{A4EB}' => 'X',
        '\u{A4EC}' => 'Y',
        '\u{A4EE}' => 'A',
        '\u{A4F0}' => 'E',
        '\u{A4F2}' => 'I',
        '\u{A4F3}' => 'O',
        '\u{A4F4}' => 'U',
        // ---- Cherokee ----
        //
        // Same story: one letter was folded and the other 34 were not. The
        // syllabary is a living script, so this is the widest false-positive
        // surface added here — but a Cherokee name is written in SYLLABLES, so
        // the Latin string it folds to is not a name in any language, and a
        // badge needs the whole thing to land on a protected name. Sourced
        // from UTR39; the letters absent from that file stay unfolded.
        '\u{13A0}' => 'D',
        '\u{13A1}' => 'R',
        '\u{13A2}' => 'T',
        '\u{13A5}' => 'i',
        '\u{13A9}' => 'Y',
        '\u{13AA}' => 'A',
        '\u{13AB}' => 'J',
        '\u{13AC}' => 'E',
        '\u{13B3}' => 'W',
        '\u{13B7}' => 'M',
        '\u{13BB}' => 'H',
        '\u{13BD}' => 'Y',
        '\u{13C0}' => 'G',
        '\u{13C2}' => 'h',
        '\u{13C3}' => 'Z',
        '\u{13CF}' => 'b',
        '\u{13D2}' => 'R',
        '\u{13D4}' => 'W',
        '\u{13D5}' => 'S',
        '\u{13D9}' => 'V',
        '\u{13DA}' => 'S',
        '\u{13DE}' => 'L',
        '\u{13DF}' => 'C',
        '\u{13E2}' => 'P',
        '\u{13E6}' => 'K',
        '\u{13E7}' => 'd',
        '\u{13F3}' => 'G',
        '\u{13F4}' => 'B',
        '\u{AB75}' => 'i',
        '\u{AB81}' => 'r',
        '\u{AB83}' => 'w',
        '\u{AB93}' => 'z',
        '\u{ABA9}' => 'v',
        '\u{ABAA}' => 's',
        '\u{ABAF}' => 'c',
        // Latin-block letters that are not the ASCII letter they resemble:
        // `\u{01C0}` is a click consonant, `\u{0251}` the IPA script-a, and
        // `\u{0131}` is Turkish dotless i — a REAL letter in Turkish names,
        // folded anyway because a reader outside Turkish sees one glyph minus a
        // dot. That cost is recorded in `documented_accepted_collisions`.
        '\u{01C0}' => 'l',
        '\u{0251}' => 'a',
        '\u{0131}' => 'i',
        // Mathematical DIVIDES: a bare vertical stroke.
        '\u{2223}' => 'l',
        // Greek, lowercase — only the three that are genuinely
        // indistinguishable.
        '\u{03BF}' => 'o', // ο
        '\u{03C1}' => 'p', // ρ
        '\u{03BD}' => 'v', // ν
        other => other,
    }
}

//@@ confusable.rs
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
        0x0180..=0x024F => table_char(LATIN_EXT_B_FOLD, 0x0180),
        0x1E00..=0x1EFF => table_char(LATIN_EXT_ADD_FOLD, 0x1E00),
        _ => None,
    }
}

//@@ confusable.rs
/// Characters whose Unicode NFKC normalisation IS a single ASCII letter, and
/// which survive [`is_display_hidden`].
///
/// **This is the low-risk half of the confusable table and it is GENERATED, not
/// curated.** Every entry is one a conforming NFKC implementation already
/// treats as equal to its ASCII letter, so folding them adds no false-positive
/// surface: none is a letter in any living orthography's normal spelling of a
/// personal name. They are modifier letters (linguistics/IPA), super/subscripts,
/// Roman numerals, ordinal indicators, the Kelvin and Angstrom signs, and the
/// double-struck italics.
///
/// Without them the bypasses are trivial and total — Roman numerals render as
/// plain Latin, so `U+2160 U+216D U+217C` spells `Ian Clarke` to any reader.
///
/// Produced by sweeping `0x80..0x30000` for `NFKC(c)` being one ASCII letter
/// and subtracting what the other tables already fold; regenerate the same way
/// if those ranges change. `roman_numerals_and_nfkc_letters_fold` pins a
/// representative of each family.
///
/// Contrast [`fold_homoglyph`]'s shape entries, which NFKC does NOT equate and
/// which therefore carry real false-positive risk.
fn fold_nfkc_letter(c: char) -> Option<char> {
    Some(match u32::from(c) {
        // 88 characters, generated from Unicode NFKC data.
        0x00AA => 'a', // FEMININE ORDINAL INDICATOR
        0x00BA => 'o', // MASCULINE ORDINAL INDICATOR
        0x02B0 => 'h', // MODIFIER LETTER SMALL H
        0x02B2 => 'j', // MODIFIER LETTER SMALL J
        0x02B3 => 'r', // MODIFIER LETTER SMALL R
        0x02B7 => 'w', // MODIFIER LETTER SMALL W
        0x02B8 => 'y', // MODIFIER LETTER SMALL Y
        0x02E1 => 'l', // MODIFIER LETTER SMALL L
        0x02E2 => 's', // MODIFIER LETTER SMALL S
        0x02E3 => 'x', // MODIFIER LETTER SMALL X
        0x1D2C => 'A', // MODIFIER LETTER CAPITAL A
        0x1D2E => 'B', // MODIFIER LETTER CAPITAL B
        0x1D30 => 'D', // MODIFIER LETTER CAPITAL D
        0x1D31 => 'E', // MODIFIER LETTER CAPITAL E
        0x1D33 => 'G', // MODIFIER LETTER CAPITAL G
        0x1D34 => 'H', // MODIFIER LETTER CAPITAL H
        0x1D35 => 'I', // MODIFIER LETTER CAPITAL I
        0x1D36 => 'J', // MODIFIER LETTER CAPITAL J
        0x1D37 => 'K', // MODIFIER LETTER CAPITAL K
        0x1D38 => 'L', // MODIFIER LETTER CAPITAL L
        0x1D39 => 'M', // MODIFIER LETTER CAPITAL M
        0x1D3A => 'N', // MODIFIER LETTER CAPITAL N
        0x1D3C => 'O', // MODIFIER LETTER CAPITAL O
        0x1D3E => 'P', // MODIFIER LETTER CAPITAL P
        0x1D3F => 'R', // MODIFIER LETTER CAPITAL R
        0x1D40 => 'T', // MODIFIER LETTER CAPITAL T
        0x1D41 => 'U', // MODIFIER LETTER CAPITAL U
        0x1D42 => 'W', // MODIFIER LETTER CAPITAL W
        0x1D43 => 'a', // MODIFIER LETTER SMALL A
        0x1D47 => 'b', // MODIFIER LETTER SMALL B
        0x1D48 => 'd', // MODIFIER LETTER SMALL D
        0x1D49 => 'e', // MODIFIER LETTER SMALL E
        0x1D4D => 'g', // MODIFIER LETTER SMALL G
        0x1D4F => 'k', // MODIFIER LETTER SMALL K
        0x1D50 => 'm', // MODIFIER LETTER SMALL M
        0x1D52 => 'o', // MODIFIER LETTER SMALL O
        0x1D56 => 'p', // MODIFIER LETTER SMALL P
        0x1D57 => 't', // MODIFIER LETTER SMALL T
        0x1D58 => 'u', // MODIFIER LETTER SMALL U
        0x1D5B => 'v', // MODIFIER LETTER SMALL V
        0x1D62 => 'i', // LATIN SUBSCRIPT SMALL LETTER I
        0x1D63 => 'r', // LATIN SUBSCRIPT SMALL LETTER R
        0x1D64 => 'u', // LATIN SUBSCRIPT SMALL LETTER U
        0x1D65 => 'v', // LATIN SUBSCRIPT SMALL LETTER V
        0x1D9C => 'c', // MODIFIER LETTER SMALL C
        0x1DA0 => 'f', // MODIFIER LETTER SMALL F
        0x1DBB => 'z', // MODIFIER LETTER SMALL Z
        0x2071 => 'i', // SUPERSCRIPT LATIN SMALL LETTER I
        0x207F => 'n', // SUPERSCRIPT LATIN SMALL LETTER N
        0x2090 => 'a', // LATIN SUBSCRIPT SMALL LETTER A
        0x2091 => 'e', // LATIN SUBSCRIPT SMALL LETTER E
        0x2092 => 'o', // LATIN SUBSCRIPT SMALL LETTER O
        0x2093 => 'x', // LATIN SUBSCRIPT SMALL LETTER X
        0x2095 => 'h', // LATIN SUBSCRIPT SMALL LETTER H
        0x2096 => 'k', // LATIN SUBSCRIPT SMALL LETTER K
        0x2097 => 'l', // LATIN SUBSCRIPT SMALL LETTER L
        0x2098 => 'm', // LATIN SUBSCRIPT SMALL LETTER M
        0x2099 => 'n', // LATIN SUBSCRIPT SMALL LETTER N
        0x209A => 'p', // LATIN SUBSCRIPT SMALL LETTER P
        0x209B => 's', // LATIN SUBSCRIPT SMALL LETTER S
        0x209C => 't', // LATIN SUBSCRIPT SMALL LETTER T
        0x212A => 'K', // KELVIN SIGN
        // ANGSTROM SIGN. NFKC gives `\u{00C5}` (A-with-ring), NOT an ASCII
        // letter, so the generated sweep correctly skipped it — but that is one
        // more step, not a different answer: `\u{00C5}` is then accent-stripped
        // to `A` like any other ring-A. Added by hand so the two-step path
        // lands where the one-step path does. Found by requiring tier 1 in
        // `roman_numerals_and_nfkc_letters_fold`; under the weaker `is_some()`
        // assertion it read as a match at NearMiss.
        0x212B => 'A',  // ANGSTROM SIGN
        0x2145 => 'D',  // DOUBLE-STRUCK ITALIC CAPITAL D
        0x2146 => 'd',  // DOUBLE-STRUCK ITALIC SMALL D
        0x2147 => 'e',  // DOUBLE-STRUCK ITALIC SMALL E
        0x2148 => 'i',  // DOUBLE-STRUCK ITALIC SMALL I
        0x2149 => 'j',  // DOUBLE-STRUCK ITALIC SMALL J
        0x2160 => 'I',  // ROMAN NUMERAL ONE
        0x2164 => 'V',  // ROMAN NUMERAL FIVE
        0x2169 => 'X',  // ROMAN NUMERAL TEN
        0x216C => 'L',  // ROMAN NUMERAL FIFTY
        0x216D => 'C',  // ROMAN NUMERAL ONE HUNDRED
        0x216E => 'D',  // ROMAN NUMERAL FIVE HUNDRED
        0x216F => 'M',  // ROMAN NUMERAL ONE THOUSAND
        0x2170 => 'i',  // SMALL ROMAN NUMERAL ONE
        0x2174 => 'v',  // SMALL ROMAN NUMERAL FIVE
        0x2179 => 'x',  // SMALL ROMAN NUMERAL TEN
        0x217C => 'l',  // SMALL ROMAN NUMERAL FIFTY
        0x217D => 'c',  // SMALL ROMAN NUMERAL ONE HUNDRED
        0x217E => 'd',  // SMALL ROMAN NUMERAL FIVE HUNDRED
        0x217F => 'm',  // SMALL ROMAN NUMERAL ONE THOUSAND
        0x2C7C => 'j',  // LATIN SUBSCRIPT SMALL LETTER J
        0x2C7D => 'V',  // MODIFIER LETTER CAPITAL V
        0xA7F2 => 'C',  // MODIFIER LETTER CAPITAL C
        0xA7F3 => 'F',  // MODIFIER LETTER CAPITAL F
        0xA7F4 => 'Q',  // MODIFIER LETTER CAPITAL Q
        0x107A5 => 'q', // MODIFIER LETTER SMALL Q
        _ => return None,
    })
}

//@@ confusable.rs
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

//@@ confusable.rs
/// One Mathematical-Greek block's 58 slots, in order, as their plain Greek
/// (or plain mathematical) equivalents.
///
/// Generated from Unicode NFKD; all five blocks share this layout, which
/// `the_five_math_greek_blocks_share_one_layout` re-derives rather than trusts.
const MATH_GREEK_FOLD: &str = concat!(
    "\u{0391}\u{0392}\u{0393}\u{0394}\u{0395}\u{0396}\u{0397}\u{0398}", // Α Β Γ Δ Ε Ζ Η Θ
    "\u{0399}\u{039A}\u{039B}\u{039C}\u{039D}\u{039E}\u{039F}\u{03A0}", // Ι Κ Λ Μ Ν Ξ Ο Π
    "\u{03A1}\u{0398}\u{03A3}\u{03A4}\u{03A5}\u{03A6}\u{03A7}\u{03A8}", // Ρ Θsym Σ Τ Υ Φ Χ Ψ
    "\u{03A9}\u{2207}\u{03B1}\u{03B2}\u{03B3}\u{03B4}\u{03B5}\u{03B6}", // Ω ∇ α β γ δ ε ζ
    "\u{03B7}\u{03B8}\u{03B9}\u{03BA}\u{03BB}\u{03BC}\u{03BD}\u{03BE}", // η θ ι κ λ μ ν ξ
    "\u{03BF}\u{03C0}\u{03C1}\u{03C2}\u{03C3}\u{03C4}\u{03C5}\u{03C6}", // ο π ρ ς σ τ υ φ
    "\u{03C7}\u{03C8}\u{03C9}\u{2202}\u{03B5}\u{03B8}\u{03BA}\u{03C6}", // χ ψ ω ∂ εsym θsym κsym φsym
    "\u{03C1}\u{03C0}",                                                 // ρsym πsym
);

//@@ confusable.rs
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

//@@ confusable.rs
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

//@@ confusable.rs
/// U+0180..U+024F (Latin Extended-B), in order. `\0` = leave the character
/// alone.
///
/// **Generated, not curated**, by the same sweep that produced
/// [`fold_nfkc_letter`]: a slot folds exactly when the codepoint's NFD is one
/// ASCII letter followed only by combining marks. That is the definition the
/// two tables above already implement by hand for their ranges, so this simply
/// stops the coverage ending at U+017F for no reason.
///
/// 83 of the 208 slots fold. The rest are letters in their own right, exactly
/// as NFD leaves them.
///
/// This range carries the Vietnamese horn letters, which is why
/// `H\u{01B0}\u{01A1}ng` needs it.
const LATIN_EXT_B_FOLD: &str = concat!(
    "\0\0\0\0\0\0\0\0", // U+0180 ƀ Ɓ Ƃ ƃ Ƅ ƅ Ɔ Ƈ
    "\0\0\0\0\0\0\0\0", // U+0188 ƈ Ɖ Ɗ Ƌ ƌ ƍ Ǝ Ə
    "\0\0\0\0\0\0\0\0", // U+0190 Ɛ Ƒ ƒ Ɠ Ɣ ƕ Ɩ Ɨ
    "\0\0\0\0\0\0\0\0", // U+0198 Ƙ ƙ ƚ ƛ Ɯ Ɲ ƞ Ɵ
    "Oo\0\0\0\0\0\0",   // U+01A0 Ơ ơ Ƣ ƣ Ƥ ƥ Ʀ Ƨ
    "\0\0\0\0\0\0\0U",  // U+01A8 ƨ Ʃ ƪ ƫ Ƭ ƭ Ʈ Ư
    "u\0\0\0\0\0\0\0",  // U+01B0 ư Ʊ Ʋ Ƴ ƴ Ƶ ƶ Ʒ
    "\0\0\0\0\0\0\0\0", // U+01B8 Ƹ ƹ ƺ ƻ Ƽ ƽ ƾ ƿ
    "\0\0\0\0\0\0\0\0", // U+01C0 ǀ ǁ ǂ ǃ Ǆ ǅ ǆ Ǉ
    "\0\0\0\0\0AaI",    // U+01C8 ǈ ǉ Ǌ ǋ ǌ Ǎ ǎ Ǐ
    "iOoUuUuU",         // U+01D0 ǐ Ǒ ǒ Ǔ ǔ Ǖ ǖ Ǘ
    "uUuUu\0Aa",        // U+01D8 ǘ Ǚ ǚ Ǜ ǜ ǝ Ǟ ǟ
    "Aa\0\0\0\0Gg",     // U+01E0 Ǡ ǡ Ǣ ǣ Ǥ ǥ Ǧ ǧ
    "KkOoOo\0\0",       // U+01E8 Ǩ ǩ Ǫ ǫ Ǭ ǭ Ǯ ǯ
    "j\0\0\0Gg\0\0",    // U+01F0 ǰ Ǳ ǲ ǳ Ǵ ǵ Ƕ Ƿ
    "NnAa\0\0\0\0",     // U+01F8 Ǹ ǹ Ǻ ǻ Ǽ ǽ Ǿ ǿ
    "AaAaEeEe",         // U+0200 Ȁ ȁ Ȃ ȃ Ȅ ȅ Ȇ ȇ
    "IiIiOoOo",         // U+0208 Ȉ ȉ Ȋ ȋ Ȍ ȍ Ȏ ȏ
    "RrRrUuUu",         // U+0210 Ȑ ȑ Ȓ ȓ Ȕ ȕ Ȗ ȗ
    "SsTt\0\0Hh",       // U+0218 Ș ș Ț ț Ȝ ȝ Ȟ ȟ
    "\0\0\0\0\0\0Aa",   // U+0220 Ƞ ȡ Ȣ ȣ Ȥ ȥ Ȧ ȧ
    "EeOoOoOo",         // U+0228 Ȩ ȩ Ȫ ȫ Ȭ ȭ Ȯ ȯ
    "OoYy\0\0\0\0",     // U+0230 Ȱ ȱ Ȳ ȳ ȴ ȵ ȶ ȷ
    "\0\0\0\0\0\0\0\0", // U+0238 ȸ ȹ Ⱥ Ȼ ȼ Ƚ Ⱦ ȿ
    "\0\0\0\0\0\0\0\0", // U+0240 ɀ Ɂ ɂ Ƀ Ʉ Ʌ Ɇ ɇ
    "\0\0\0\0\0\0\0\0", // U+0248 Ɉ ɉ Ɋ ɋ Ɍ ɍ Ɏ ɏ
);

//@@ confusable.rs
/// U+1E00..U+1EFF (Latin Extended Additional), in order. `\0` = leave the
/// character alone.
///
/// Generated the same way as [`LATIN_EXT_B_FOLD`]. 244 of the 256 slots fold,
/// which makes this by far the largest gap the accent strip had: the block is
/// **all of Vietnamese**, plus the Latin dot-above/dot-below letters. Every one
/// of them was an evasion — `I\u{1EA1}n Clarke`, an `a` with a dot below that
/// at member-list size is a speck, folded to itself and never came near
/// `Ian Clarke`.
///
/// The cost is the accent-stripping cost already accepted for `Muller`, scaled
/// up: Vietnamese tone marks are meaning-bearing, so `Nguy\u{1EC5}n`,
/// `Nguy\u{00EA}n` and `Nguyen` all reach one skeleton. Recorded in
/// `documented_accepted_collisions`; the surface that matters is bounded,
/// because a collision only produces a badge when it lands on a PROTECTED name,
/// and `real_names_in_other_scripts_are_not_flagged` sweeps a Vietnamese corpus
/// against the real moderator list.
const LATIN_EXT_ADD_FOLD: &str = concat!(
    "AaBbBbBb",       // U+1E00 Ḁ ḁ Ḃ ḃ Ḅ ḅ Ḇ ḇ
    "CcDdDdDd",       // U+1E08 Ḉ ḉ Ḋ ḋ Ḍ ḍ Ḏ ḏ
    "DdDdEeEe",       // U+1E10 Ḑ ḑ Ḓ ḓ Ḕ ḕ Ḗ ḗ
    "EeEeEeFf",       // U+1E18 Ḙ ḙ Ḛ ḛ Ḝ ḝ Ḟ ḟ
    "GgHhHhHh",       // U+1E20 Ḡ ḡ Ḣ ḣ Ḥ ḥ Ḧ ḧ
    "HhHhIiIi",       // U+1E28 Ḩ ḩ Ḫ ḫ Ḭ ḭ Ḯ ḯ
    "KkKkKkLl",       // U+1E30 Ḱ ḱ Ḳ ḳ Ḵ ḵ Ḷ ḷ
    "LlLlLlMm",       // U+1E38 Ḹ ḹ Ḻ ḻ Ḽ ḽ Ḿ ḿ
    "MmMmNnNn",       // U+1E40 Ṁ ṁ Ṃ ṃ Ṅ ṅ Ṇ ṇ
    "NnNnOoOo",       // U+1E48 Ṉ ṉ Ṋ ṋ Ṍ ṍ Ṏ ṏ
    "OoOoPpPp",       // U+1E50 Ṑ ṑ Ṓ ṓ Ṕ ṕ Ṗ ṗ
    "RrRrRrRr",       // U+1E58 Ṙ ṙ Ṛ ṛ Ṝ ṝ Ṟ ṟ
    "SsSsSsSs",       // U+1E60 Ṡ ṡ Ṣ ṣ Ṥ ṥ Ṧ ṧ
    "SsTtTtTt",       // U+1E68 Ṩ ṩ Ṫ ṫ Ṭ ṭ Ṯ ṯ
    "TtUuUuUu",       // U+1E70 Ṱ ṱ Ṳ ṳ Ṵ ṵ Ṷ ṷ
    "UuUuVvVv",       // U+1E78 Ṹ ṹ Ṻ ṻ Ṽ ṽ Ṿ ṿ
    "WwWwWwWw",       // U+1E80 Ẁ ẁ Ẃ ẃ Ẅ ẅ Ẇ ẇ
    "WwXxXxYy",       // U+1E88 Ẉ ẉ Ẋ ẋ Ẍ ẍ Ẏ ẏ
    "ZzZzZzht",       // U+1E90 Ẑ ẑ Ẓ ẓ Ẕ ẕ ẖ ẗ
    "wy\0\0\0\0\0\0", // U+1E98 ẘ ẙ ẚ ẛ ẜ ẝ ẞ ẟ
    "AaAaAaAa",       // U+1EA0 Ạ ạ Ả ả Ấ ấ Ầ ầ
    "AaAaAaAa",       // U+1EA8 Ẩ ẩ Ẫ ẫ Ậ ậ Ắ ắ
    "AaAaAaAa",       // U+1EB0 Ằ ằ Ẳ ẳ Ẵ ẵ Ặ ặ
    "EeEeEeEe",       // U+1EB8 Ẹ ẹ Ẻ ẻ Ẽ ẽ Ế ế
    "EeEeEeEe",       // U+1EC0 Ề ề Ể ể Ễ ễ Ệ ệ
    "IiIiOoOo",       // U+1EC8 Ỉ ỉ Ị ị Ọ ọ Ỏ ỏ
    "OoOoOoOo",       // U+1ED0 Ố ố Ồ ồ Ổ ổ Ỗ ỗ
    "OoOoOoOo",       // U+1ED8 Ộ ộ Ớ ớ Ờ ờ Ở ở
    "OoOoUuUu",       // U+1EE0 Ỡ ỡ Ợ ợ Ụ ụ Ủ ủ
    "UuUuUuUu",       // U+1EE8 Ứ ứ Ừ ừ Ử ử Ữ ữ
    "UuYyYyYy",       // U+1EF0 Ự ự Ỳ ỳ Ỵ ỵ Ỷ ỷ
    "Yy\0\0\0\0\0\0", // U+1EF8 Ỹ ỹ Ỻ ỻ Ỽ ỽ Ỿ ỿ
);

//@@ confusable.rs
/// Fold `s` to the characters a reader would take it for, KEEPING case and
/// every ASCII character as is: invisible and joiner characters dropped,
/// fullwidth / mathematical / small-capital forms and combining accents folded,
/// and cross-script homoglyphs mapped to the Latin letter they look like.
///
/// This is steps 1-5 of [`skeleton`] only, with no case, bar, digit or
/// multi-character folds, for callers that must compare case-sensitive ASCII
/// (a base58 contract id) against what is displayed.
pub(crate) fn visual_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if is_display_hidden(c) {
            if c.is_whitespace() {
                out.push(' ');
            }
            continue;
        }
        if c == '\u{200C}' || c == '\u{200D}' || is_combining_mark(c) {
            continue;
        }
        if let Some(ascii) = fold_presentation_form(c).or_else(|| fold_small_capital(c)) {
            out.push(ascii);
            continue;
        }
        out.push(fold_homoglyph(strip_latin_accent(c).unwrap_or(c)));
    }
    out
}

//@@ display_name.rs
/// Unicode general category `Cf` (Format): every character whose whole job is
/// to be invisible and change how neighbouring text is laid out.
///
/// **Generated from Unicode 15.0 data, not hand-listed.** The list above grew
/// one codepoint at a time as each was noticed, and the result had the exact
/// signature of that process: U+06DE was in it but U+06DD was not; U+0488/U+0489
/// were covered but U+0890/U+0891 were not. Thirteen `Cf` codepoints survived
/// BOTH the sanitiser and the confusable fold — U+0600..U+0605, U+06DD, U+070F,
/// U+0890, U+0891, U+08E2, U+110BD, U+110CD and U+13430..U+1343F — and because
/// `is_display_hidden` also gates [`sanitize_display_name`], they survived into
/// the RENDERED nickname. `Ian\u{070F} Clarke` is not merely a fold miss; it is
/// a pixel-identical clone of another member's displayed name.
///
/// Regenerate by sweeping `0..0x110000` for `unicodedata.category(chr(cp)) ==
/// 'Cf'` and collapsing to ranges; 170 codepoints in 21 ranges as of Unicode
/// 15.0. `every_format_character_is_hidden` pins a representative of each range.
///
/// ## The other invisible categories
///
/// * `Cs` (surrogates) cannot exist in a Rust `char`, so there is nothing to do.
/// * `Co` (private use) is covered by the PUA ranges in [`is_display_hidden`].
/// * `Cn` (unassigned) is deliberately NOT covered. An unassigned codepoint
///   renders as a visible replacement box, not as nothing, so it does not clone
///   anyone's name — and the set SHRINKS with every Unicode release, so a frozen
///   `Cn` table would start stripping newly-assigned letters out of real names.
///   That is the one direction of error this module must not have.
///
/// ## The two exceptions, which are the same ones as everywhere else
///
/// U+200C ZWNJ and U+200D ZWJ are `Cf` and are deliberately NOT reported. They
/// are orthography in Persian, Sinhala and Malayalam, and stripping them at
/// RENDER time mangles real names — the reasoning is on the `0x200B` entry
/// below. `crate::util::confusable::skeleton` drops them anyway, because
/// comparison is not rendering.
fn is_format_control(c: char) -> bool {
    if c == '\u{200C}' || c == '\u{200D}' {
        return false;
    }
    matches!(u32::from(c),
        0x00AD                  // SOFT HYPHEN
        | 0x0600..=0x0605       // ARABIC NUMBER SIGN..ARABIC NUMBER MARK ABOVE
        | 0x061C                // ARABIC LETTER MARK
        | 0x06DD                // ARABIC END OF AYAH
        | 0x070F                // SYRIAC ABBREVIATION MARK
        | 0x0890..=0x0891       // ARABIC POUND/PIASTRE MARK ABOVE
        | 0x08E2                // ARABIC DISPUTED END OF AYAH
        | 0x180E                // MONGOLIAN VOWEL SEPARATOR
        | 0x200B..=0x200F       // ZWSP, ZWNJ, ZWJ, LRM, RLM (joiners excepted above)
        | 0x202A..=0x202E       // bidi embedding / override
        | 0x2060..=0x2064       // word joiner, invisible operators
        | 0x2066..=0x206F       // bidi isolates, deprecated formatting
        | 0xFEFF                // ZERO WIDTH NO-BREAK SPACE / BOM
        | 0xFFF9..=0xFFFB       // interlinear annotation anchors
        | 0x110BD | 0x110CD     // KAITHI NUMBER SIGN, ...ABOVE
        | 0x13430..=0x1343F     // Egyptian hieroglyph format controls
        | 0x1BCA0..=0x1BCA3     // shorthand format controls
        | 0x1D173..=0x1D17A     // musical beam/slur/phrase controls
        | 0xE0001               // LANGUAGE TAG
        | 0xE0020..=0xE007F     // TAG SPACE..CANCEL TAG
    )
}

//@@ display_name.rs
/// Whether `c` must never appear in rendered display text.
///
/// Ranges are Unicode *blocks* rather than the `Emoji` character property:
/// River has no Unicode-property dependency and the UI's wasm bundle size is
/// a standing concern, so a table of block ranges is the right trade. Blocks
/// are slightly broader than the emoji property (they also catch arrows,
/// geometric shapes and dingbats), which is the safe direction — those are
/// symbols, not letters, and none of them belong in a person's name.
pub fn is_display_hidden(c: char) -> bool {
    // Control characters (C0/C1). A newline or NUL in a nickname is never
    // legitimate and breaks layout.
    if c.is_control() {
        return true;
    }
    // Every Unicode FORMAT character, by category rather than by whichever ones
    // someone happened to hit. See [`is_format_control`].
    if is_format_control(c) {
        return true;
    }

    matches!(u32::from(c),
        // Symbols that Latin-1 inherited and that render as emoji: © ®
        0x00A9 | 0x00AE
        // ‼ ⁉
        | 0x203C | 0x2049
        // Zero-width space, and the LTR/RTL marks.
        //
        // NOT U+200C ZWNJ or U+200D ZWJ. Those look like emoji machinery — ZWJ
        // is what joins 👩 + ZWJ + 💻 — but they are orthography in several
        // scripts, and stripping them mangles real names: Persian compounds
        // (`علی‌رضا` Alireza, `حسین‌زاده` Hosseinzadeh) need ZWNJ to keep the
        // preceding letter in its final form, Sinhala touching letters
        // (`සූර්‍ය` Surya) need ZWJ, and several Malayalam IMEs emit chillu as
        // consonant + virama + ZWJ. Keeping them is safe here because every
        // emoji a ZWJ could join is itself stripped, so the joiner has nothing
        // left to assemble.
        | 0x200B | 0x200E | 0x200F
        // Line/paragraph separators.
        | 0x2028..=0x2029
        // Bidi embedding / override controls (the `\u{202E}` reversal trick).
        | 0x202A..=0x202E
        // Word joiner, invisible operators, bidi isolates.
        | 0x2060..=0x2064 | 0x2066..=0x206F
        // ™ ℹ
        | 0x2122 | 0x2139
        // Arrows.
        | 0x2190..=0x21FF
        // Miscellaneous Technical (⌚ ⌛ ⏰ …).
        | 0x2300..=0x23FF
        // Enclosed Alphanumerics (① Ⓐ …).
        | 0x2460..=0x24FF
        // Geometric Shapes, Miscellaneous Symbols (☀ ⚔ ⛨ …), Dingbats
        // (✅ ❌ ❤ …) — one contiguous run.
        | 0x25A0..=0x27BF
        // Supplemental Arrows-B.
        | 0x2900..=0x297F
        // Miscellaneous Symbols and Arrows (⬛ ⭐ …).
        | 0x2B00..=0x2BFF
        // Combining Diacritical Marks for Symbols. Includes the keycap
        // assembler (`1️⃣`) but also the ENCLOSING marks — U+20DD circle,
        // U+20DE square, U+20E0 circle-backslash, U+20E4 triangle — which
        // rebuild by composition the very glyphs `0x2460..=0x24FF` is stripped
        // for: `A\u{20DD}` is Ⓐ, `!\u{20E4}` reads as ⚠. The block has no
        // letter content.
        | 0x20D0..=0x20F0
        // The rest of Unicode general category Me (Enclosing_Mark). Me draws a
        // shape AROUND the preceding character, so every one of them composes
        // a badge the same way U+20DD does: `A\u{A670}` renders a circled A,
        // which is byte-for-byte the `A\u{20DD}` attack above. Me has thirteen
        // members; the block above covers seven and these are the other six.
        // U+0488/U+0489 are the Cyrillic hundred-thousands and millions signs,
        // U+A670..U+A672 the ten-millions family, U+1ABE the parentheses
        // overlay. Stripping a whole general category is the exception to the
        // "combining marks are preserved" rule in the module header: Mn marks
        // decorate a letter, Me marks enclose it, and only the latter can draw
        // a badge.
        | 0x0488..=0x0489 | 0x1ABE | 0xA670..=0xA672
        // 〰 〽 and the two emoji-presented enclosed ideographs ㊗ ㊙. The
        // rest of the CJK punctuation and Enclosed CJK blocks is untouched.
        | 0x3030 | 0x303D | 0x3297 | 0x3299
        // Variation selectors — VS16 is what turns a text-presentation
        // character into its emoji glyph.
        | 0xFE00..=0xFE0F
        // Zero-width no-break space / BOM.
        | 0xFEFF
        // The remaining invisible `Cf` formatting characters that no range
        // above covers, and that `char::is_control()` (which is `Cc` only)
        // misses. U+061C ARABIC LETTER MARK is a bidi control like U+200E/F;
        // U+00AD SOFT HYPHEN and U+180E MONGOLIAN VOWEL SEPARATOR render as
        // nothing; U+FFF9..U+FFFB are interlinear annotation anchors that hide
        // the text between them.
        | 0x00AD | 0x061C | 0x180E | 0xFFF0..=0xFFFB
        // Blank glyphs that are not whitespace, so the space collapse below
        // would not remove them: they let two members share a pixel-identical
        // rendered name (`Alice` vs `Alice\u{3164}`), which undermines the
        // "you can tell members apart by name" assumption the badge sits on.
        // U+2800 BRAILLE PATTERN BLANK, and the Hangul fillers.
        | 0x115F | 0x1160 | 0x2800 | 0x3164 | 0xFFA0
        // The rest of the Default_Ignorable characters, which render as
        // nothing. Without these a nickname made ENTIRELY of them is
        // non-empty (so it never becomes `UNNAMED`) yet renders blank, which
        // makes a message header look like a continuation of the group above
        // it — including a badged moderator's group.
        // U+034F combining grapheme joiner, the Mongolian free variation
        // selectors, the Khmer inherent vowels, U+2065, the Variation
        // Selectors Supplement (U+FE00..FE0F's big brother), and the
        // shorthand-format and musical beam/slur/phrase controls.
        | 0x034F | 0x180B..=0x180D | 0x180F | 0x17B4..=0x17B5 | 0x2065
        | 0x1BCA0..=0x1BCA3 | 0x1D173..=0x1D17A
        // Text-presentation symbols that read as a badge in the fonts that
        // carry them: ۞ (ornate star, present wherever Arabic renders), ٭,
        // ꙳, and the Phaistos shield. Plus Symbols for Legacy Computing and
        // its supplement, which contain an inverse check mark and stick
        // figures.
        | 0x066D | 0x06DE | 0xA673
        // Aegean/Phaistos: picking out only the shield (U+101DB) left the
        // helmet, tiara, rosette and — the one that matters — U+10102 AEGEAN
        // CHECK MARK, which renders as ✓ wherever Noto Sans Symbols is
        // installed (stock Ubuntu/Fedora).
        | 0x10100..=0x101FC
        // Halfwidth clones of the geometric shapes stripped above.
        | 0xFFED..=0xFFEE
        | 0x1FB00..=0x1FBFF | 0x1CC00..=0x1CEBF
        // Private Use Area (BMP). Font-defined glyphs, and River's own
        // mention sentinels live at U+E000/U+E001.
        | 0xE000..=0xF8FF
        // The emoji planes: Mahjong/Domino/Cards, Enclosed Alphanumeric
        // Supplement (regional-indicator flags), Miscellaneous Symbols and
        // Pictographs, Emoticons, Transport, Supplemental Symbols and
        // Pictographs, Symbols and Pictographs Extended-A. 🛡 is U+1F6E1.
        | 0x1F000..=0x1FAFF
        // Plane 14's Default_Ignorable range. `E0000..E0FFF` is the whole set
        // Unicode marks ignorable in that plane, and the rest of plane 14
        // (`E1000..EFFFF`) is not ignorable and is left alone. Naming only the
        // tag block (`E0000..E007F`, the flag-sequence assembler 🏴󠁧󠁢󠁳󠁣󠁴󠁿) left
        // ~3,700 invisible characters through, each of which clones another
        // member's rendered name.
        //
        // The Ideographic Variation Selectors (`E0100..E01EF`) are INSIDE this
        // range on purpose, even though they are the one part of it that is
        // legitimate in a name. Their exception is applied ON TOP, in context,
        // by [`sanitize_display_name`] and [`contains_hidden_chars`] — NOT by
        // punching a hole here. TWO separate things depend on that.
        //
        // First, `confusable.rs::skeleton` folds names for impersonation
        // detection by calling THIS function directly. Keeping the selectors
        // inside the range is what lets it fold `"李\u{E0100}小龍"` and
        // `"李小龍"` together, so the confusable warning covers the residual
        // the in-context exception deliberately leaves open (module header).
        // Punching a hole here would switch that warning off silently.
        //
        // Second, two complementary hand-written ranges drift:
        // narrowing the carve-out by one codepoint leaves a character that is
        // neither stripped nor judged, so it survives verbatim in every name
        // while rendering as nothing. Layering makes that gap impossible, and
        // `no_plane_14_codepoint_escapes_both_the_strip_and_the_carve_out`
        // fails if anyone splits it again.
        | 0xE0000..=0xE0FFF
        // Supplementary Private Use Areas A and B.
        | 0xF0000..=0xFFFFD
        | 0x100000..=0x10FFFD
    )
}

//@@ confusable.rs
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

//@@ confusable.rs
fn strip_spaces(s: &str) -> String {
    s.chars().filter(|c| *c != ' ').collect()
}

//@@ confusable.rs
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
        // 1b. **Joiners, dropped unconditionally** — the OTHER half of #488's
        //     kept-in-context residual.
        //
        //     `is_display_hidden` deliberately does NOT report U+200C/U+200D,
        //     because they are orthography in Persian, Sinhala and Malayalam and
        //     blanket-stripping them mangles real names at RENDER time. But this
        //     is not render, it is comparison, and a joiner never distinguishes
        //     two people's names to a reader — which is the entire premise of
        //     the residual `display_name.rs`'s header documents. So dropping
        //     them here has zero false-positive cost and closes the clone:
        //     without it `"李\u{200D}小龍"` and `"李小龍"` fold apart and the
        //     impostor is unflagged, leaving every CJK, Persian, Sinhala and
        //     Malayalam deputy name cloneable.
        if c == '\u{200C}' || c == '\u{200D}' {
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
        // 2b. Small capitals — the single commonest fancy-text style on chat
        //     platforms after the mathematical blocks, and one NFKC does NOT
        //     touch, so it needs its own judgement. See [`fold_small_capital`].
        if let Some(folded) = fold_small_capital(c) {
            out.push(folded);
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

    // 7a. **Homoglyphs, AGAIN, after the case fold.**
    //
    // The pass at step 5 runs before `to_lowercase`, which is correct for the
    // uppercase entries (`Η` looks like `H` but its lowercase `η` looks like
    // `n`, so one case-insensitive rule would be wrong for both). But it left a
    // whole class through: any UPPERCASE codepoint absent from the table whose
    // Unicode lowercase IS in the table survived as the raw codepoint, because
    // nothing folded it after the case step. The lowercase forms were caught;
    // the uppercase ones were not.
    //
    //   * `Ӏ` U+04C0 CYRILLIC PALOCHKA — a bare vertical stroke, pixel-identical
    //     to Latin capital I, and the single most-used I-spoof in IDN attacks.
    //   * `Ԁ` U+0500 CYRILLIC KOMI DE, `Ѵ` U+0474 CYRILLIC IZHITSA.
    //
    // Running the same table a second time closes the class rather than the
    // three instances: `to_lowercase` has already mapped each of them onto its
    // lowercase form (`ӏ`, `ԁ`, `ѵ`), all of which the table already carried.
    //
    // **A character this pass rewrites must then go through the bar fold too,
    // and ONLY such a character.** The palochka lowercases to `ӏ`, which this
    // table maps to `l` — a bar — but the bar fold at step 6/7 has already run,
    // so without this it stayed a literal `l` and `"Ӏan Clarke"` folded to
    // `"lan c1arke"` against the real `"1an c1arke"`. It still evaded.
    //
    // Re-running the bar fold over the WHOLE string instead would be wrong: in
    // the visual fold a capital `L` legitimately reaches this point as `l` (it
    // is not a bar — it has a foot), and folding it here would re-merge
    // `Lisa`/`Iisa`, undoing the split [`Fold`] exists to make. So the bar fold
    // is applied per character, conditional on this pass having changed it.
    let out: String = out
        .chars()
        .map(|c| {
            let folded = fold_homoglyph(c);
            if folded == c {
                c
            } else {
                fold_bar_char(folded)
            }
        })
        .collect();

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

//@@ confusable.rs
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

//@@ confusable.rs
/// The bar-shaped characters, folded to a `'1'` sentinel.
///
/// Deliberately NOT folded to the letter `i`: `i` has a dot and is a different
/// shape, and merging them is what made `Alia`/`Alla` and `Ilan`/`Lian` collide.
/// See [`Fold`]. Which characters are in the class depends on whether the case
/// step has already run — the caller decides by choosing the [`Fold`].
fn fold_bar_class(s: &str) -> String {
    s.chars().map(fold_bar_char).collect()
}

//@@ confusable.rs
/// [`fold_bar_class`] for one character.
fn fold_bar_char(c: char) -> char {
    match c {
        // `I` is here for the pre-case pass; after `to_lowercase` it can no
        // longer appear, so the same table serves both directions.
        'l' | 'I' | '1' | '|' | '!' => '1',
        other => other,
    }
}

//@@ confusable.rs
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

//@@ confusable.rs
/// The space-stripped form of BOTH folds of `name`.
///
/// This is the pair a *guard* must compare when it wants to answer "could this
/// name ever tier-1 match that one?". [`ImpersonationChecker::tier_one_for`]
/// accepts a hit under any of four comparisons, and the two space-stripped ones
/// subsume the other two: equality WITH spaces implies equality without them.
/// So two names that disagree on both of these can never be a tier-1 match, and
/// two that agree on either always are.
///
/// It exists because the filters deciding which names ENTER the protected set
/// used to compare exact strings while the matcher they gate compares folds — a
/// guard weaker than the thing it guards, which is not a guard. See
/// [`crate::components::members::impersonation_checker_for_viewer`].
pub fn comparison_folds(name: &str) -> [String; 2] {
    [
        strip_spaces(&skeleton_with(name, Fold::Visual)),
        strip_spaces(&skeleton_with(name, Fold::CaseInsensitive)),
    ]
}

//@@ confusable.rs
/// Whether `a` and `b` are a tier-1 (same-skeleton) match under either fold.
///
/// The identity-free form of [`ImpersonationChecker::check_identical`], for
/// guards that compare a candidate against a fixed literal rather than against
/// the room's protected set.
pub fn folds_together(a: &str, b: &str) -> bool {
    let [a_visual, a_case] = comparison_folds(a);
    if a_visual.is_empty() {
        // A name that folds to nothing can never match: `candidate_folds`
        // rejects it before any comparison runs.
        return false;
    }
    let [b_visual, b_case] = comparison_folds(b);
    a_visual == b_visual || a_case == b_case
}

//@@ nickname.rs
/// First half of the handle. Exactly 100 entries — see `pools_are_100`.
pub const FIRST_NAMES: [&str; 100] = [
    "Acid", "Zero", "Crash", "Cyber", "Ghost", "Neon", "Razor", "Static", "Phantom", "Chrome",
    "Glitch", "Cipher", "Daemon", "Null", "Hex", "Rogue", "Vapor", "Plasma", "Volt", "Frost",
    "Quantum", "Binary", "Proxy", "Logic", "Echo", "Pixel", "Neural", "Photon", "Onyx", "Cobalt",
    "Nitro", "Turbo", "Solar", "Lunar", "Cosmic", "Astro", "Argon", "Xenon", "Neutron", "Proton",
    "Flux", "Prism", "Laser", "Lumen", "Nova", "Pulsar", "Quasar", "Comet", "Meteor", "Orbit",
    "Zenith", "Nebula", "Aurora", "Halcyon", "Obsidian", "Titanium", "Iridium", "Mercury",
    "Silicon", "Carbon", "Crystal", "Granite", "Velvet", "Indigo", "Violet", "Crimson", "Scarlet",
    "Azure", "Magenta", "Amber", "Copper", "Silver", "Ember", "Cinder", "Blaze", "Tempest",
    "Thunder", "Monsoon", "Zephyr", "Glacier", "Tundra", "Midnight", "Twilight", "Umbra",
    "Eclipse", "Stellar", "Astral", "Lucid", "Silent", "Hollow", "Feral", "Wired", "Analog",
    "Digital", "Modular", "Fractal", "Syntax", "Bitwise", "Cache", "Packet",
];

//@@ nickname.rs
/// Second half of the handle. Exactly 100 entries — see `pools_are_100`.
pub const LAST_NAMES: [&str; 100] = [
    "Override",
    "Phreak",
    "Wraith",
    "Surge",
    "Spike",
    "Pulse",
    "Breaker",
    "Worm",
    "Drift",
    "Storm",
    "Specter",
    "Cascade",
    "Cortex",
    "Reaper",
    "Circuit",
    "Modem",
    "Kernel",
    "Daylight",
    "Vector",
    "Falcon",
    "Raven",
    "Socket",
    "Glider",
    "Nomad",
    "Sentinel",
    "Havoc",
    "Relay",
    "Vertex",
    "Payload",
    "Mainframe",
    "Runner",
    "Prowler",
    "Seeker",
    "Voyager",
    "Corsair",
    "Ronin",
    "Vagabond",
    "Wanderer",
    "Pilgrim",
    "Courier",
    "Beacon",
    "Lantern",
    "Compass",
    "Anchor",
    "Harbor",
    "Keystone",
    "Bastion",
    "Citadel",
    "Rampart",
    "Bulwark",
    "Talon",
    "Osprey",
    "Kestrel",
    "Condor",
    "Harrier",
    "Albatross",
    "Skylark",
    "Nightjar",
    "Peregrine",
    "Corvid",
    "Lynx",
    "Jackal",
    "Viper",
    "Cobra",
    "Mantis",
    "Panther",
    "Jaguar",
    "Ocelot",
    "Wolfhound",
    "Serpent",
    "Foundry",
    "Forge",
    "Anvil",
    "Crucible",
    "Furnace",
    "Bellows",
    "Piston",
    "Turbine",
    "Dynamo",
    "Flywheel",
    "Lattice",
    "Matrix",
    "Nexus",
    "Conduit",
    "Junction",
    "Gateway",
    "Bridge",
    "Tunnel",
    "Terminal",
    "Console",
    "Sigil",
    "Rune",
    "Glyph",
    "Token",
    "Ledger",
    "Archive",
    "Codex",
    "Almanac",
    "Manifest",
    "Protocol",
];

//@@ nickname.rs
/// The folded word pools backing [`folds_to_generated_handle`], built once.
///
/// Indexed by fold: `[0]` is the visual fold, `[1]` the case-insensitive one,
/// matching the order [`crate::util::confusable::comparison_folds`] returns.
struct HandleSkeletons {
    first: [std::collections::HashSet<String>; 2],
    last: [std::collections::HashSet<String>; 2],
}

//@@ nickname.rs
fn handle_skeletons() -> &'static HandleSkeletons {
    static POOLS: std::sync::OnceLock<HandleSkeletons> = std::sync::OnceLock::new();
    POOLS.get_or_init(|| {
        let fold_pool = |words: &[&str]| {
            let mut sets = [
                std::collections::HashSet::new(),
                std::collections::HashSet::new(),
            ];
            for word in words {
                for (i, folded) in crate::util::confusable::comparison_folds(word)
                    .into_iter()
                    .enumerate()
                {
                    sets[i].insert(folded);
                }
            }
            sets
        };
        HandleSkeletons {
            first: fold_pool(&FIRST_NAMES),
            last: fold_pool(&LAST_NAMES),
        }
    })
}

//@@ nickname.rs
/// Whether `name` could be a tier-1 confusable match for one of the 10,000
/// handles this module can assign.
///
/// **A guard must be at least as wide as the thing it guards.**
/// [`is_generated_handle`] compares exact strings; the impersonation matcher it
/// used to gate compares [`crate::util::confusable::comparison_folds`]. Every
/// one of `amber worm`, `AMBER WORM`, `AmberWorm`, `Amber\u{3000}Worm`,
/// `Arnber Worm`, `Amber W0rm` and `Amber Worrn` is "not a generated handle" to
/// `==`, and every one of them is a tier-1 match for the member River assigned
/// `Amber Worm`. Letting any of them into the protected set puts "this member is
/// NOT a moderator" on a member who did nothing at all.
///
/// **No attacker is required.** A moderator who styles their own name in lower
/// case — `cipher daylight` — is enough to accuse every member holding the
/// handle `Cipher Daylight`. That is exactly the birthday-collision harm the
/// original exact filter was written to prevent, arriving through the filter.
///
/// ## Why this is a per-word split rather than 10,000 skeletons
///
/// The space-stripped fold of `"First Last"` is the space-stripped fold of
/// `"First"` followed by that of `"Last"`: the multi-character confusables
/// (`rn -> m`, `vv -> w`) run BEFORE spaces are removed, so none of them can
/// span the word boundary, and no word folds to leading or trailing space. So a
/// name matches some handle exactly when its fold splits into a folded
/// FIRST_NAME and a folded LAST_NAME — 200 skeletons to build instead of 20,000,
/// and a linear scan of split points instead of a 10,000-entry set probe.
///
/// `every_generated_handle_is_rejected_by_the_fold_guard` proves the equivalence
/// the cheap way round: it sweeps all 10,000 handles and requires each to be
/// rejected.
pub fn folds_to_generated_handle(name: &str) -> bool {
    let pools = handle_skeletons();
    for (fold_index, candidate) in crate::util::confusable::comparison_folds(name)
        .iter()
        .enumerate()
    {
        if candidate.is_empty() {
            continue;
        }
        let chars: Vec<char> = candidate.chars().collect();
        for split in 1..chars.len() {
            let head: String = chars[..split].iter().collect();
            if !pools.first[fold_index].contains(&head) {
                continue;
            }
            let tail: String = chars[split..].iter().collect();
            if pools.last[fold_index].contains(&tail) {
                return true;
            }
        }
    }
    false
}

//@@ display_name.rs
/// Rendered in place of a nickname that is empty once sanitised (e.g. the
/// nickname was nothing but emoji). Distinct from `"Unknown"`, which callers
/// already use for "no `member_info` record at all", so the two cases stay
/// distinguishable in the UI.
pub const UNNAMED: &str = "Unnamed";

//@@ display_name.rs
/// Shown next to a nickname `<input>` whose contents would not survive
/// [`sanitize_display_name`]. One wording, used by every nickname input, so a
/// user gets the same explanation wherever they hit it.
pub const EMOJI_REJECTION_MESSAGE: &str = "Nicknames can't contain emoji";

//@@ display_name.rs
/// Whether `c` is an Ideographic Variation Selector (VS17..VS256).
///
/// These sit inside plane 14's Default_Ignorable range but are the one part of
/// it that is NOT invisible: they SELECT A GLYPH. Japanese family names are
/// routinely spelled with one — `辻` has the variant `辻󠄀` (U+8FBB U+E0100), and
/// `邊`/`邉`, `﨑`, `髙` work the same way — and any font with an IVS table
/// (Source Han, Noto CJK) renders the selected form. Stripping them blanket-
/// wise rewrote real names, and because [`contains_hidden_chars`] gates the
/// nickname `<input>`, it also told those users their own name "can't contain
/// emoji" and refused to save it.
///
/// So they are judged in context instead, exactly like `U+200C`/`U+200D`: kept
/// where they can be doing the work they exist for, dropped everywhere else.
fn is_variation_selector_supplement(c: char) -> bool {
    matches!(u32::from(c), 0xE0100..=0xE01EF)
}

//@@ display_name.rs
/// The ideographs a variation selector may legitimately follow. These are the
/// blocks the Ideographic Variation Database actually registers sequences for;
/// after anything else a selector is invisible filler.
fn is_ideograph(c: char) -> bool {
    matches!(u32::from(c),
        // CJK Unified Ideographs Extension A, and the main block.
        0x3400..=0x4DBF | 0x4E00..=0x9FFF
        // CJK Compatibility Ideographs (where `﨑` lives).
        | 0xF900..=0xFAFF
        // Extensions B onwards, plus the compatibility supplement.
        | 0x20000..=0x323AF
    )
}

//@@ display_name.rs
/// Whether the variation selector at `chars[i]` can be selecting a glyph, i.e.
/// it directly follows an ideograph.
///
/// Anywhere else it renders as nothing and clones the surrounding name, so it
/// is dropped for the same reason an orphaned joiner is. `"Ian\u{E0100}"` is a
/// clone of `"Ian"`; `"辻\u{E0100}"` is a person's name.
fn selects_an_ideograph_variant(chars: &[char], i: usize) -> bool {
    i > 0 && chars.get(i - 1).copied().is_some_and(is_ideograph)
}

//@@ display_name.rs
/// Whether `s` contains anything [`sanitize_display_name`] would remove.
///
/// Drives the nickname `<input>`'s "Nicknames can't contain emoji" message —
/// UX only. Never rely on this for safety: the render-time strip is the
/// boundary, because `riverctl` never runs this code.
///
/// Position-sensitive for the same characters [`sanitize_display_name`] judges
/// in context, so it cannot reject a name the sanitiser would have kept: a
/// variation selector after an ideograph is a legitimate Japanese name and is
/// NOT flagged, while the same selector after `n` is invisible filler and is.
pub fn contains_hidden_chars(s: &str) -> bool {
    let chars: Vec<char> = s.chars().collect();
    chars.iter().enumerate().any(|(i, c)| {
        if is_variation_selector_supplement(*c) {
            return !selects_an_ideograph_variant(&chars, i);
        }
        is_display_hidden(*c)
    })
}

//@@ display_name.rs
/// Strip everything [`is_display_hidden`] rejects and tidy the result.
///
/// Removing a character can leave a double space (`"Alice 🛡 Smith"`), so
/// internal whitespace runs are collapsed to one space and the result is
/// trimmed. A name that is empty afterwards becomes [`UNNAMED`] rather than a
/// blank author line.
///
/// A removed character that was itself whitespace (a newline, a paragraph
/// separator) becomes a space rather than vanishing, so `"Alice\nBob"` stays
/// two words instead of collapsing to `"AliceBob"`. Zero-width characters are
/// not whitespace, so `"A\u{200B}lice"` correctly rejoins as `"Alice"`.
///
/// Only runs of ASCII space are collapsed, and only interior ones. Non-ASCII
/// spaces are left alone: U+3000 IDEOGRAPHIC SPACE is the conventional
/// separator between a Japanese surname and given name (`山田　太郎`), and
/// normalising it to `' '` would quietly rewrite a real name.
pub fn sanitize_display_name(raw: &str) -> String {
    // 1. Remove the hidden characters. A hidden character that was itself
    //    whitespace becomes a space so words don't run together.
    //
    //    A variation selector is judged here rather than by the blanket strip,
    //    because whether it is a Japanese name or invisible filler depends on
    //    what precedes it. Same decision, same inputs as
    //    [`contains_hidden_chars`], so the input check and the render strip
    //    cannot disagree about a given name.
    let raw_chars: Vec<char> = raw.chars().collect();
    let stripped: Vec<char> = raw_chars
        .iter()
        .enumerate()
        .filter_map(|(i, &c)| {
            if is_variation_selector_supplement(c) {
                return selects_an_ideograph_variant(&raw_chars, i).then_some(c);
            }
            match (is_display_hidden(c), c.is_whitespace()) {
                (true, true) => Some(' '),
                (true, false) => None,
                (false, _) => Some(c),
            }
        })
        .collect();

    // 2. Keep a joiner only where it can be doing orthographic work, which is
    //    between two NON-ASCII letters (Persian `علی‌رضا`, Sinhala `සූර්‍ය`,
    //    Malayalam chillu — which is consonant + virama + ZWJ and legitimately
    //    ENDS a name, so a trailing joiner after a non-ASCII letter is kept
    //    too). Everywhere else it is dropped, which covers two cases:
    //
    //    * Orphans left by step 1: `"Bob 👮🏽‍♀️"` strips down to `"Bob ‍"`.
    //    * The clone attack `"Bo\u{200D}b"`, which renders exactly like
    //      `"Bob"` in any Latin font. Latin script never needs a joiner, so
    //      requiring a non-ASCII neighbour costs nothing and closes it.
    //
    //    A joiner between two non-ASCII letters is still kept, so a CJK name
    //    can still be cloned this way. That is the residual documented in the
    //    module header, and it is the same shape as the homoglyph problem.
    let is_joiner = |c: char| c == '\u{200C}' || c == '\u{200D}';
    let joins_letters =
        |c: Option<&char>| c.is_some_and(|c| !c.is_ascii() && !c.is_whitespace() && !is_joiner(*c));
    let kept: Vec<char> = stripped
        .iter()
        .enumerate()
        .filter(|(i, c)| {
            !is_joiner(**c)
                || (*i > 0
                    && joins_letters(stripped.get(i - 1))
                    // A joiner is legitimate at the end of a WORD, not just at
                    // the end of the string: Malayalam legacy chillu is
                    // consonant + virama + ZWJ, so `"മോഹന\u{0D4D}\u{200D} കുമാർ"`
                    // (Mohan Kumar) carries one mid-name, and word-final ZWNJ
                    // does the same in Persian and Kurdish. Requiring
                    // end-of-string dropped it and degraded the chillu `ൻ` to
                    // `ന്`, showing a chandrakkala that is not part of the name.
                    // So: no next character, a non-ASCII letter, or whitespace.
                    //
                    // This does not widen the Latin clone surface — the
                    // PRECEDING character must still be a non-ASCII letter, so
                    // `"Bo\u{200D}b"` stays closed. It does add word-final
                    // positions to the non-ASCII residual already documented in
                    // the module header (an interior joiner in a CJK name), so
                    // no new capability, just more places for the same one.
                    && stripped
                        .get(i + 1)
                        .is_none_or(|n| joins_letters(Some(n)) || n.is_whitespace()))
        })
        .map(|(_, c)| *c)
        .collect();

    // 3. Normalise the spaces that are visually IDENTICAL to U+0020 (NBSP and
    //    friends) down to it, then collapse runs. Normalising loses nothing a
    //    reader can see, and it closes the `"Alice\u{00A0}Smith"` clone of
    //    `"Alice Smith"`. U+3000 IDEOGRAPHIC SPACE is deliberately NOT in this
    //    set: it renders double-width, it is visibly different, and it is the
    //    conventional separator in a Japanese name (`山田　太郎`).
    let looks_like_a_plain_space =
        |c: char| matches!(u32::from(c), 0x00A0 | 0x2000..=0x200A | 0x202F | 0x205F);
    let mut collapsed = String::with_capacity(kept.len());
    let mut last_was_space = false;
    for c in kept {
        let c = if looks_like_a_plain_space(c) { ' ' } else { c };
        let is_space = c == ' ';
        if !(is_space && last_was_space) {
            collapsed.push(c);
        }
        last_was_space = is_space;
    }

    let trimmed = collapsed.trim();
    if trimmed.is_empty() {
        UNNAMED.to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ported from `ui/src/util/confusable.rs`.
    #[test]
    fn visual_ascii_folds_what_the_reader_sees_and_keeps_case() {
        assert_eq!(visual_ascii("raAqM"), "raAqM");
        assert_eq!(visual_ascii("\u{0430}\u{0410}"), "aA");
        assert_eq!(visual_ascii("\u{FF52}\u{FF1A}"), "r:");
        assert_eq!(visual_ascii("\u{1D41F}\u{1D42B}"), "fr");
        assert_eq!(visual_ascii("fr\u{200B}ee\u{200D}net"), "freenet");
        assert_eq!(visual_ascii("e\u{0301}\u{00E9}"), "ee");
    }

    /// Every item above is a verbatim copy of an item in the web UI's source; this fails when
    /// the web copy changes or moves, so the two cannot drift apart silently.
    #[test]
    fn copies_match_the_web_ui() {
        let sources = [
            (
                "confusable.rs",
                include_str!("../../../../ui/src/util/confusable.rs"),
            ),
            (
                "display_name.rs",
                include_str!("../../../../ui/src/util/display_name.rs"),
            ),
            (
                "nickname.rs",
                include_str!("../../../../ui/src/nickname.rs"),
            ),
        ];
        // A Windows checkout may turn every newline into CRLF; compare line content only.
        let unix = |s: &str| s.replace("\r\n", "\n");
        let sources = sources.map(|(f, s)| (f, unix(s)));
        let mine = unix(include_str!("visual_fold.rs"));
        let body = mine.split("\n#[cfg(test)]\nmod tests").next().unwrap();
        let mut checked = 0;
        for chunk in body.split("//@@ ").skip(1) {
            let (file, item) = chunk.split_once('\n').expect("a marker line");
            let item = item.trim_end();
            let web = sources
                .iter()
                .find(|(f, _)| *f == file.trim())
                .unwrap_or_else(|| panic!("unknown source {file}"))
                .1
                .as_str();
            assert!(
                web.contains(item),
                "the copy of an item from {file} no longer matches the web UI:\n{}",
                item.lines()
                    .find(|l| l.contains("fn ") || l.contains("const "))
                    .unwrap_or("")
            );
            checked += 1;
        }
        assert!(checked >= 21, "only {checked} items found");
    }
}
