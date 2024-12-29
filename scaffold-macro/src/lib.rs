extern crate proc_macro;

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{parse_macro_input, Data, DeriveInput, Fields};

#[proc_macro_attribute]
pub fn composable(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as DeriveInput);
    let name = &input.ident;

    let fields = match &input.data {
        Data::Struct(data_struct) => match &data_struct.fields {
            Fields::Named(fields_named) => &fields_named.named,
            _ => panic!("ComposableState can only be applied to structs with named fields"),
        },
        _ => panic!("ComposableState can only be applied to structs"),
    };

    let field_names: Vec<_> = fields.iter().map(|f| &f.ident).collect();
    let field_types: Vec<_> = fields.iter().map(|f| &f.ty).collect();

    // Take the type of the first field to derive ParentState and Parameters
    let first_field_type = &field_types[0];

    let summary_name = format_ident!("{}Summary", name);
    let delta_name = format_ident!("{}Delta", name);

    let summary_fields = field_names
        .iter()
        .zip(field_types.iter())
        .map(|(name, ty)| {
            quote! {
                pub #name: <#ty as ComposableState>::Summary
            }
        });

    let delta_fields = field_names
        .iter()
        .zip(field_types.iter())
        .map(|(name, ty)| {
            quote! {
                pub #name: Option<<#ty as ComposableState>::Delta>
            }
        });

    // Error messages for missing ComposableState implementation
    let check_composable_impls = field_types.iter().map(|ty| {
        quote! {
            const _: fn() = || {
                fn check_composable<T: ComposableState>() {}
                check_composable::<#ty>();
            };
        }
    });

    // Ensure that all fields share the same ParentState and Parameters
    let check_matching_parent_state = field_types.iter().map(|ty| {
        quote! {
            const _: fn() = || {
                fn check_parent_state<T: ComposableState<ParentState = <#first_field_type as ComposableState>::ParentState>>() {}
                check_parent_state::<#ty>();
            };
        }
    });

    let check_matching_parameters = field_types.iter().map(|ty| {
        quote! {
            const _: fn() = || {
                fn check_parameters<T: ComposableState<Parameters = <#first_field_type as ComposableState>::Parameters>>() {}
                check_parameters::<#ty>();
            };
        }
    });

    let verify_impl = field_names.iter().map(|name| {
        quote! {
            self.#name.verify(parent_state, parameters)?;
        }
    });

    let summarize_impl = field_names.iter().map(|name| {
        quote! {
            #name: self.#name.summarize(parent_state, parameters)
        }
    });

    let delta_impl = field_names.iter().map(|name| {
        quote! {
            #name: self.#name.delta(parent_state, parameters, &old_state_summary.#name)
        }
    });

    let all_none_check = field_names
        .iter()
        .map(|name| {
            quote! {
                delta.#name.is_none()
            }
        })
        .collect::<Vec<_>>();

    // Note: we're passing self_clone as the parent_state so that dependencies between fields work
    let apply_delta_impl = field_names.iter().map(|name| {
        quote! {
            let self_clone = self.clone();
            self.#name.apply_delta(&self_clone, parameters, &delta.#name)?;
        }
    });

    let _generic_params: Vec<_> = input.generics.params.iter().collect();
    let where_clause = input.generics.where_clause.clone();
    let (impl_generics, ty_generics, _) = input.generics.split_for_impl();

    let expanded = quote! {
        use freenet_scaffold::ComposableState;

        #input

        // Automatically implement Serialize, Deserialize, Clone, PartialEq, and Debug for the generated Summary and Delta structs
        #[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq, Debug)]
        pub struct #summary_name #ty_generics #where_clause {
            #(#summary_fields,)*
        }

        #[derive(serde::Serialize, serde::Deserialize, Clone, PartialEq, Debug, Default)]
        pub struct #delta_name #ty_generics #where_clause {
            #(#delta_fields,)*
        }

        impl #impl_generics ComposableState for #name #ty_generics #where_clause
        where
            #(#field_types: ComposableState,)*
        {
            type ParentState = #name;
            type Summary = #summary_name #ty_generics;
            type Delta = #delta_name #ty_generics;
            type Parameters = <#first_field_type as ComposableState>::Parameters;

            fn verify(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Result<(), String> {
                #(#verify_impl)*
                Ok(())
            }

            fn summarize(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters) -> Self::Summary {
                #summary_name {
                    #(#summarize_impl,)*
                }
            }

            fn delta(&self, parent_state: &Self::ParentState, parameters: &Self::Parameters, old_state_summary: &Self::Summary) -> Option<Self::Delta> {
                let delta = #delta_name {
                    #(#delta_impl,)*
                };

                if #(#all_none_check)&&* {
                    None
                } else {
                    Some(delta)
                }
            }

            // parent_state disregarded because we need to use self so that dependencies between fields work, ugly
            fn apply_delta(&mut self, _parent_state: &Self::ParentState, parameters: &Self::Parameters, delta: &Option<Self::Delta>) -> Result<(), String> {
                if let Some(delta) = delta {
                    #(#apply_delta_impl)*
                }
                Ok(())
            }
        }

        // Additional checks to provide better compile-time error messages
        #(#check_composable_impls)*
        #(#check_matching_parent_state)*
        #(#check_matching_parameters)*
    };

    TokenStream::from(expanded)
}
