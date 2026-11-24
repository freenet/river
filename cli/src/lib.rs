// Test code stamps arbitrary times; the rule is for what production signs (freenet/river#971).
#![cfg_attr(test, allow(clippy::disallowed_methods))]
pub mod admission;
pub mod api;
pub mod commands;
pub mod config;
pub mod deputies;
pub mod error;
pub mod output;
pub mod pointer;
pub mod private_room;
pub mod signed_time;
pub mod storage;
pub mod version_check;
