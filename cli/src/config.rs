use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub default_room: Option<String>,
    pub user_key_path: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_room: None,
            user_key_path: None,
        }
    }
}

impl Config {
    pub fn load() -> Result<Self> {
        // TODO: Load from config file
        Ok(Self::default())
    }
}