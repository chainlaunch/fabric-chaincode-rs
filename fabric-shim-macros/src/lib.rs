//! Proc macros behind `fabric_shim::contract` — see that module for docs.

#![forbid(unsafe_code)]

use heck::{ToLowerCamelCase, ToUpperCamelCase};
use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::punctuated::Punctuated;
use syn::{
    parse_macro_input, Data, DeriveInput, Fields, FnArg, ImplItem, ItemImpl, LitStr, Meta,
    ReturnType, Token, Type,
};

// ---------------------------------------------------------------------------
// #[contract]
// ---------------------------------------------------------------------------

struct ContractArgs {
    name: Option<String>,
    version: Option<String>,
    title: Option<String>,
}

fn parse_contract_args(attr: TokenStream) -> syn::Result<ContractArgs> {
    let mut out = ContractArgs {
        name: None,
        version: None,
        title: None,
    };
    if attr.is_empty() {
        return Ok(out);
    }
    let metas = syn::parse::Parser::parse(Punctuated::<Meta, Token![,]>::parse_terminated, attr)?;
    for meta in metas {
        let syn::Meta::NameValue(nv) = &meta else {
            return Err(syn::Error::new_spanned(
                meta,
                "expected key = \"value\" (name, version, title)",
            ));
        };
        let syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(lit),
            ..
        }) = &nv.value
        else {
            return Err(syn::Error::new_spanned(
                &nv.value,
                "expected string literal",
            ));
        };
        let key = nv
            .path
            .get_ident()
            .map(|i| i.to_string())
            .unwrap_or_default();
        match key.as_str() {
            "name" => out.name = Some(lit.value()),
            "version" => out.version = Some(lit.value()),
            "title" => out.title = Some(lit.value()),
            other => {
                return Err(syn::Error::new_spanned(
                    &nv.path,
                    format!("unknown contract option `{other}` (expected name, version, title)"),
                ))
            }
        }
    }
    Ok(out)
}

struct TxMethod {
    wire_name: String,
    tag: &'static str, // "submit" | "evaluate"
    method_ident: syn::Ident,
    is_async: bool,
    params: Vec<(syn::Ident, Type)>, // excludes &self and ctx
    ok_ty: Option<Type>,             // None for Result<(), _>
}

