use super::*;

// Constants
pub(crate) const KEY_INDEX_SUFFIX: &str = "::key_index";
pub(crate) const ORIGIN_KEY_SEPARATOR: &str = ":";

/// Origin contract ID - represents the attested identity of the caller
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Origin(pub(crate) Vec<u8>);

impl Origin {
    pub(crate) fn to_b58(&self) -> String {
        bs58::encode(&self.0).into_string()
    }
}
