//! The browser-facing surface. Built only under the `wasm` feature, which only
//! wasm-pack turns on — the daemon links this crate as a plain rlib and never
//! compiles wasm-bindgen at all.

use crate::{CONTEXT_UNITS, How, resolve_utf16};
use wasm_bindgen::prelude::*;

/// A resolved anchor, as seen from JS.
///
/// A struct rather than a bare index because the caller needs to know *how* the
/// match was found — see [`How::Fallback`]. Returned as `Resolved | undefined`,
/// so a missing quote reads as falsy on the JS side.
#[wasm_bindgen]
pub struct Resolved {
    start: u32,
    end: u32,
    fallback: bool,
}

#[wasm_bindgen]
impl Resolved {
    /// Start of the match, in UTF-16 code units — directly comparable to a JS
    /// string index, which is what `highlightQuote` walks text nodes against.
    #[wasm_bindgen(getter)]
    pub fn start(&self) -> u32 {
        self.start
    }

    /// End of the match, exclusive, in UTF-16 code units.
    #[wasm_bindgen(getter)]
    pub fn end(&self) -> u32 {
        self.end
    }

    /// True when no occurrence agreed with the recorded context and this is
    /// merely the first one — very likely the wrong paragraph. The JS original
    /// could not report this: it returned a bare index either way.
    #[wasm_bindgen(getter)]
    pub fn fallback(&self) -> bool {
        self.fallback
    }
}

/// Resolve a quote against `haystack`, the flattened text of the document.
///
/// `prefix`/`suffix` arrive as `Uint16Array` rather than strings on purpose: the
/// context window is a fixed number of UTF-16 code units and can bisect a
/// surrogate pair, and a lone surrogate has no UTF-8 encoding to cross a `&str`
/// boundary with — wasm-bindgen would substitute `U+FFFD` and the prefix would
/// quietly stop matching. See the crate docs.
///
/// `haystack` and `exact` stay strings because both come from well-formed
/// sources: parsed document text, and `Selection::toString()`.
///
/// Pass `undefined` (or an empty array) for context that was not recorded; that
/// disables the corresponding half of disambiguation, matching how the daemon
/// omits absent fields.
#[wasm_bindgen(js_name = resolveQuote)]
pub fn resolve_quote(
    haystack: &str,
    exact: &str,
    prefix: Option<Vec<u16>>,
    suffix: Option<Vec<u16>>,
) -> Option<Resolved> {
    resolve_utf16(haystack, exact, prefix.as_deref(), suffix.as_deref()).map(|a| Resolved {
        start: a.units.start as u32,
        end: a.units.end as u32,
        fallback: a.how == How::Fallback,
    })
}

/// Width of the context window, in UTF-16 code units.
///
/// Exported so `captureSelector` records exactly as much context as resolution
/// compares. Duplicating the constant in JS is how the two silently disagree
/// about long prefixes.
#[wasm_bindgen(js_name = contextUnits)]
pub fn context_units() -> u32 {
    CONTEXT_UNITS as u32
}
