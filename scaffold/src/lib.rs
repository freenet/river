pub mod util;

pub use freenet_scaffold_macro::*;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fmt::Debug;

pub trait ComposableState {
    type ParentState: Serialize + DeserializeOwned + Clone + Debug;
    type Summary: Serialize + DeserializeOwned + Clone + Debug;
    type Delta: Serialize + DeserializeOwned + Clone + Debug;
    type Parameters: Serialize + DeserializeOwned + Clone + Debug;

    fn verify(
        &self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
    ) -> Result<(), String>;
    fn summarize(
        &self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
    ) -> Self::Summary;
    fn delta(
        &self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
        old_state_summary: &Self::Summary,
    ) -> Option<Self::Delta>;

    /// Applies the specified `delta` to the current state.
    ///
    /// If delta is None then this function should still make any changes that might be
    /// required based on other fields in the parent_state which may have changed.
    fn apply_delta(
        &mut self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
        delta: &Option<Self::Delta>,
    ) -> Result<(), String>;

    /// Merges the current state with another state.
    fn merge(
        &mut self,
        parent_state: &Self::ParentState,
        parameters: &Self::Parameters,
        other_state: &Self,
    ) -> Result<(), String> {
        let my_summary = self.summarize(parent_state, parameters);
        let delta_in = other_state.delta(parent_state, parameters, &my_summary);
        self.apply_delta(parent_state, parameters, &delta_in)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
