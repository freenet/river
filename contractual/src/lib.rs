pub use contractual_macro::contractual;
use serde::{Serialize, Deserialize};

pub trait Contractual {
    type State: Serialize + Deserialize<'static>;
    type Summary: Serialize + Deserialize<'static>;
    type Delta: Serialize + Deserialize<'static>;

    fn verify(&self, state: &Self::State) -> Result<(), String>;
    fn summarize(&self, state: &Self::State) -> Self::Summary;
    fn delta(&self, old_state_summary: &Self::Summary, new_state: &Self::State) -> Self::Delta;
    fn apply_delta(&self, old_state: &Self::State, delta: &Self::Delta) -> Self::State;
}
