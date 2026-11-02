//! The `statechart!` DSL: [`ast`] types, a parser per construct ([`parse`]), compile-time
//! structural checks ([`validate`]) and the JSON expansion ([`expand`]).

mod ast;
mod expand;
mod parse;
mod validate;

use proc_macro2::TokenStream;

/// Parse, check and expand one `statechart!` invocation.
pub(crate) fn statechart(input: TokenStream) -> syn::Result<TokenStream> {
    let chart: ast::Chart = syn::parse2(input)?;
    validate::validate(&chart)?;
    Ok(expand::expand(&chart))
}
