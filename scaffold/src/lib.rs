pub mod util;
pub mod signed;

use std::fmt::Debug;
pub use freenet_scaffold_macro::composable;
use serde::{Serialize, Deserialize};
use serde::de::DeserializeOwned;

pub trait ComposableState {
    type ParentState: Serialize + DeserializeOwned + Clone + Debug;
    type Summary: Serialize + DeserializeOwned + Clone + Debug;
    type Delta: Serialize + DeserializeOwned + Clone + Debug;
    type Parameters: Serialize + DeserializeOwned + Clone + Debug;

    fn verify(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Result<(), String>;
    fn summarize(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Self::Summary;
    fn delta(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters, old_state_summary: &Self::Summary) -> Self::Delta;
    fn apply_delta(&mut self, parent_state: &Self::ParentState, parameters: &Self::Parameters, delta: &Self::Delta) -> Result<(), String>;
}

#[cfg(test)]
mod tests;