/// Turns an impl block into a Fabric contract: routes transactions by name,
/// parses typed arguments, and generates `Chaincode::{invoke, metadata}`.
#[proc_macro_attribute]
pub fn contract(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = match parse_contract_args(attr) {
        Ok(a) => a,
        Err(e) => return e.to_compile_error().into(),
    };
    let mut input = parse_macro_input!(item as ItemImpl);

    if !input.generics.params.is_empty() {
        return syn::Error::new_spanned(&input.generics, "#[contract] impls cannot be generic")
            .to_compile_error()
            .into();
    }
    let self_ty = &input.self_ty;
    let type_name = match self_ty.as_ref() {
        Type::Path(p) => p.path.segments.last().unwrap().ident.to_string(),
        _ => {
            return syn::Error::new_spanned(self_ty, "#[contract] requires a named type")
                .to_compile_error()
                .into()
        }
    };

    let mut txs: Vec<TxMethod> = Vec::new();
    let mut errors: Vec<syn::Error> = Vec::new();

    for item in &mut input.items {
        let ImplItem::Fn(method) = item else { continue };
        let Some(pos) = method
            .attrs
            .iter()
            .position(|a| a.path().is_ident("transaction"))
        else {
            continue;
        };
        let tx_attr = method.attrs.remove(pos);

        // #[transaction], #[transaction(evaluate)], #[transaction(submit, name = "X")]
        let mut tag = "submit";
        let mut wire_name: Option<String> = None;
        if let Meta::List(_) = &tx_attr.meta {
            let nested =
                match tx_attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated) {
                    Ok(n) => n,
                    Err(e) => {
                        errors.push(e);
                        continue;
                    }
                };
            for meta in nested {
                match &meta {
                    Meta::Path(p) if p.is_ident("evaluate") => tag = "evaluate",
                    Meta::Path(p) if p.is_ident("submit") => tag = "submit",
                    Meta::NameValue(nv) if nv.path.is_ident("name") => {
                        if let syn::Expr::Lit(syn::ExprLit {
                            lit: syn::Lit::Str(lit),
                            ..
                        }) = &nv.value
                        {
                            wire_name = Some(lit.value());
                        } else {
                            errors.push(syn::Error::new_spanned(
                                &nv.value,
                                "expected string literal",
                            ));
                        }
                    }
                    other => errors.push(syn::Error::new_spanned(
                        other,
                        "expected `submit`, `evaluate`, or `name = \"...\"`",
                    )),
                }
            }
        }

        let mut inputs = method.sig.inputs.iter();
        match inputs.next() {
            Some(FnArg::Receiver(_)) => {}
            _ => {
                errors.push(syn::Error::new_spanned(
                    &method.sig,
                    "#[transaction] methods must take &self",
                ));
                continue;
            }
        }
        if inputs.next().is_none() {
            errors.push(syn::Error::new_spanned(
                &method.sig,
                "#[transaction] methods must take a context as their first argument, e.g. `ctx: &ChaincodeStub`",
            ));
            continue;
        }
        let mut params = Vec::new();
        let mut bad = false;
        for arg in inputs {
            let FnArg::Typed(pt) = arg else { continue };
            match pt.pat.as_ref() {
                syn::Pat::Ident(pi) => params.push((pi.ident.clone(), (*pt.ty).clone())),
                other => {
                    errors.push(syn::Error::new_spanned(
                        other,
                        "#[transaction] parameters must be simple identifiers",
                    ));
                    bad = true;
                }
            }
        }
        if bad {
            continue;
        }

        // Return type must be Result<T, E>; T = () suppresses `returns` metadata.
        let ok_ty = match &method.sig.output {
            ReturnType::Type(_, ty) => match extract_result_ok(ty) {
                Some(t) => t,
                None => {
                    errors.push(syn::Error::new_spanned(
                        ty,
                        "#[transaction] methods must return Result<T, E> (E: Display)",
                    ));
                    continue;
                }
            },
            ReturnType::Default => {
                errors.push(syn::Error::new_spanned(
                    &method.sig,
                    "#[transaction] methods must return Result<T, E> (E: Display)",
                ));
                continue;
            }
        };
        let ok_ty = match &ok_ty {
            Type::Tuple(t) if t.elems.is_empty() => None,
            other => Some(other.clone()),
        };

        txs.push(TxMethod {
            wire_name: wire_name
                .unwrap_or_else(|| method.sig.ident.to_string().to_upper_camel_case()),
            tag,
            method_ident: method.sig.ident.clone(),
            is_async: method.sig.asyncness.is_some(),
            params,
            ok_ty,
        });
    }

    if let Some(err) = errors.into_iter().reduce(|mut a, b| {
        a.combine(b);
        a
    }) {
        return err.to_compile_error().into();
    }

    let contract_name = args.name.unwrap_or_else(|| type_name.clone());
    let title = args.title.unwrap_or_else(|| contract_name.clone());
    let version_tokens: TokenStream2 = match args.version {
        Some(v) => quote! { #v },
        None => quote! { env!("CARGO_PKG_VERSION") },
    };

    // ---- invoke() match arms ----
    let arms = txs.iter().map(|tx| {
        let wire = &tx.wire_name;
        let n = tx.params.len();
        let idents: Vec<_> = tx.params.iter().map(|(i, _)| i).collect();
        let param_lets = tx.params.iter().enumerate().map(|(i, (ident, ty))| {
            let idx = i + 1;
            let pname = ident.to_string().to_lower_camel_case();
            quote! {
                let #ident = match <#ty as fabric_shim::contract::ContractArg>::from_arg(&__args[#idx]) {
                    Ok(v) => v,
                    Err(e) => return fabric_shim::Response::error(
                        format!("invalid argument `{}`: {}", #pname, e),
                    ),
                };
            }
        });
        let method_ident = &tx.method_ident;
        let maybe_await = tx.is_async.then(|| quote! { .await });
        quote! {
            #wire => {
                if __args.len() != #n + 1 {
                    return fabric_shim::Response::error(format!(
                        "{} expects {} argument(s), got {}", #wire, #n, __args.len() - 1
                    ));
                }
                #(#param_lets)*
                match self.#method_ident(&__stub #(, #idents)*) #maybe_await {
                    Ok(v) => match fabric_shim::contract::ContractReturn::into_payload(v) {
                        Ok(payload) => fabric_shim::Response::success(payload),
                        Err(e) => fabric_shim::Response::error(e),
                    },
                    Err(e) => fabric_shim::Response::error(e.to_string()),
                }
            }
        }
    });

    // ---- metadata() ----
    let component_regs = txs.iter().flat_map(|tx| {
        let params = tx.params.iter().map(|(_, ty)| {
            quote! { <#ty as fabric_shim::contract::ContractSchema>::components(&mut __components); }
        });
        let ret = tx.ok_ty.iter().map(|ty| {
            quote! { <#ty as fabric_shim::contract::ContractSchema>::components(&mut __components); }
        });
        params.chain(ret).collect::<Vec<_>>()
    });
    let tx_builders = txs.iter().map(|tx| {
        let wire = &tx.wire_name;
        let ctor = if tx.tag == "evaluate" {
            quote! { fabric_shim::metadata::Transaction::evaluate(#wire) }
        } else {
            quote! { fabric_shim::metadata::Transaction::submit(#wire) }
        };
        let param_calls = tx.params.iter().map(|(ident, ty)| {
            let pname = ident.to_string().to_lower_camel_case();
            quote! { .parameter(#pname, <#ty as fabric_shim::contract::ContractSchema>::schema()) }
        });
        let returns_call = tx.ok_ty.iter().map(|ty| {
            quote! { .returns(<#ty as fabric_shim::contract::ContractSchema>::schema()) }
        });
        quote! { __contract = __contract.transaction(#ctor #(#param_calls)* #(#returns_call)*); }
    });

    let expanded = quote! {
        #input

        #[fabric_shim::async_trait]
        impl fabric_shim::Chaincode for #self_ty {
            fn metadata(&self) -> Option<fabric_shim::metadata::Metadata> {
                let mut __components: ::std::collections::BTreeMap<
                    ::std::string::String,
                    fabric_shim::serde_json::Value,
                > = ::std::collections::BTreeMap::new();
                #(#component_regs)*
                let mut __contract = fabric_shim::metadata::Contract::new(#contract_name);
                #(#tx_builders)*
                let mut __md = fabric_shim::metadata::Metadata::new(#title, #version_tokens);
                for (name, schema) in __components {
                    __md = __md.component(name, schema);
                }
                Some(__md.contract(__contract))
            }

            async fn invoke(&self, __stub: fabric_shim::ChaincodeStub) -> fabric_shim::Response {
                let __args = __stub.get_args();
                if __args.is_empty() {
                    return fabric_shim::Response::error("no function specified");
                }
                // `from_utf8_lossy` only allocates if the bytes aren't valid
                // UTF-8 (the common case borrows, no copy). Matching directly
                // on the resulting `&str` avoids two further String allocs
                // that a `.into_owned()` + `.to_string()` round trip would add.
                let __full = ::std::string::String::from_utf8_lossy(&__args[0]);
                // Accept namespaced calls ("Contract:Function") like the
                // Go/Node contract APIs.
                let __function = __full.rsplit(':').next().unwrap_or(&__full);
                match __function {
                    #(#arms)*
                    other => fabric_shim::Response::error(format!("unknown function {other}")),
                }
            }
        }
    };
    expanded.into()
}

/// Extract `T` from a syntactic `Result<T, E>` (with or without path prefix).
fn extract_result_ok(ty: &Type) -> Option<Type> {
    let Type::Path(p) = ty else { return None };
    let seg = p.path.segments.last()?;
    if seg.ident != "Result" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(ab) = &seg.arguments else {
        return None;
    };
    ab.args.iter().find_map(|a| match a {
        syn::GenericArgument::Type(t) => Some(t.clone()),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// #[derive(DataType)]
// ---------------------------------------------------------------------------

/// Derives `ContractSchema`/`ContractArg`/`ContractReturn` for a struct so it
/// can be used as a typed transaction parameter or return value. Field names
/// in the generated JSON schema honor `#[serde(rename)]`/`#[serde(rename_all)]`,
/// so the schema always matches the serialized JSON.
#[proc_macro_derive(DataType)]
pub fn derive_data_type(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let ident = &input.ident;
    let name = ident.to_string();

    let Data::Struct(data) = &input.data else {
        return syn::Error::new_spanned(&input.ident, "DataType supports only structs")
            .to_compile_error()
            .into();
    };
    let Fields::Named(fields) = &data.fields else {
        return syn::Error::new_spanned(&input.ident, "DataType requires named fields")
            .to_compile_error()
            .into();
    };
    if !input.generics.params.is_empty() {
        return syn::Error::new_spanned(&input.generics, "DataType structs cannot be generic")
            .to_compile_error()
            .into();
    }

    let rename_all = serde_rename_all(&input.attrs);

    let mut prop_inserts = Vec::new();
    let mut required = Vec::new();
    let mut nested_components = Vec::new();
    for field in &fields.named {
        let fident = field.ident.as_ref().unwrap();
        let json_name = serde_rename(&field.attrs)
            .unwrap_or_else(|| apply_rename_all(&fident.to_string(), rename_all.as_deref()));
        let (ty, optional) = unwrap_option(&field.ty);
        prop_inserts.push(quote! {
            __props.insert(
                #json_name.to_string(),
                <#ty as fabric_shim::contract::ContractSchema>::schema(),
            );
        });
        if !optional {
            required.push(json_name.clone());
        }
        nested_components.push(quote! {
            <#ty as fabric_shim::contract::ContractSchema>::components(out);
        });
    }

    let ref_path = format!("#/components/schemas/{name}");
    let expanded = quote! {
        impl fabric_shim::contract::ContractSchema for #ident {
            fn schema() -> fabric_shim::serde_json::Value {
                fabric_shim::serde_json::json!({ "$ref": #ref_path })
            }

            fn components(
                out: &mut ::std::collections::BTreeMap<
                    ::std::string::String,
                    fabric_shim::serde_json::Value,
                >,
            ) {
                if out.contains_key(#name) {
                    return;
                }
                let mut __props = fabric_shim::serde_json::Map::new();
                #(#prop_inserts)*
                out.insert(
                    #name.to_string(),
                    fabric_shim::serde_json::json!({
                        "type": "object",
                        "required": [#(#required),*],
                        "properties": __props,
                    }),
                );
                #(#nested_components)*
            }
        }

        impl fabric_shim::contract::ContractArg for #ident {
            fn from_arg(raw: &[u8]) -> ::std::result::Result<Self, ::std::string::String> {
                fabric_shim::serde_json::from_slice(raw)
                    .map_err(|e| format!("invalid JSON for {}: {}", #name, e))
            }
        }

        impl fabric_shim::contract::ContractReturn for #ident {
            fn into_payload(self) -> ::std::result::Result<::std::vec::Vec<u8>, ::std::string::String> {
                fabric_shim::serde_json::to_vec(&self)
                    .map_err(|e| format!("failed to serialize {}: {}", #name, e))
            }
        }
    };
    expanded.into()
}

fn unwrap_option(ty: &Type) -> (Type, bool) {
    if let Type::Path(p) = ty {
        if let Some(seg) = p.path.segments.last() {
            if seg.ident == "Option" {
                if let syn::PathArguments::AngleBracketed(ab) = &seg.arguments {
                    if let Some(syn::GenericArgument::Type(inner)) = ab.args.first() {
                        return (inner.clone(), true);
                    }
                }
            }
        }
    }
    (ty.clone(), false)
}

fn serde_rename(attrs: &[syn::Attribute]) -> Option<String> {
    serde_string_value(attrs, "rename")
}

fn serde_rename_all(attrs: &[syn::Attribute]) -> Option<String> {
    serde_string_value(attrs, "rename_all")
}

fn serde_string_value(attrs: &[syn::Attribute], key: &str) -> Option<String> {
    for attr in attrs {
        if !attr.path().is_ident("serde") {
            continue;
        }
        let mut found = None;
        // Ignore unknown serde attributes; we only care about `key = "..."`.
        let _ = attr.parse_nested_meta(|meta| {
            if meta.path.is_ident(key) {
                let lit: LitStr = meta.value()?.parse()?;
                found = Some(lit.value());
            } else if meta.input.peek(Token![=]) {
                let _: syn::Expr = meta.value()?.parse()?;
            }
            Ok(())
        });
        if found.is_some() {
            return found;
        }
    }
    None
}

fn apply_rename_all(field: &str, rule: Option<&str>) -> String {
    match rule {
        Some("camelCase") => field.to_lower_camel_case(),
        Some("PascalCase") => field.to_upper_camel_case(),
        Some("UPPERCASE") => field.to_uppercase(),
        Some("lowercase") => field.to_lowercase(),
        Some("SCREAMING_SNAKE_CASE") => heck::ToShoutySnakeCase::to_shouty_snake_case(field),
        Some("kebab-case") => heck::ToKebabCase::to_kebab_case(field),
        _ => field.to_string(),
    }
}
