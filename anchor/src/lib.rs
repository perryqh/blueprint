//! Text-quote anchoring: given a document's flattened text and a
//! [`TextQuoteSelector`], decide which occurrence of the quote the selector
//! meant.
//!
//! This is a port of `findQuoteIndex` from `frontend/anchor.js`, which it
//! replaces — the browser now calls into this crate through wasm-bindgen, so
//! there is one implementation instead of an untested JS copy. The DOM half of
//! anchoring (walking text nodes, building Ranges, wrapping highlight spans)
//! stays in JavaScript, because it genuinely needs a DOM.
//!
//! # Indices are UTF-16 code units
//!
//! Every offset here counts UTF-16 code units, not bytes and not `char`s. That
//! is not an aesthetic choice: the caller is a browser, where `indexOf`,
//! `slice`, and `length` are all defined on UTF-16 code units, and the
//! [`CONTEXT_UNITS`]-wide context window is recorded on the JS side by
//! `captureSelector`. A port using bytes or `char`s would agree with the browser
//! on ASCII and diverge the moment a document contains an emoji — an
//! astral-plane character is one `char`, two code units, and four UTF-8 bytes,
//! so the window would span different text in each representation.
//!
//! # Why context crosses the boundary as `[u16]`
//!
//! Because the window has a fixed width in code units, it can bisect a
//! surrogate pair and leave a *lone surrogate* at the edge of the recorded
//! prefix. That is not hypothetical: 20 emoji followed by one BMP character
//! puts the cut at an odd offset, and the browser's `slice(-32)` duly hands back
//! a string starting with an unpaired low surrogate. The JS original compared it
//! as code units and matched.
//!
//! A lone surrogate has no UTF-8 encoding, so it cannot survive a `&str`
//! boundary — wasm-bindgen's `TextEncoder` replaces it with `U+FFFD` and the
//! prefix silently stops matching, sending the anchor to the wrong occurrence.
//! [`resolve_utf16`] therefore takes context as `&[u16]` (a `Uint16Array` from
//! JS), and `&str` is used only for the haystack and the quote, both of which
//! come from well-formed sources — parsed document text and `Selection::toString`.
//!
//! One consequence is worth knowing: such a selector can never reach the
//! *daemon*, because `JSON.stringify` emits a lone `\udfaf` escape and
//! `serde_json` rejects it (asserted in the tests below). So a comment anchored
//! there fails to save with a 400 — a pre-existing rough edge, not one this
//! crate introduces. It still matters here, because a staged draft is
//! highlighted from its in-memory selector before it is ever saved.

use std::ops::Range;

mod selector;
pub use selector::TextQuoteSelector;

#[cfg(feature = "wasm")]
mod wasm;

/// Width of the context window, in UTF-16 code units.
///
/// Shared with the frontend rather than duplicated: `captureSelector` records
/// the context and this module compares it, so a disagreement about the width
/// would silently break disambiguation for long prefixes. The wasm build
/// re-exports this as `contextUnits()` and `anchor.js` reads it from here.
pub const CONTEXT_UNITS: usize = 32;

/// How an anchor was resolved. The position is the same either way; this says
/// how much to trust it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// The recorded prefix/suffix agreed with the text around this occurrence —
    /// or the selector carried no context, so there was nothing to disagree
    /// with. Both mean "no evidence this is the wrong occurrence".
    Context,
    /// No occurrence agreed with the recorded context, and this is the first one
    /// anyway.
    ///
    /// Almost certainly the wrong occurrence: the comment was anchored to some
    /// other instance and the surrounding text has since changed. The
    /// alternative — reporting drift on text that is plainly still present —
    /// tested worse, so the wrong-but-visible anchor is deliberate. Surfacing it
    /// as a distinct variant is what lets a caller badge it rather than present
    /// it as a confident match, which the JS original could not do: it returned
    /// a bare index.
    Fallback,
}

/// A resolved anchor: where the quote landed, and how much to trust it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    /// Half-open range of the match, in UTF-16 code units.
    pub units: Range<usize>,
    pub how: How,
}

