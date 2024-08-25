extern crate proc_macro;

use proc_macro::TokenStream;
use quote::{quote, format_ident};
use syn::{parse_macro_input, DeriveInput, Data, Fields, GenericParam, TypeParam};

#[proc_macro_attribute]
pub fn contractual(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as DeriveInput);
    let name = &input.ident;
    
    let fields = match &input.data {
        Data::Struct(data_struct) => {
            match &data_struct.fields {
                Fields::Named(fields_named) => &fields_named.named,
                _ => panic!("Contractual can only be applied to structs with named fields"),
            }
        },
        _ => panic!("Contractual can only be applied to structs"),
    };

    let field_names: Vec<_> = fields.iter().map(|f| &f.ident).collect();
    let field_types: Vec<_> = fields.iter().map(|f| &f.ty).collect();

    let summary_name = format_ident!("{}Summary", name);
    let delta_name = format_ident!("{}Delta", name);

    let summary_fields = field_names.iter().zip(field_types.iter()).map(|(name, ty)| {
        quote! { #name: <#ty as Contractual>::Summary }
    });

    let delta_fields = field_names.iter().zip(field_types.iter()).map(|(name, ty)| {
        quote! { #name: <#ty as Contractual>::Delta }
    });

    let verify_impl = field_names.iter().map(|name| {
        quote! {
            self.#name.verify(&state.#name)?;
        }
    });

    let summarize_impl = field_names.iter().map(|name| {
        quote! { #name: self.#name.summarize(&state.#name) }
    });

    let delta_impl = field_names.iter().map(|name| {
        quote! { #name: self.#name.delta(&old_state_summary.#name, &new_state.#name) }
    });

    let apply_delta_impl = field_names.iter().map(|name| {
        quote! { #name: self.#name.apply_delta(&old_state.#name, &delta.#name) }
    });

    let generic_params: Vec<_> = input.generics.params.iter().collect();
    let where_clause = input.generics.where_clause.clone();
    let (impl_generics, ty_generics, _) = input.generics.split_for_impl();

    let contractual_bounds = field_types.iter().map(|ty| {
        quote! { #ty: Contractual }
    });

    let expanded = quote! {
        #input

        #[derive(serde::Serialize, serde::Deserialize, Clone)]
        pub struct #summary_name #ty_generics #where_clause {
            #(#summary_fields,)*
        }

        #[derive(serde::Serialize, serde::Deserialize, Clone)]
        pub struct #delta_name #ty_generics #where_clause {
            #(#delta_fields,)*
        }

        impl #impl_generics Contractual for #name #ty_generics #where_clause
        where
            #(#contractual_bounds,)*
        {
            type State = #name #ty_generics;
            type Summary = #summary_name #ty_generics;
            type Delta = #delta_name #ty_generics;

            fn verify(&self, state: &Self::State) -> Result<(), String> {
                #(#verify_impl)*
                Ok(())
            }

            fn summarize(&self, state: &Self::State) -> Self::Summary {
                #summary_name {
                    #(#summarize_impl,)*
                }
            }

            fn delta(&self, old_state_summary: &Self::Summary, new_state: &Self::State) -> Self::Delta {
                #delta_name {
                    #(#delta_impl,)*
                }
            }

            fn apply_delta(&self, old_state: &Self::State, delta: &Self::Delta) -> Self::State {
                #name {
                    #(#apply_delta_impl,)*
                }
            }
        }
    };

    println!("{}", expanded);
    
    TokenStream::from(expanded)
}