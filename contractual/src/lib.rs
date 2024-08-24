use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, DeriveInput, Data, Fields};

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

#[proc_macro_attribute]
pub fn contractual(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as DeriveInput);
    let name = &input.ident;
    let fields = match &input.data {
        Data::Struct(data_struct) => {
            match &data_struct.fields {
                Fields::Named(fields_named) => &fields_named.named,
                _ => panic!("Only named fields are supported"),
            }
        },
        _ => panic!("Only structs are supported"),
    };

    let summary_fields = fields.iter().map(|f| {
        let name = &f.ident;
        let ty = &f.ty;
        quote! { #name: <#ty as Contractual>::Summary }
    });

    let delta_fields = fields.iter().map(|f| {
        let name = &f.ident;
        let ty = &f.ty;
        quote! { #name: <#ty as Contractual>::Delta }
    });

    let verify_impl = fields.iter().map(|f| {
        let name = &f.ident;
        quote! {
            self.#name.verify(&state.#name).map_err(|e| format!("Error in {}: {}", stringify!(#name), e))?;
        }
    });

    let summarize_impl = fields.iter().map(|f| {
        let name = &f.ident;
        quote! { #name: self.#name.summarize(&state.#name) }
    });

    let delta_impl = fields.iter().map(|f| {
        let name = &f.ident;
        quote! { #name: self.#name.delta(&old_state_summary.#name, &new_state.#name) }
    });

    let apply_delta_impl = fields.iter().map(|f| {
        let name = &f.ident;
        quote! { #name: self.#name.apply_delta(&old_state.#name, &delta.#name) }
    });

    let expanded = quote! {
        #input

        #[derive(Serialize, Deserialize)]
        struct #name Summary {
            #(#summary_fields),*
        }

        #[derive(Serialize, Deserialize)]
        struct #name Delta {
            #(#delta_fields),*
        }

        impl Contractual for #name {
            type State = #name;
            type Summary = #name Summary;
            type Delta = #name Delta;

            fn verify(&self, state: &Self::State) -> Result<(), String> {
                #(#verify_impl)*
                Ok(())
            }

            fn summarize(&self, state: &Self::State) -> Self::Summary {
                Self::Summary {
                    #(#summarize_impl),*
                }
            }

            fn delta(&self, old_state_summary: &Self::Summary, new_state: &Self::State) -> Self::Delta {
                Self::Delta {
                    #(#delta_impl),*
                }
            }

            fn apply_delta(&self, old_state: &Self::State, delta: &Self::Delta) -> Self::State {
                Self::State {
                    #(#apply_delta_impl),*
                }
            }
        }
    };

    TokenStream::from(expanded)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[contractual]
    #[derive(Debug, Clone, PartialEq)]
    struct TestContract {
        field1: TestField,
        field2: TestField,
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