impl Anchor {
    /// The match as a byte range into `haystack`, for Rust callers that want to
    /// slice the string (drift re-resolution, `review.json`).
    ///
    /// `None` when either endpoint falls inside a surrogate pair, which has no
    /// byte position in UTF-8. Unreachable for a quote captured from a real
    /// selection; rounding to a neighbouring boundary instead would hand back a
    /// range covering text the caller never asked about.
    pub fn byte_range(&self, haystack: &str) -> Option<Range<usize>> {
        Some(utf16_to_byte(haystack, self.units.start)?..utf16_to_byte(haystack, self.units.end)?)
    }
}

/// Resolve `selector` against `haystack`, the flattened text of the document.
///
/// `None` means the quote does not occur at all — the caller renders that as
/// "drifted".
pub fn resolve(haystack: &str, selector: &TextQuoteSelector) -> Option<Anchor> {
    let prefix = selector.prefix.as_deref().map(encode);
    let suffix = selector.suffix.as_deref().map(encode);
    resolve_utf16(
        haystack,
        &selector.exact,
        prefix.as_deref(),
        suffix.as_deref(),
    )
}

/// [`resolve`] with context supplied as raw UTF-16 code units, which is how the
/// browser has to pass it — see the module docs on lone surrogates.
pub fn resolve_utf16(
    haystack: &str,
    exact: &str,
    prefix: Option<&[u16]>,
    suffix: Option<&[u16]>,
) -> Option<Anchor> {
    let hay = encode(haystack);
    let needle = encode(exact);

    // An empty quote is unanchorable, and bailing here is a deliberate
    // divergence from the JS original, which *hangs* on it: `indexOf('', n)`
    // clamps to `length` rather than returning -1, so once the scan passes the
    // end of the string the `searchFrom = found + 1` cursor stops advancing the
    // result and the loop spins forever. In the browser that freezes the tab,
    // and the daemon does not reject `exact: ""`, so a hand-rolled comment can
    // reach it. Nothing can usefully anchor to a zero-length quote regardless.
    if needle.is_empty() {
        return None;
    }

    // A missing or empty context field opts out of that half of the check —
    // `!selector.prefix` in the original, truthy for both `undefined` (the
    // server omits absent context) and `''` (a selection at the very start of
    // the document has no prefix).
    let prefix_tail = window_tail(prefix);
    let suffix_head = window_head(suffix);

    let mut from = 0;
    while let Some(found) = find(&hay, &needle, from) {
        let end = found + needle.len();
        // `saturating_sub` is JS's `Math.max(0, found - CONTEXT_UNITS)`; the
        // `min` is its implicit slice clamping.
        let before = &hay[found.saturating_sub(CONTEXT_UNITS)..found];
        let after = &hay[end..(end + CONTEXT_UNITS).min(hay.len())];

        let prefix_ok = prefix_tail.is_none_or(|t| before.ends_with(t));
        let suffix_ok = suffix_head.is_none_or(|h| after.starts_with(h));
        if prefix_ok && suffix_ok {
            return Some(Anchor {
                units: found..end,
                how: How::Context,
            });
        }
        from = found + 1;
    }

    // No occurrence agreed. Fall back to the first one if the quote exists at
    // all, mirroring the original's trailing `return text.indexOf(quote)`.
    let first = find(&hay, &needle, 0)?;
    Some(Anchor {
        units: first..first + needle.len(),
        how: How::Fallback,
    })
}

