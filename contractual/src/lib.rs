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
    fn apply_delta(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters, delta: &Self::Delta) -> Self::State;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
    struct ContractualI32(i32);

    #[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
    struct ContractualString(String);

    #[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
    struct I32Parameters;

    impl Contractual for ContractualI32 {
        type ParentState = Self;
        type Summary = i32;
        type Delta = i32;
        type Parameters = I32Parameters;

        fn verify(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters) -> Result<(), String> {
            Ok(())
        }

        fn summarize(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters) -> Self::Summary {
            self.0
        }

        fn delta(&self, _parent_state: Self::ParentState, _parameters: &Self::Parameters, _old_state_summary : &Self::Summary) -> Self::Delta {
            self.0
        }

        fn apply_delta(&self, _old_state: &Self::State, delta: &Self::Delta, _parameters: &Self::Parameters) -> Self::State {
            ContractualI32(*delta)
        }
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
    struct StringParameters;

    impl Contractual for ContractualString {
        type ParentState = Self;
        type Summary = String;
        type Delta = String;
        type Parameters = StringParameters;

        fn verify(&self, state: &Self::State, _parameters: &Self::Parameters) -> Result<(), String> {
            Ok(())
        }

        fn summarize(&self, state: &Self::State, _parameters: &Self::Parameters) -> Self::Summary {
            state.0.clone()
        }

        fn delta(&self, _old_state_summary: &Self::Summary, new_state: &Self::State, _parameters: &Self::Parameters) -> Self::Delta {
            new_state.0.clone()
        }

        fn apply_delta(&self, _old_state: &Self::State, delta: &Self::Delta, _parameters: &Self::Parameters) -> Self::State {
            ContractualString(delta.clone())
        }
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
    struct TestStructParameters {
        number: I32Parameters,
        text: StringParameters,
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
        let parameters = TestStructParameters {
            number: I32Parameters,
            text: StringParameters,
        };

        // Test verify
        assert!(test_struct.verify(&state, &parameters).is_ok());

        // Test summarize
        let summary = test_struct.summarize(&state, &parameters);
        assert_eq!(summary.number, 42);
        assert_eq!(summary.text, "hello");

        // Test delta
        let new_state = TestStruct::new(84, "world");
        let delta = test_struct.delta(&summary, &new_state, &parameters);
        assert_eq!(delta.number, 84);
        assert_eq!(delta.text, "world");

        // Test apply_delta
        let updated_state = test_struct.apply_delta(&state, &delta, &parameters);
        assert_eq!(updated_state, new_state);
    }
}
