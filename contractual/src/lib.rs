pub mod contractuals;
pub mod util;

pub use contractual_macro::contractual;
use serde::{Serialize, Deserialize};

pub trait Contractual {
    type ParentState: Serialize + Deserialize<'static> + Clone;
    type Summary: Serialize + Deserialize<'static> + Clone;
    type Delta: Serialize + Deserialize<'static> + Clone;
    type Parameters: Serialize + Deserialize<'static> + Clone;

    fn verify(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Result<(), String>;
    fn summarize(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Self::Summary;
    fn delta(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters, old_state_summary: &Self::Summary) -> Self::Delta;
    fn apply_delta(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters, delta: &Self::Delta) -> Self;
}

#[cfg(test)]
mod tests;

