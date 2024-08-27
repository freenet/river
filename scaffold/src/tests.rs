use super::*;

#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
struct ContractualI32(i32);

#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
struct ContractualString(String);

#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
struct I32Parameters;

impl ComposableState for ContractualI32 {
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

    fn delta(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters, old_state_summary: &Self::Summary) -> Self::Delta {
        self.0 - old_state_summary
    }

    fn apply_delta(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters, delta: &Self::Delta) -> Self {
        ContractualI32(self.0 + delta)
    }
}

#[derive(Debug, PartialEq, Serialize, Deserialize, Clone)]
struct StringParameters;

impl ComposableState for ContractualString {
    type ParentState = Self;
    type Summary = String;
    type Delta = String;
    type Parameters = StringParameters;

    fn verify(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters) -> Result<(), String> {
        Ok(())
    }

    fn summarize(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters) -> Self::Summary {
        self.0.clone()
    }

    fn delta(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters, old_state_summary: &Self::Summary) -> Self::Delta {
        if self.0 == *old_state_summary {
            String::new()
        } else {
            self.0.clone()
        }
    }

    fn apply_delta(&self, _parent_state: &Self::ParentState, _parameters: &Self::Parameters, delta: &Self::Delta) -> Self {
        if delta.is_empty() {
            self.clone()
        } else {
            ContractualString(delta.clone())
        }
    }
}

#[composable]
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
    let parameters = TestStructParameters {
        number: I32Parameters,
        text: StringParameters,
    };

    // Test verify
    assert!(test_struct.verify(&test_struct, &parameters).is_ok());

    // Test summarize
    let summary = test_struct.summarize(&test_struct, &parameters);
    assert_eq!(summary.number, 42);
    assert_eq!(summary.text, "hello");

    // Test delta
    let new_state = TestStruct::new(84, "world");
    let delta = new_state.delta(&test_struct, &parameters, &summary);
    assert_eq!(delta.number, 42); // The delta should be the difference: 84 - 42 = 42
    assert_eq!(delta.text, "world");

    // Test apply_delta
    let updated_state = test_struct.apply_delta(&test_struct, &parameters, &delta);
    assert_eq!(updated_state, new_state);
}