fn encode(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// First occurrence of `needle` in `hay` at or after `from`. Callers guarantee a
/// non-empty needle, so there is no empty-needle case to clamp.
fn find(hay: &[u16], needle: &[u16], from: usize) -> Option<usize> {
    if from >= hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// Last [`CONTEXT_UNITS`] of the recorded prefix — all the haystack slice can
/// provide, so all that is worth comparing. `None` disables the check.
fn window_tail(units: Option<&[u16]>) -> Option<&[u16]> {
    let units = units?;
    if units.is_empty() {
        return None;
    }
    Some(&units[units.len() - units.len().min(CONTEXT_UNITS)..])
}

/// First [`CONTEXT_UNITS`] of the recorded suffix. `None` disables the check.
fn window_head(units: Option<&[u16]>) -> Option<&[u16]> {
    let units = units?;
    if units.is_empty() {
        return None;
    }
    Some(&units[..units.len().min(CONTEXT_UNITS)])
}

/// Byte offset in `haystack` of UTF-16 code-unit offset `unit_idx`.
///
/// `None` if the offset is past the end or lands inside a surrogate pair.
pub fn utf16_to_byte(haystack: &str, unit_idx: usize) -> Option<usize> {
    let mut units = 0;
    for (byte, ch) in haystack.char_indices() {
        if units == unit_idx {
            return Some(byte);
        }
        units += ch.len_utf16();
        // Stepped straight over the target: it was the low half of a pair.
        if units > unit_idx {
            return None;
        }
    }
    (units == unit_idx).then_some(haystack.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel(exact: &str, prefix: Option<&str>, suffix: Option<&str>) -> TextQuoteSelector {
        TextQuoteSelector {
            ty: "TextQuoteSelector".into(),
            exact: exact.into(),
            prefix: prefix.map(Into::into),
            suffix: suffix.map(Into::into),
        }
    }

    struct Case {
        name: &'static str,
        text: &'static str,
        selector: TextQuoteSelector,
        /// Expected start offset and how it was resolved; `None` for absent.
        expect: Option<(usize, How)>,
    }

    impl Case {
        fn new(
            name: &'static str,
            text: &'static str,
            selector: TextQuoteSelector,
            expect: Option<(usize, How)>,
        ) -> Self {
            Self {
                name,
                text,
                selector,
                expect,
            }
        }
    }

    /// Cases carried over from the vitest suite that covered the JS original, so
    /// the port is pinned to the behaviour that shipped rather than to a fresh
    /// reading of the algorithm.
    #[test]
    fn resolves_the_occurrence_the_context_points_at() {
        let cases = [
            Case::new(
                "unique quote — context is redundant but must not break the match",
                "alpha beta gamma",
                sel("beta", Some("alpha "), Some(" gamma")),
                Some((6, How::Context)),
            ),
            Case::new(
                "unique quote with no context at all",
                "alpha beta gamma",
                sel("beta", None, None),
                Some((6, How::Context)),
            ),
            Case::new(
                "duplicate quotes — prefix disambiguates to the second",
                "the cat sat. the dog sat. done",
                sel("sat", Some("the dog "), None),
                Some((21, How::Context)),
            ),
            Case::new(
                "duplicate quotes — suffix disambiguates to the second",
                "x: value one. x: value two.",
                sel("x: value", None, Some(" two.")),
                Some((14, How::Context)),
            ),
            Case::new(
                "duplicate quotes — prefix+suffix together pick the middle one",
                "a ref b. c ref d. e ref f.",
                sel("ref", Some("c "), Some(" d.")),
                Some((11, How::Context)),
            ),
            Case::new(
                "no match — quote is simply gone",
                "nothing to see here",
                sel("absent", Some("no"), Some("pe")),
                None,
            ),
            Case::new(
                "empty prefix opts out of the prefix check; suffix still decides",
                "aa ZZ bb ZZ cc",
                sel("ZZ", Some(""), Some(" cc")),
                Some((9, How::Context)),
            ),
            Case::new(
                "absent prefix behaves exactly like an empty one",
                "aa ZZ bb ZZ cc",
                sel("ZZ", None, Some(" cc")),
                Some((9, How::Context)),
            ),
            Case::new(
                "a quote at the very start has no prefix to check",
                "ZZ leads here",
                sel("ZZ", Some(""), Some(" leads")),
                Some((0, How::Context)),
            ),
            Case::new(
                "a quote at the very end has no suffix to check",
                "trails with ZZ",
                sel("ZZ", Some("with "), Some("")),
                Some((12, How::Context)),
            ),
        ];

        for Case {
            name,
            text,
            selector,
            expect,
        } in cases
        {
            let got = resolve(text, &selector);
            assert_eq!(
                got.as_ref().map(|a| (a.units.start, a.how)),
                expect,
                "case: {name}"
            );
            // Whenever a match is reported the range must actually cover the
            // quote: a correct offset with a wrong length would highlight the
            // wrong span, and an offset-only assertion would never notice.
            if let Some(a) = got {
                let bytes = a.byte_range(text).expect("ascii case has byte bounds");
                assert_eq!(&text[bytes], selector.exact, "case: {name}");
            }
        }
    }

    /// Only the last [`CONTEXT_UNITS`] of a long prefix are compared, because
    /// that is all `captureSelector` recorded and all the haystack slice offers.
    #[test]
    fn a_prefix_longer_than_the_window_matches_on_its_tail() {
        let text = format!("{}TARGET tail", "x".repeat(50));
        let prefix = format!("{}{}", "y".repeat(20), "x".repeat(CONTEXT_UNITS));
        let got = resolve(&text, &sel("TARGET", Some(&prefix), None)).unwrap();
        assert_eq!(got.units.start, 50);
        assert_eq!(got.how, How::Context);
    }

    /// The known-wrong fallback, asserted deliberately. Several occurrences and
    /// none agreeing with the recorded context yields the *first* — very likely
    /// the wrong one.
    ///
    /// The JS original returned a bare index here, indistinguishable from a
    /// confident hit, so this asserts the new discrimination as much as the
    /// position.
    #[test]
    fn failed_disambiguation_falls_back_to_the_first_hit_and_says_so() {
        let text = "one HIT two HIT three HIT four";
        let got = resolve(text, &sel("HIT", Some("ZZZZ "), Some(" QQQQ"))).unwrap();
        assert_eq!(got.units.start, 4, "first occurrence");
        assert_eq!(
            got.how,
            How::Fallback,
            "a context-less fallback must be distinguishable from a real match"
        );

        // Contrast: context that does agree reports the third occurrence and
        // marks it trustworthy. Same haystack, so the selector is the only
        // difference — which is exactly what `How` reports on.
        let ok = resolve(text, &sel("HIT", Some("three "), None)).unwrap();
        assert_eq!(ok.units.start, 22);
        assert_eq!(ok.how, How::Context);
    }

    /// 16 emoji fill the window exactly (2 code units each), so the recorded
    /// prefix and the haystack slice are the same 32 units.
    #[test]
    fn an_astral_plane_prefix_is_compared_by_code_units() {
        let text = format!("lead {}TARGET trail", "🎯".repeat(16));
        let start = encode(&text).len() - encode("TARGET trail").len();
        let full = encode(&text)[..start].to_vec();
        assert!(full.len() > CONTEXT_UNITS);
        let recorded = &full[full.len() - CONTEXT_UNITS..];

        let got = resolve_utf16(&text, "TARGET", Some(recorded), None).unwrap();
        assert_eq!(got.units.start, start);
        assert_eq!(got.how, How::Context);
    }

    /// The case that forces `[u16]` end to end. A 32-unit window over a run of
    /// 2-unit emoji always lands on a pair boundary — both are even — so a split
    /// needs an odd number of 1-unit characters after the run. 20 emoji plus one
    /// puts the cut at offset 9, inside emoji #5, leaving an unpaired low
    /// surrogate at the front of the recorded prefix.
    ///
    /// The browser produces exactly this and the JS original matched on it. It
    /// has no `&str` spelling (`String::from_utf16` rejects it) and no UTF-8
    /// encoding, which is why context crosses the wasm boundary as code units.
    #[test]
    fn a_prefix_starting_on_a_lone_surrogate_still_anchors() {
        let text = format!("{}aTARGET", "🎯".repeat(20));
        let hay = encode(&text);
        let start = hay.len() - encode("TARGET").len();
        assert_eq!(start, 41, "20 emoji × 2 units + 'a'");

        let recorded = &hay[start - CONTEXT_UNITS..start];
        assert_eq!(recorded.len(), CONTEXT_UNITS);
        assert!(
            (0xDC00..=0xDFFF).contains(&recorded[0]),
            "expected an unpaired low surrogate at the window edge, got {:x}",
            recorded[0]
        );
        assert!(
            String::from_utf16(recorded).is_err(),
            "this prefix must be unrepresentable as a Rust String"
        );

        let got = resolve_utf16(&text, "TARGET", Some(recorded), None).unwrap();
        assert_eq!(
            got.units.start, start,
            "a prefix opening mid-surrogate must still match"
        );
        assert_eq!(got.how, How::Context);
    }

    /// The corollary: a selector like the one above cannot round-trip through
    /// the daemon, because JSON carries it as a lone `\udfaf` escape and
    /// `serde_json` refuses it. Asserted so the 400 is a documented boundary
    /// rather than a mystery — and so nobody "simplifies" the wasm signature
    /// back to `&str` reasoning that stored selectors are always well-formed:
    /// staged drafts are highlighted before they are ever saved.
    #[test]
    fn a_lone_surrogate_prefix_cannot_survive_json() {
        let wire = r#"{"exact":"TARGET","prefix":"\udfaf"}"#;
        let err = serde_json::from_str::<TextQuoteSelector>(wire)
            .expect_err("serde_json must reject an unpaired surrogate escape");
        assert!(
            err.to_string().contains("surrogate"),
            "unexpected error: {err}"
        );
    }

    /// No Unicode normalization anywhere, so a selector recorded against one
    /// form does not match text stored in the other. Asserted so the limitation
    /// is documented rather than discovered by a reviewer whose comment vanished.
    #[test]
    fn combining_marks_are_matched_as_written_not_normalized() {
        let decomposed = "cafe\u{301} menu";
        let precomposed = "caf\u{e9} menu";
        assert!(resolve(decomposed, &sel("caf\u{e9}", None, None)).is_none());
        assert_eq!(
            resolve(precomposed, &sel("caf\u{e9}", None, None))
                .unwrap()
                .units
                .start,
            0
        );
    }

    /// The JS original spins forever on this input (see `resolve_utf16`). The
    /// port refuses instead — a divergence worth having, because the browser
    /// calls straight into here and a hang would freeze the reviewer's tab.
    #[test]
    fn an_empty_quote_is_unanchorable_rather_than_an_infinite_loop() {
        assert!(resolve("any text at all", &sel("", Some("ZZZZ"), None)).is_none());
        assert!(resolve("any text at all", &sel("", None, None)).is_none());
        assert!(resolve("", &sel("", None, None)).is_none());
    }

    /// A quote longer than the document, and an empty document, must report
    /// absence rather than panicking on an out-of-range slice — `windows()`
    /// yields nothing when the size exceeds the slice, so both edges route
    /// through `find`.
    #[test]
    fn a_quote_longer_than_the_haystack_is_absent() {
        assert!(resolve("short", &sel("much longer than that", None, None)).is_none());
        assert!(resolve("", &sel("anything", None, None)).is_none());
    }

    #[test]
    fn utf16_offsets_map_back_to_byte_offsets() {
        let s = "a🎯b";
        assert_eq!(utf16_to_byte(s, 0), Some(0));
        assert_eq!(utf16_to_byte(s, 1), Some(1)); // start of the emoji
        assert_eq!(
            utf16_to_byte(s, 2),
            None,
            "the low half of a surrogate pair has no byte position"
        );
        assert_eq!(utf16_to_byte(s, 3), Some(5)); // 'b', after 4 emoji bytes
        assert_eq!(utf16_to_byte(s, 4), Some(6)); // one past the end
        assert_eq!(utf16_to_byte(s, 5), None);
    }

    /// A byte range over astral-plane text has to slice the quote exactly, which
    /// is the whole reason `byte_range` exists.
    #[test]
    fn byte_range_slices_the_quote_out_of_astral_text() {
        let text = "🎯 hit the TARGET 🎯";
        let a = resolve(text, &sel("TARGET", None, None)).unwrap();
        assert_eq!(&text[a.byte_range(text).unwrap()], "TARGET");
    }
}
