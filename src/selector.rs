use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TextQuoteSelector {
    #[serde(rename = "type", default = "default_type")]
    pub ty: String,
    pub exact: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suffix: Option<String>,
}

fn default_type() -> String {
    "TextQuoteSelector".into()
}

impl TextQuoteSelector {
    pub fn quote(&self) -> &str {
        &self.exact
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `type` is optional on the wire because the reply route and the CLI both
    /// build selectors without it — only the browser sends the full annotation
    /// shape. Dropping the `default` would 400 those callers.
    #[test]
    fn absent_type_defaults_to_text_quote_selector() {
        let s: TextQuoteSelector = serde_json::from_str(r#"{"exact":"hello"}"#).unwrap();
        assert_eq!(s.ty, "TextQuoteSelector");
        assert_eq!(s.quote(), "hello");
        // And it round-trips back with the type spelled out, so a client that
        // omitted it still reads a complete selector.
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["type"], "TextQuoteSelector");
    }

    /// An explicit `type` is preserved verbatim rather than normalized, so a
    /// future selector kind survives the round trip through the store.
    #[test]
    fn explicit_type_is_preserved() {
        let s: TextQuoteSelector =
            serde_json::from_str(r#"{"type":"TextPositionSelector","exact":"x"}"#).unwrap();
        assert_eq!(s.ty, "TextPositionSelector");
    }

    /// Absent and empty-string `prefix` are *different* values that arrive
    /// differently over the wire, and the distinction has to survive.
    ///
    /// Absent must stay absent: `skip_serializing_if = "Option::is_none"` means a
    /// missing prefix round-trips as missing rather than materializing as `""`.
    /// The anchoring code in `frontend/anchor.js` treats both as falsy, so this
    /// costs nothing there — but the *store* keeps the serialized selector
    /// verbatim, so silently inventing a field would rewrite what the reviewer
    /// actually anchored on.
    #[test]
    fn absent_and_empty_prefix_are_distinguishable_through_a_round_trip() {
        let absent: TextQuoteSelector = serde_json::from_str(r#"{"exact":"x"}"#).unwrap();
        assert_eq!(absent.prefix, None);
        assert_eq!(absent.suffix, None);
        let v = serde_json::to_value(&absent).unwrap();
        assert!(
            v.get("prefix").is_none(),
            "an absent prefix must not be serialized back as \"\""
        );

        let empty: TextQuoteSelector =
            serde_json::from_str(r#"{"exact":"x","prefix":"","suffix":""}"#).unwrap();
        assert_eq!(empty.prefix.as_deref(), Some(""));
        assert_eq!(empty.suffix.as_deref(), Some(""));
        let v = serde_json::to_value(&empty).unwrap();
        assert_eq!(
            v["prefix"], "",
            "an explicitly-empty prefix must survive as an empty string"
        );
    }

    /// A populated selector — the browser's shape — round-trips unchanged.
    #[test]
    fn full_selector_round_trips_unchanged() {
        let json = r#"{"type":"TextQuoteSelector","exact":"the quote","prefix":"before ","suffix":" after"}"#;
        let s: TextQuoteSelector = serde_json::from_str(json).unwrap();
        assert_eq!(
            serde_json::to_value(&s).unwrap(),
            serde_json::from_str::<serde_json::Value>(json).unwrap()
        );
    }
}
