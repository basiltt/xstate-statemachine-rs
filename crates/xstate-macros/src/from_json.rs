//! Implementation of the `xstate_from_json!` and `xstate_inline_json!` macros.

use std::path::PathBuf;

use proc_macro2::TokenStream;
use quote::quote;
use syn::{LitStr, parse2};
use xstate_core::Machine;
use xstate_core::codegen::{CodegenOptions, generate_rust_code};

use crate::krate;

/// Expands `xstate_from_json!(...)`.
pub(crate) fn expand_from_json(input: TokenStream) -> syn::Result<TokenStream> {
    let lit: LitStr = parse2(input)?;
    let val = lit.value();
    let trimmed = val.trim();

    let resolved_krate = krate::resolve(None);
    let options = CodegenOptions { crate_path: resolved_krate.to_string(), ..Default::default() };

    if trimmed.starts_with('{') {
        // Inline JSON
        expand_json_string(&val, &options, None, lit.span())
    } else {
        // File path relative to CARGO_MANIFEST_DIR
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").map_err(|e| {
            syn::Error::new(
                lit.span(),
                format!("failed to read CARGO_MANIFEST_DIR environment variable: {e}"),
            )
        })?;

        let manifest_path = PathBuf::from(&manifest_dir).join(&val);
        let full_path = if manifest_path.exists() {
            manifest_path
        } else if PathBuf::from(&manifest_dir).join("tests").join(&val).exists() {
            PathBuf::from(&manifest_dir).join("tests").join(&val)
        } else if PathBuf::from(&manifest_dir).join("src").join(&val).exists() {
            PathBuf::from(&manifest_dir).join("src").join(&val)
        } else {
            manifest_path
        };

        let content = std::fs::read_to_string(&full_path).map_err(|e| {
            syn::Error::new(
                lit.span(),
                format!("failed to read statechart JSON file `{}`: {e}", full_path.display()),
            )
        })?;

        expand_json_string(&content, &options, Some(&val), lit.span())
    }
}

/// Expands `xstate_inline_json!(...)`.
pub(crate) fn expand_inline_json(input: TokenStream) -> syn::Result<TokenStream> {
    let lit: LitStr = parse2(input)?;
    let resolved_krate = krate::resolve(None);
    let options = CodegenOptions { crate_path: resolved_krate.to_string(), ..Default::default() };

    expand_json_string(&lit.value(), &options, None, lit.span())
}

fn expand_json_string(
    json_text: &str,
    options: &CodegenOptions,
    tracked_file: Option<&str>,
    span: proc_macro2::Span,
) -> syn::Result<TokenStream> {
    let machine = Machine::from_json(json_text)
        .map_err(|e| syn::Error::new(span, format!("invalid XState JSON document: {e}")))?;

    let rust_code = generate_rust_code(&machine, options);
    let parsed_file: syn::File = syn::parse_str(&rust_code).map_err(|e| {
        syn::Error::new(span, format!("failed to parse generated Rust code (internal error): {e}"))
    })?;

    let items = parsed_file.items;

    let tracking = if let Some(path_str) = tracked_file {
        quote! {
            // Include file to trigger Cargo rebuild on modification
            const _: &str = include_str!(#path_str);
        }
    } else {
        quote! {}
    };

    Ok(quote! {
        #tracking
        #(#items)*
    })
}
