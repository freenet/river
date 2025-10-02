use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    pub default_room: Option<String>,
    pub user_key_path: Option<String>,
}

impl Config {
    pub fn load() -> Result<Self> {
        // TODO: Load from config file
        Ok(Self::default())
    }
}
