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

