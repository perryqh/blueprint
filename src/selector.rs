//! Re-export of the selector type, which now lives in the `blueprint-anchor`
//! member crate alongside the resolution algorithm that consumes it.
//!
//! Kept as a module so the ~dozen `crate::selector::TextQuoteSelector` paths
//! through the store, server, and review-file layers stay put; the type and its
//! serde tests moved wholesale, and `cargo test --workspace` runs them.
//!
//! `resolve` is re-exported too, for server-side callers that want to
//! re-resolve an anchor against the current HTML rather than only asking
//! whether the quote still appears verbatim.

pub use blueprint_anchor::{Anchor, How, TextQuoteSelector, resolve};
