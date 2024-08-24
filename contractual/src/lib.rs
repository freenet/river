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
macro_rules! compose_contractual {
    ($struct_name:ident { $($field:ident: $field_type:ty),+ $(,)? }) => {
        #[derive(Serialize, Deserialize)]
        pub struct $struct_name {
            $(pub $field: $field_type),+
        }

        #[derive(Serialize, Deserialize)]
        pub struct $struct_name Summary {
            $($field: <$field_type as Contractual>::Summary),+
        }

        #[derive(Serialize, Deserialize)]
        pub struct $struct_name Delta {
            $($field: <$field_type as Contractual>::Delta),+
        }

        impl Contractual for $struct_name {
            type State = $struct_name;
            type Summary = $struct_name Summary;
            type Delta = $struct_name Delta;

            fn verify(&self, state: &Self::State) -> Result<(), String> {
                $(
                    self.$field.verify(&state.$field).map_err(|e| format!("Error in {}: {}", stringify!($field), e))?;
                )+
                Ok(())
            }

            fn summarize(&self, state: &Self::State) -> Self::Summary {
                $struct_name Summary {
                    $($field: self.$field.summarize(&state.$field)),+
                }
            }

            fn delta(&self, old_state_summary: &Self::Summary, new_state: &Self::State) -> Self::Delta {
                $struct_name Delta {
                    $($field: self.$field.delta(&old_state_summary.$field, &new_state.$field)),+
                }
            }

            fn apply_delta(&self, old_state: &Self::State, delta: &Self::Delta) -> Self::State {
                $struct_name {
                    $($field: self.$field.apply_delta(&old_state.$field, &delta.$field)),+
                }
            }
        }
    };
}

// Example usage:
// compose_contractual!(MyContract {
//     field1: ContractualType1,
//     field2: ContractualType2,
// });
