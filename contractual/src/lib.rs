use serde::{Serialize, Deserialize};

pub trait Contractual {
    type State: Serialize + Deserialize<'static>;
    type Summary: Serialize + Deserialize<'static>;
    type Delta: Serialize + Deserialize<'static>;

    /// Verify that this Contractual (which is part of [state]) is valid
    fn verify(&self, state: &Self::State) -> Result<(), String>;
    
    /// Create a compact summary of this Contractual (which is part of [state])
    fn summarize(&self, state: &Self::State) -> Self::Summary;
    
    /// Calculate the delta between an old state summary and a new state
    fn delta(&self, old_state_summary: &Self::Summary, new_state: &Self::State) -> Self::Delta;
    
    /// Apply a delta to a state to create a new state
    fn apply_delta(&self, old_state: &Self::State, delta: &Self::Delta) -> Self::State;
}

#[macro_export]
macro_rules! contractual {
    (
        $(#[$meta:meta])*
        $vis:vis struct $name:ident {
            $(
                $(#[$field_meta:meta])*
                $field_vis:vis $field:ident: $field_type:ty
            ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis struct $name {
            $(
                $(#[$field_meta])*
                $field_vis $field: $field_type
            ),+
        }

        #[derive(Serialize, Deserialize)]
        $vis struct $name {
            $($field: <$field_type as Contractual>::Summary),+
        }

        #[derive(Serialize, Deserialize)]
        $vis struct $name {
            $($field: <$field_type as Contractual>::Delta),+
        }

        impl Contractual for $name {
            type State = $name;
            type Summary = $name;
            type Delta = $name;

            fn verify(&self, state: &Self::State) -> Result<(), String> {
                $(
                    self.$field.verify(&state.$field).map_err(|e| format!("Error in {}: {}", stringify!($field), e))?;
                )+
                Ok(())
            }

            fn summarize(&self, state: &Self::State) -> Self::Summary {
                Self::Summary {
                    $($field: self.$field.summarize(&state.$field)),+
                }
            }

            fn delta(&self, old_state_summary: &Self::Summary, new_state: &Self::State) -> Self::Delta {
                Self::Delta {
                    $($field: self.$field.delta(&old_state_summary.$field, &new_state.$field)),+
                }
            }

            fn apply_delta(&self, old_state: &Self::State, delta: &Self::Delta) -> Self::State {
                Self::State {
                    $($field: self.$field.apply_delta(&old_state.$field, &delta.$field)),+
                }
            }
        }
    };
}

// Example usage:
// contractual! {
//     #[derive(Debug, Clone)]
//     pub struct MyContract {
//         pub field1: ContractualType1,
//         pub field2: ContractualType2,
//     }
// }

#[cfg(test)]
mod tests {
    use super::*;

    // A simple Contractual type for testing
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct TestField(i32);

    impl Contractual for TestField {
        type State = Self;
        type Summary = i32;
        type Delta = i32;

        fn verify(&self, state: &Self::State) -> Result<(), String> {
            if self.0 == state.0 {
                Ok(())
            } else {
                Err("Verification failed".to_string())
            }
        }

        fn summarize(&self, _state: &Self::State) -> Self::Summary {
            self.0
        }

        fn delta(&self, old_state_summary: &Self::Summary, new_state: &Self::State) -> Self::Delta {
            new_state.0 - old_state_summary
        }

        fn apply_delta(&self, old_state: &Self::State, delta: &Self::Delta) -> Self::State {
            TestField(old_state.0 + delta)
        }
    }

    contractual! {
        #[derive(Debug, Clone, PartialEq)]
        struct TestContract {
            field1: TestField,
            field2: TestField,
        }
    }

    #[test]
    fn test_contractual_macro() {
        let contract = TestContract {
            field1: TestField(10),
            field2: TestField(20),
        };

        let state = TestContract {
            field1: TestField(10),
            field2: TestField(20),
        };

        // Test verify
        assert!(contract.verify(&state).is_ok());

        // Test summarize
        let summary = contract.summarize(&state);
        assert_eq!(summary.field1, 10);
        assert_eq!(summary.field2, 20);

        // Test delta
        let new_state = TestContract {
            field1: TestField(15),
            field2: TestField(25),
        };
        let delta = contract.delta(&summary, &new_state);
        assert_eq!(delta.field1, 5);
        assert_eq!(delta.field2, 5);

        // Test apply_delta
        let updated_state = contract.apply_delta(&state, &delta);
        assert_eq!(updated_state, new_state);
    }
}
