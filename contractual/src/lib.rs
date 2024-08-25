pub use contractual_macro::contractual;
use serde::{Serialize, Deserialize};

pub trait Contractual {
    type State: Serialize + Deserialize<'static> + Clone;
    type Summary: Serialize + Deserialize<'static> + Clone;
    type Delta: Serialize + Deserialize<'static> + Clone;

    fn verify(&self, state: &Self::State) -> Result<(), String>;
    fn summarize(&self, state: &Self::State) -> Self::Summary;
    fn delta(&self, old_state_summary: &Self::Summary, new_state: &Self::State) -> Self::Delta;
    fn apply_delta(&self, old_state: &Self::State, delta: &Self::Delta) -> Self::State;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
    struct ContractualI32(i32);

    impl Contractual for ContractualI32 {
        type State = Self;
        type Summary = i32;
        type Delta = i32;

        fn verify(&self, state: &Self::State) -> Result<(), String> {
            Ok(())
        }

        fn summarize(&self, state: &Self::State) -> Self::Summary {
            state.0
        }

        fn delta(&self, _old_state_summary: &Self::Summary, new_state: &Self::State) -> Self::Delta {
            new_state.0
        }

        fn apply_delta(&self, _old_state: &Self::State, delta: &Self::Delta) -> Self::State {
            ContractualI32(*delta)
        }
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
    struct ContractualString(String);

    impl Contractual for ContractualString {
        type State = Self;
        type Summary = String;
        type Delta = String;

        fn verify(&self, state: &Self::State) -> Result<(), String> {
            Ok(())
        }

        fn summarize(&self, state: &Self::State) -> Self::Summary {
            state.0.clone()
        }

        fn delta(&self, _old_state_summary: &Self::Summary, new_state: &Self::State) -> Self::Delta {
            new_state.0.clone()
        }

        fn apply_delta(&self, _old_state: &Self::State, delta: &Self::Delta) -> Self::State {
            ContractualString(delta.clone())
        }
    }

    #[contractual]
    #[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
    struct TestStruct {
        number: ContractualI32,
        text: ContractualString,
    }

    impl TestStruct {
        fn new(number: i32, text: &str) -> Self {
            TestStruct {
                number: ContractualI32(number),
                text: ContractualString(text.to_string()),
            }
        }
    }

    #[test]
    fn test_contractual_macro() {
        let test_struct = TestStruct::new(42, "hello");
        let state = TestStruct::new(42, "hello");

        // Test verify
        assert!(test_struct.verify(&state).is_ok());

        // Test summarize
        let summary = test_struct.summarize(&state);
        assert_eq!(summary.number, 42);
        assert_eq!(summary.text, "hello");

        // Test delta
        let new_state = TestStruct::new(84, "world");
        let delta = test_struct.delta(&summary, &new_state);
        assert_eq!(delta.number, 84);
        assert_eq!(delta.text, "world");

        // Test apply_delta
        let updated_state = test_struct.apply_delta(&state, &delta);
        assert_eq!(updated_state, new_state);
    }
}
