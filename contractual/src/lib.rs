use serde::{Serialize, Deserialize};

trait Contractual<State, Summary, Delta>
where
    State: Serialize + Deserialize<'static>,
    Summary: Serialize + Deserialize<'static>,
    Delta: Serialize + Deserialize<'static>,
{
    /// Verify that this Contractual (which is part of [state]) is valid
    fn verify(&self, state: State) -> Result<(), String>;
    
    /// Create a compact summary of this Contractual (which is part of [state])
    fn summarize(&self, state: State) -> Summary;
    
    /// Calculate the delta between this Contractual and another contractual
    fn delta(old_state_summary: Summary, new_state: State) -> Delta;
    
    /// Apply a delta to a state to create a new state
    fn apply_delta(old_state: State, delta: Delta) -> State;
}