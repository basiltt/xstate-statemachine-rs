//! `#[derive(TypedState)]`.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, Ident, LitStr, Variant};

use crate::case::Case;

/// Enum-level `#[state(...)]` options.
#[derive(Default)]
struct EnumOptions {
    machine: Option<String>,
    rename_all: Option<Case>,
}

/// Expand the derive.
pub(crate) fn derive(input: &DeriveInput) -> syn::Result<TokenStream> {
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "TypedState can only be derived for enums",
        ));
    };
    crate::reject_generics(&input.generics, "TypedState")?;
    let options = enum_options(input)?;
    let mut states: Vec<(&Ident, String)> = Vec::with_capacity(data.variants.len());
    for variant in &data.variants {
        let id = variant_id(variant, &options)?;
        if states.iter().any(|(_, prev)| *prev == id) {
            let msg = format!("state id `{id}` is named by more than one variant");
            return Err(syn::Error::new_spanned(&variant.ident, msg));
        }
        states.push((&variant.ident, id));
    }
    let krate = crate::krate::resolve(crate::krate::from_attrs(&input.attrs)?.as_ref());
    Ok(expand(input, &states, &krate))
}

fn enum_options(input: &DeriveInput) -> syn::Result<EnumOptions> {
    let mut options = EnumOptions::default();
    for attr in input.attrs.iter().filter(|a| a.path().is_ident("state")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("machine") {
                let lit: LitStr = meta.value()?.parse()?;
                if lit.value().is_empty() {
                    return Err(syn::Error::new(lit.span(), "machine id must not be empty"));
                }
                options.machine = Some(lit.value());
            } else if meta.path.is_ident("rename_all") {
                options.rename_all = Some(Case::from_lit(&meta.value()?.parse()?)?);
            } else {
                let msg = "unknown enum attribute; expected `machine = \"...\"` or \
                           `rename_all = \"...\"`";
                return Err(meta.error(msg));
            }
            Ok(())
        })?;
    }
    Ok(options)
}

fn variant_id(variant: &Variant, options: &EnumOptions) -> syn::Result<String> {
    if !matches!(variant.fields, Fields::Unit) {
        let msg = "TypedState variants must be unit variants naming a state";
        return Err(syn::Error::new_spanned(&variant.fields, msg));
    }
    let mut id = None;
    for attr in variant.attrs.iter().filter(|a| a.path().is_ident("state")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("id") {
                let lit: LitStr = meta.value()?.parse()?;
                if lit.value().is_empty() {
                    return Err(syn::Error::new(lit.span(), "state id must not be empty"));
                }
                id = Some(lit.value());
                Ok(())
            } else {
                Err(meta.error("unknown variant attribute; expected `id = \"...\"`"))
            }
        })?;
    }
    if let Some(id) = id {
        return Ok(id);
    }
    let Some(machine) = &options.machine else {
        let msg = "no state id: add `#[state(id = \"machine.path\")]` here or \
                   `#[state(machine = \"...\")]` on the enum";
        return Err(syn::Error::new_spanned(&variant.ident, msg));
    };
    let key = options.rename_all.unwrap_or(Case::Snake).apply(&variant.ident.to_string());
    Ok(format!("{machine}.{key}"))
}

fn expand(input: &DeriveInput, states: &[(&Ident, String)], krate: &TokenStream) -> TokenStream {
    let (name, vis) = (&input.ident, &input.vis);
    let idents: Vec<&Ident> = states.iter().map(|(ident, _)| *ident).collect();
    let ids: Vec<&str> = states.iter().map(|(_, id)| id.as_str()).collect();
    let (queries, trait_impl) = (queries(vis, krate), trait_impl(name, krate));
    quote! {
        #[automatically_derived]
        impl #name {
            /// Full state id this variant stands for.
            #[must_use]
            #vis const fn id(&self) -> &'static str {
                match *self { #(Self::#idents => #ids,)* }
            }

            /// The variant whose full state id is `id`.
            #[must_use]
            #vis fn from_id(id: &str) -> ::core::option::Option<Self> {
                match id {
                    #(#ids => ::core::option::Option::Some(Self::#idents),)*
                    _ => ::core::option::Option::None,
                }
            }

            /// Every variant, in declaration order.
            #[must_use]
            #vis fn all() -> &'static [Self] {
                const ALL: &[#name] = &[#(#name::#idents),*];
                ALL
            }

            #queries
        }

        #trait_impl
    }
}

/// `impl TypedState`, delegating to the inherent items.
fn trait_impl(name: &Ident, krate: &TokenStream) -> TokenStream {
    quote! {
        #[automatically_derived]
        impl #krate::TypedState for #name {
            fn id(&self) -> &'static str {
                #name::id(self)
            }

            fn from_id(id: &str) -> ::core::option::Option<Self> {
                #name::from_id(id)
            }

            fn all() -> &'static [Self] {
                #name::all()
            }
        }
    }
}

/// The inherent `is_active` and `check`.
fn queries(vis: &syn::Visibility, krate: &TokenStream) -> TokenStream {
    quote! {
        /// `true` when this state is active in `interp`.
        #[must_use]
        #vis fn is_active<C: #krate::Context>(
            &self,
            interp: &#krate::SyncInterpreter<C>,
        ) -> bool {
            interp.matches(self.id())
        }

        /// Compare this enum with the leaf states of `machine`.
        ///
        /// # Errors
        /// Describes ids that are not states of the chart, ids of non-leaf states and leaf
        /// states the enum does not cover.
        #vis fn check(
            machine: &#krate::Machine,
        ) -> ::core::result::Result<(), ::std::string::String> {
            <Self as #krate::TypedState>::check(machine)
        }
    }
}
