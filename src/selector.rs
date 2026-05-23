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
