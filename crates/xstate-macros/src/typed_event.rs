//! `#[derive(TypedEvent)]`.

use proc_macro2::TokenStream;
use quote::quote;
use syn::{Data, DeriveInput, Fields, Ident, LitStr, Variant};

use crate::case::Case;

/// Payload shape of one variant.
enum Shape<'a> {
    Unit,
    Newtype,
    Named(Vec<&'a Ident>),
}

struct EventVariant<'a> {
    ident: &'a Ident,
    ty: String,
    shape: Shape<'a>,
}

/// Expand the derive.
pub(crate) fn derive(input: &DeriveInput) -> syn::Result<TokenStream> {
    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "TypedEvent can only be derived for enums",
        ));
    };
    crate::reject_generics(&input.generics, "TypedEvent")?;
    let rename_all = enum_case(input)?;
    let variants = (data.variants.iter())
        .map(|v| parse_variant(v, rename_all))
        .collect::<syn::Result<Vec<_>>>()?;
    for (i, v) in variants.iter().enumerate() {
        if variants[..i].iter().any(|prev| prev.ty == v.ty) {
            let msg = format!("event type `{}` is declared by more than one variant", v.ty);
            return Err(syn::Error::new_spanned(v.ident, msg));
        }
    }
    Ok(expand(input, &variants))
}

fn enum_case(input: &DeriveInput) -> syn::Result<Option<Case>> {
    let mut case = None;
    for attr in input.attrs.iter().filter(|a| a.path().is_ident("event")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename_all") {
                case = Some(Case::from_lit(&meta.value()?.parse()?)?);
                Ok(())
            } else {
                Err(meta.error("unknown enum attribute; expected `rename_all = \"...\"`"))
            }
        })?;
    }
    Ok(case)
}

fn parse_variant(variant: &Variant, rename_all: Option<Case>) -> syn::Result<EventVariant<'_>> {
    let mut rename = None;
    for attr in variant.attrs.iter().filter(|a| a.path().is_ident("event")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                let lit: LitStr = meta.value()?.parse()?;
                if lit.value().is_empty() {
                    return Err(syn::Error::new(lit.span(), "event type must not be empty"));
                }
                rename = Some(lit.value());
                Ok(())
            } else {
                Err(meta.error("unknown variant attribute; expected `rename = \"...\"`"))
            }
        })?;
    }
    let name = variant.ident.to_string();
    let ty = rename.unwrap_or_else(|| rename_all.map_or_else(|| name.clone(), |c| c.apply(&name)));
    let shape = match &variant.fields {
        Fields::Unit => Shape::Unit,
        Fields::Unnamed(f) if f.unnamed.is_empty() => Shape::Unit,
        Fields::Unnamed(f) if f.unnamed.len() == 1 => Shape::Newtype,
        Fields::Unnamed(f) => {
            let msg = "a tuple variant carries exactly one payload value; use named fields for \
                       an object payload";
            return Err(syn::Error::new_spanned(f, msg));
        }
        Fields::Named(f) => Shape::Named(f.named.iter().filter_map(|f| f.ident.as_ref()).collect()),
    };
    Ok(EventVariant { ident: &variant.ident, ty, shape })
}

fn expand(input: &DeriveInput, variants: &[EventVariant<'_>]) -> TokenStream {
    let (name, vis) = (&input.ident, &input.vis);
    let mut sorted: Vec<&str> = variants.iter().map(|v| v.ty.as_str()).collect();
    sorted.sort_unstable();
    let type_arms = variants.iter().map(|v| {
        let (ident, ty) = (v.ident, &v.ty);
        quote!(Self::#ident { .. } => #ty)
    });
    let conversions = conversions(name, variants);
    quote! {
        #[automatically_derived]
        impl #name {
            /// The XState event `type` of this event.
            #[must_use]
            #vis const fn event_type(&self) -> &'static str {
                match *self { #(#type_arms,)* }
            }

            /// Every event type this enum declares, sorted.
            #[must_use]
            #vis fn declared_events() -> &'static [&'static str] {
                &[#(#sorted),*]
            }

            /// Compare this enum with the events `machine` handles.
            ///
            /// # Errors
            /// Describes the events the enum declares that the chart does not handle and the
            /// events the chart handles that the enum does not declare.
            #vis fn check(
                machine: &::xstate_core::Machine,
            ) -> ::core::result::Result<(), ::std::string::String> {
                <Self as ::xstate_core::TypedEvent>::check(machine)
            }
        }

        #[automatically_derived]
        impl ::xstate_core::TypedEvent for #name {
            fn event_type(&self) -> &'static str {
                #name::event_type(self)
            }

            fn declared_events() -> &'static [&'static str] {
                #name::declared_events()
            }
        }

        #conversions
    }
}

/// `From<Enum> for Event` and `TryFrom<Event> for Enum`.
fn conversions(name: &Ident, variants: &[EventVariant<'_>]) -> TokenStream {
    let into_arms = variants.iter().map(|v| into_arm(name, v));
    let from_arms = variants.iter().map(from_arm);
    quote! {
        #[automatically_derived]
        impl ::core::convert::From<#name> for ::xstate_core::Event {
            fn from(event: #name) -> Self {
                match event { #(#into_arms,)* }
            }
        }

        #[automatically_derived]
        impl ::core::convert::TryFrom<::xstate_core::Event> for #name {
            type Error = ::xstate_core::Error;

            fn try_from(
                event: ::xstate_core::Event,
            ) -> ::core::result::Result<Self, ::xstate_core::Error> {
                match event.ty.as_str() {
                    #(#from_arms,)*
                    other => ::core::result::Result::Err(::xstate_core::Error::InvalidEvent(
                        ::std::format!(
                            "unknown event `{other}`; expected one of {:?}",
                            #name::declared_events(),
                        ),
                    )),
                }
            }
        }
    }
}

/// `Enum::Variant.. => Event` arm of `From<Enum> for Event`.
fn into_arm(name: &Ident, v: &EventVariant<'_>) -> TokenStream {
    let (ident, ty) = (v.ident, &v.ty);
    match &v.shape {
        Shape::Unit => quote!(#name::#ident { .. } => ::xstate_core::Event::new(#ty)),
        Shape::Newtype => quote! {
            #name::#ident(payload) => ::xstate_core::Event::with_payload(
                #ty, ::xstate_core::typed::encode(#ty, &payload),
            )
        },
        Shape::Named(fields) => {
            let keys = fields.iter().map(|f| crate::unraw(f));
            quote! {
                #name::#ident { #(#fields),* } => ::xstate_core::Event::with_payload(
                    #ty,
                    ::xstate_core::typed::object([
                        #((#keys, ::xstate_core::typed::encode(#ty, &#fields))),*
                    ]),
                )
            }
        }
    }
}

/// `"TYPE" => Ok(Variant..)` arm of `TryFrom<Event> for Enum`.
fn from_arm(v: &EventVariant<'_>) -> TokenStream {
    let (ident, ty) = (v.ident, &v.ty);
    let decode = quote!(::xstate_core::typed::decode);
    let value = match &v.shape {
        Shape::Unit => quote!(Self::#ident {}),
        Shape::Newtype => {
            quote!(Self::#ident(#decode(#ty, ::core::option::Option::None, &event.payload)?))
        }
        Shape::Named(fields) => {
            let keys = fields.iter().map(|f| crate::unraw(f));
            quote!(Self::#ident {
                #(#fields: #decode(#ty, ::core::option::Option::Some(#keys), &event.payload)?),*
            })
        }
    };
    quote!(#ty => ::core::result::Result::Ok(#value))
}
