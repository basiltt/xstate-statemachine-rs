//! Resolution of the path generated code uses to reach `xstate-core`.
//!
//! A user may depend on `xstate-core` directly or only on the `xstate-statemachine` facade
//! (which re-exports it as `xstate_statemachine::core`), possibly under another name. The
//! derives accept `#[xstate(crate = "path")]` and `statechart!` a leading `crate: path,` item
//! to override the detection.

use proc_macro_crate::{FoundCrate, crate_name};
use proc_macro2::{Span, TokenStream};
use quote::quote;
use syn::{Ident, Path};

/// Where to find `xstate_core` items: the override when given, else the dependency the
/// calling crate declares (`xstate-core` first, then the `xstate-statemachine` facade), else
/// `::xstate_core`.
pub(crate) fn resolve(explicit: Option<&Path>) -> TokenStream {
    if let Some(path) = explicit {
        return quote!(#path);
    }
    if let Some(root) = dependency("xstate-core") {
        return root;
    }
    if let Some(root) = dependency("xstate-statemachine") {
        return quote!(#root::core);
    }
    quote!(::xstate_core)
}

/// `::name` for a direct dependency (honouring `package` renames), `None` when absent.
fn dependency(package: &str) -> Option<TokenStream> {
    match crate_name(package).ok()? {
        // The crate being compiled is `package` itself (its tests and examples see it as an
        // external crate under its own name).
        FoundCrate::Itself => {
            let ident = Ident::new(&package.replace('-', "_"), Span::call_site());
            Some(quote!(::#ident))
        }
        FoundCrate::Name(name) => {
            let ident = Ident::new(&name, Span::call_site());
            Some(quote!(::#ident))
        }
    }
}

/// The `#[xstate(crate = "path")]` override on a derive input.
pub(crate) fn from_attrs(attrs: &[syn::Attribute]) -> syn::Result<Option<Path>> {
    let mut path = None;
    for attr in attrs.iter().filter(|a| a.path().is_ident("xstate")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("crate") {
                let lit: syn::LitStr = meta.value()?.parse()?;
                path = Some(lit.parse::<Path>()?);
                Ok(())
            } else {
                Err(meta.error("unknown attribute; expected `crate = \"path\"`"))
            }
        })?;
    }
    Ok(path)
}
