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

#[cfg(test)]
mod tests {
    use super::*;

    #[contractual]
    #[derive(Debug, PartialEq, Serialize, Deserialize)]
    struct TestStruct {
        value: i32,
    }

    impl TestStruct {
        fn new(value: i32) -> Self {
            TestStruct { value }
        }
    }

    #[test]
    fn test_contractual_macro() {
        let test_struct = TestStruct::new(42);
        let state = TestStruct::new(42);

        // Test verify
        assert!(test_struct.verify(&state).is_ok());

        // Test summarize
        let summary = test_struct.summarize(&state);
        assert_eq!(summary.value, 42);

        // Test delta
        let new_state = TestStruct::new(84);
        let delta = test_struct.delta(&summary, &new_state);
        assert_eq!(delta.value, 84);

        // Test apply_delta
        let updated_state = test_struct.apply_delta(&state, &delta);
        assert_eq!(updated_state, new_state);
    }
}
