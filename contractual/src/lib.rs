pub use contractual_macro::contractual;
use serde::{Serialize, Deserialize};

pub trait Contractual {
    type State: Serialize + Deserialize<'static> + Clone;
    type Summary: Serialize + Deserialize<'static> + Clone;
    type Delta: Serialize + Deserialize<'static> + Clone;
    type Parameters: Serialize + Deserialize<'static> + Clone;

    fn verify(&self, state: &Self::State, parameters: &Self::Parameters) -> Result<(), String>;
    fn summarize(&self, state: &Self::State, parameters: &Self::Parameters) -> Self::Summary;
    fn delta(&self, old_state_summary: &Self::Summary, new_state: &Self::State, parameters: &Self::Parameters) -> Self::Delta;
    fn apply_delta(&self, old_state: &Self::State, delta: &Self::Delta, parameters: &Self::Parameters) -> Self::State;
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
        type State = Self;
        type Summary = i32;
        type Delta = i32;
        type Parameters = I32Parameters;

        fn verify(&self, state: &Self::State, _parameters: &Self::Parameters) -> Result<(), String> {
            Ok(())
        }

        fn summarize(&self, state: &Self::State, _parameters: &Self::Parameters) -> Self::Summary {
            state.0
        }

        fn delta(&self, _old_state_summary: &Self::Summary, new_state: &Self::State, _parameters: &Self::Parameters) -> Self::Delta {
            new_state.0
        }

        fn apply_delta(&self, _old_state: &Self::State, delta: &Self::Delta, _parameters: &Self::Parameters) -> Self::State {
            ContractualI32(*delta)
        }
    }

    #[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
    struct StringParameters;

    impl Contractual for ContractualString {
        type State = Self;
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

    impl Contractual for TestStruct {
        type State = Self;
        type Summary = TestStructSummary;
        type Delta = TestStructDelta;
        type Parameters = TestStructParameters;

        fn verify(&self, state: &Self::State, parameters: &Self::Parameters) -> Result<(), String> {
            self.number.verify(&state.number, &parameters.number)?;
            self.text.verify(&state.text, &parameters.text)?;
            Ok(())
        }

        fn summarize(&self, state: &Self::State, parameters: &Self::Parameters) -> Self::Summary {
            TestStructSummary {
                number: self.number.summarize(&state.number, &parameters.number),
                text: self.text.summarize(&state.text, &parameters.text),
            }
        }

        fn delta(&self, old_state_summary: &Self::Summary, new_state: &Self::State, parameters: &Self::Parameters) -> Self::Delta {
            TestStructDelta {
                number: self.number.delta(&old_state_summary.number, &new_state.number, &parameters.number),
                text: self.text.delta(&old_state_summary.text, &new_state.text, &parameters.text),
            }
        }

        fn apply_delta(&self, old_state: &Self::State, delta: &Self::Delta, parameters: &Self::Parameters) -> Self::State {
            TestStruct {
                number: self.number.apply_delta(&old_state.number, &delta.number, &parameters.number),
                text: self.text.apply_delta(&old_state.text, &delta.text, &parameters.text),
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
