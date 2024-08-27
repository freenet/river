extern crate proc_macro;

use proc_macro::TokenStream;
use quote::{quote, format_ident};
use syn::{parse_macro_input, DeriveInput, Data, Fields, GenericParam, TypeParam};

#[proc_macro_attribute]
pub fn composable(_attr: TokenStream, item: TokenStream) -> TokenStream {
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
    let parameters_name = format_ident!("{}Parameters", name);

    let summary_fields = field_names.iter().zip(field_types.iter()).map(|(name, ty)| {
        quote! { #name: <#ty as Contractual>::Summary }
    });

    let delta_fields = field_names.iter().zip(field_types.iter()).map(|(name, ty)| {
        quote! { #name: <#ty as Contractual>::Delta }
    });

    let parameters_fields = field_names.iter().zip(field_types.iter()).map(|(name, ty)| {
        quote! { #name: <#ty as Contractual>::Parameters }
    });

    let verify_impl = field_names.iter().map(|name| {
        quote! {
            self.#name.verify(&self.#name, &parameters.#name)?;
        }
    });

    let summarize_impl = field_names.iter().map(|name| {
        quote! { #name: self.#name.summarize(&self.#name, &parameters.#name) }
    });

    let delta_impl = field_names.iter().map(|name| {
        quote! { #name: self.#name.delta(&self.#name, &parameters.#name, &old_state_summary.#name) }
    });

    let apply_delta_impl = field_names.iter().map(|name| {
        quote! { #name: self.#name.apply_delta(&self.#name, &parameters.#name, &delta.#name) }
    });

    let generic_params: Vec<_> = input.generics.params.iter().collect();
    let where_clause = input.generics.where_clause.clone();
    let (impl_generics, ty_generics, _) = input.generics.split_for_impl();

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

        #[derive(serde::Serialize, serde::Deserialize, Clone)]
        pub struct #parameters_name #ty_generics #where_clause {
            #(#parameters_fields,)*
        }

        impl #impl_generics ComposableState for #name #ty_generics #where_clause
        where
            #(#field_types: ComposableState,)*
        {
            type ParentState = Self;
            type Summary = #summary_name #ty_generics;
            type Delta = #delta_name #ty_generics;
            type Parameters = #parameters_name #ty_generics;

            fn verify(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Result<(), String> {
                #(#verify_impl)*
                Ok(())
            }

            fn summarize(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Self::Summary {
                #summary_name {
                    #(#summarize_impl,)*
                }
            }

            fn delta(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters, old_state_summary: &Self::Summary) -> Self::Delta {
                #delta_name {
                    #(#delta_impl,)*
                }
            }

            fn apply_delta(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters, delta: &Self::Delta) -> Self {
                #name {
                    #(#apply_delta_impl,)*
                }
            }
        }
    };

    TokenStream::from(expanded)
}
