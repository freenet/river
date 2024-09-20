pub mod util;

use std::fmt::Debug;
pub use freenet_scaffold_macro::composable;
use serde::Serialize;
use serde::de::DeserializeOwned;

pub trait ComposableState {
    type ParentState: Serialize + DeserializeOwned + Clone + Debug;
    type Summary: Serialize + DeserializeOwned + Clone + Debug;
    type Delta: Serialize + DeserializeOwned + Clone + Debug;
    type Parameters: Serialize + DeserializeOwned + Clone + Debug;

    fn verify(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Result<(), String>;
    fn summarize(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Self::Summary;
    fn delta(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters, old_state_summary: &Self::Summary) -> Option<Self::Delta>;
    fn apply_delta(&mut self, parent_state: &Self::ParentState, parameters: &Self::Parameters, delta: &Self::Delta) -> Result<(), String>;
    fn merge(&mut self, parent_state: &Self::ParentState, parameters : &Self::Parameters, other_state : &Self) -> Result<(), String> {
        let my_summary = self.summarize(parent_state, parameters);
        let delta_in = other_state.delta(parent_state, parameters, &my_summary);
        match delta_in {
            Some(delta) => {
                self.apply_delta(parent_state, parameters, &delta)?;
            },
            None => {
                // No delta, so nothing to do
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;

