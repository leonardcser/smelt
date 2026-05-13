//! Proc macros backing `smelt-core`'s `LuaOpts` and `LuaAlias`
//! traits — see `crates/core/src/lua/lua_type.rs` for the trait
//! definitions and the doc registry that `gen-lua-docs` reads from.
//!
//! Both derives emit:
//! - `LuaType` (so the type can show up directly in a `register_fn`
//!   sig as e.g. `opts: smelt.buf.ExtmarkOpts?` instead of `opts:
//!   table?`).
//! - `LuaTypeTuple` for single-arg use (so `|_, opts: ExtmarkOpts|`
//!   compiles without wrapping the closure's arg in a 1-tuple).
//! - `mlua::FromLua` so the closure receives a fully decoded value.
//!
//! The `LuaType` impl pushes the decl into the doc registry the first
//! time `lua_type()` runs — i.e. as soon as a `register_fn` site
//! references the type. No central "declare" step needed; declaration
//! follows usage.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{parse_macro_input, Attribute, Data, DeriveInput, Expr, Lit, Meta, Type};

#[proc_macro_derive(LuaOpts, attributes(lua))]
pub fn derive_lua_opts(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_lua_opts(&input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

#[proc_macro_derive(LuaAlias, attributes(lua))]
pub fn derive_lua_alias(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand_lua_alias(&input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  #[lua_module(name = "smelt.foo", doc = "...")]
//
//  Marks a Lua registration function and injects a `record_module_doc`
//  call at the top of its body. Both args are required — forgetting one
//  is a compile error, so every module ships with a doc string that the
//  gen-lua-docs pass can pick up.
// ═══════════════════════════════════════════════════════════════════════

#[proc_macro_attribute]
pub fn lua_module(args: TokenStream, input: TokenStream) -> TokenStream {
    match expand_lua_module(args, input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_lua_module(args: TokenStream, input: TokenStream) -> syn::Result<TokenStream2> {
    let meta_list = syn::parse::Parser::parse(
        syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
        args,
    )?;
    let mut name: Option<String> = None;
    let mut doc: Option<String> = None;
    for meta in meta_list {
        if let Meta::NameValue(nv) = meta {
            if let syn::Expr::Lit(syn::ExprLit {
                lit: Lit::Str(s), ..
            }) = nv.value
            {
                if nv.path.is_ident("name") {
                    name = Some(s.value());
                } else if nv.path.is_ident("doc") {
                    doc = Some(s.value());
                }
            }
        }
    }
    let name = name.ok_or_else(|| {
        syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[lua_module] requires `name = \"smelt.foo\"`",
        )
    })?;
    let doc = doc.ok_or_else(|| {
        syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[lua_module] requires `doc = \"...\"`",
        )
    })?;

    let mut func: syn::ItemFn = syn::parse(input)?;
    let prelude: syn::Stmt = syn::parse_quote! {
        ::smelt_core::lua::doc::record_module_doc(#name, #doc);
    };
    func.block.stmts.insert(0, prelude);
    Ok(quote! { #func })
}

// ── shared helpers ────────────────────────────────────────────────────

/// Read the `#[lua(name = "...")]` attribute. Returns `None` when
/// missing; defaults are applied by callers.
fn lua_name_attr(attrs: &[Attribute]) -> syn::Result<Option<String>> {
    lua_string_attr(attrs, "name")
}

/// Read the `#[lua(mirror = "path::to::Enum")]` attribute on a
/// `LuaAlias`-derived enum. When set, the macro emits a pair of
/// `From` impls between the Lua enum and the mirrored enum, expecting
/// matching variant identifiers.
fn lua_mirror_attr(attrs: &[Attribute]) -> syn::Result<Option<syn::Path>> {
    let Some(raw) = lua_string_attr(attrs, "mirror")? else {
        return Ok(None);
    };
    let parsed: syn::Path = syn::parse_str(&raw)?;
    Ok(Some(parsed))
}

fn lua_string_attr(attrs: &[Attribute], key: &str) -> syn::Result<Option<String>> {
    for attr in attrs {
        if !attr.path().is_ident("lua") {
            continue;
        }
        let nested = attr.parse_args_with(
            syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
        )?;
        for meta in nested {
            if let Meta::NameValue(nv) = meta {
                if nv.path.is_ident(key) {
                    if let syn::Expr::Lit(syn::ExprLit {
                        lit: Lit::Str(lit_str),
                        ..
                    }) = nv.value
                    {
                        return Ok(Some(lit_str.value()));
                    }
                }
            }
        }
    }
    Ok(None)
}

/// Read the `#[lua(rename = "...")]` attribute on enum variants.
fn lua_rename_attr(attrs: &[Attribute]) -> syn::Result<Option<String>> {
    for attr in attrs {
        if !attr.path().is_ident("lua") {
            continue;
        }
        let nested = attr.parse_args_with(
            syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated,
        )?;
        for meta in nested {
            if let Meta::NameValue(nv) = meta {
                if nv.path.is_ident("rename") {
                    if let syn::Expr::Lit(syn::ExprLit {
                        lit: Lit::Str(lit_str),
                        ..
                    }) = nv.value
                    {
                        return Ok(Some(lit_str.value()));
                    }
                }
            }
        }
    }
    Ok(None)
}

/// `#[lua(default)]` on a struct field: missing key in Lua falls back
/// to `Default::default()`. Implies optional in the Lua-side type.
fn has_lua_default(attrs: &[Attribute]) -> bool {
    has_lua_path_flag(attrs, "default")
}

/// `#[lua(rest)]` on a `LuaOpts` field: collect every key not consumed
/// by the struct's named fields into a `HashMap<String, V>` (the field's
/// declared type). Surfaces in the LuaCATS class as `[string]: V`.
fn has_lua_rest(attrs: &[Attribute]) -> bool {
    has_lua_path_flag(attrs, "rest")
}

fn has_lua_path_flag(attrs: &[Attribute], key: &str) -> bool {
    for attr in attrs {
        if !attr.path().is_ident("lua") {
            continue;
        }
        if let Ok(nested) = attr
            .parse_args_with(syn::punctuated::Punctuated::<Meta, syn::Token![,]>::parse_terminated)
        {
            for meta in nested {
                if matches!(&meta, Meta::Path(p) if p.is_ident(key)) {
                    return true;
                }
            }
        }
    }
    false
}

/// Concatenate every `#[doc = "..."]` line on an item into a single
/// string. Whitespace at the start of each line is trimmed and joined
/// with spaces — keeps `--- ` Lua-comment output flat.
fn doc_string(attrs: &[Attribute]) -> String {
    let mut out = String::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let Meta::NameValue(nv) = &attr.meta {
            if let Expr::Lit(syn::ExprLit {
                lit: Lit::Str(s), ..
            }) = &nv.value
            {
                let line = s.value();
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    if !out.is_empty() {
                        out.push(' ');
                    }
                    out.push_str(trimmed);
                }
            }
        }
    }
    out
}

/// Detect `Option<X>` and return the inner `X` if so. Used to peel
/// optionality out of a struct field's type.
fn unwrap_option(ty: &Type) -> Option<&Type> {
    if let Type::Path(tp) = ty {
        let last = tp.path.segments.last()?;
        if last.ident != "Option" {
            return None;
        }
        if let syn::PathArguments::AngleBracketed(args) = &last.arguments {
            if let Some(syn::GenericArgument::Type(inner)) = args.args.first() {
                return Some(inner);
            }
        }
    }
    None
}

// ── LuaOpts ──────────────────────────────────────────────────────────

fn expand_lua_opts(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let struct_ident = &input.ident;
    let lua_name = lua_name_attr(&input.attrs)?
        .ok_or_else(|| syn::Error::new_spanned(struct_ident, "#[lua(name = \"...\")] required"))?;
    let class_doc = doc_string(&input.attrs);

    let Data::Struct(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            struct_ident,
            "#[derive(LuaOpts)] only supports plain structs",
        ));
    };

    let fields = match &data.fields {
        syn::Fields::Named(named) => &named.named,
        _ => {
            return Err(syn::Error::new_spanned(
                struct_ident,
                "#[derive(LuaOpts)] requires named fields",
            ));
        }
    };

    let mut field_descs = Vec::new();
    let mut from_lua_field_reads = Vec::new();
    let mut named_keys: Vec<String> = Vec::new();
    let mut rest_field: Option<(syn::Ident, Type, String)> = None;
    for f in fields {
        let fname = f.ident.as_ref().unwrap();
        // `r#` raw identifiers print as `r#override`; strip the prefix
        // so the Lua-side key matches the user-facing form.
        let fname_raw = fname.to_string();
        let fname_str = lua_rename_attr(&f.attrs)?.unwrap_or_else(|| {
            fname_raw
                .strip_prefix("r#")
                .map(str::to_string)
                .unwrap_or(fname_raw)
        });
        let fdoc = doc_string(&f.attrs);

        if has_lua_rest(&f.attrs) {
            if rest_field.is_some() {
                return Err(syn::Error::new_spanned(
                    f,
                    "#[derive(LuaOpts)] only supports one #[lua(rest)] field",
                ));
            }
            let t = &f.ty;
            // Inner V for `HashMap<String, V>` — used as the index-signature
            // value type in the LuaCATS render.
            let value_ty_expr = quote! {
                <#t as ::smelt_core::lua::lua_type::LuaType>::lua_type()
            };
            // Render as `[string]` in the LuaClassField table — the
            // emitter treats this name as a LuaCATS index signature
            // (`---@field [string] V`).
            field_descs.push(quote! {
                ::smelt_core::lua::lua_type::LuaClassField {
                    name: "[string]",
                    ty: {
                        let __raw = #value_ty_expr;
                        // Strip the outer `table<string, …>` so the
                        // index-signature line reads `[string]: V`.
                        let trimmed = __raw
                            .strip_prefix("table<string, ")
                            .and_then(|s| s.strip_suffix('>'))
                            .map(::std::string::ToString::to_string)
                            .unwrap_or(__raw);
                        trimmed
                    },
                    optional: true,
                    doc: #fdoc,
                }
            });
            rest_field = Some((fname.clone(), f.ty.clone(), fname_str));
            continue;
        }

        named_keys.push(fname_str.clone());
        let lua_default = has_lua_default(&f.attrs);
        let inner_opt = unwrap_option(&f.ty);
        let optional = inner_opt.is_some() || lua_default;
        // Inner type for Lua sig — strip the Option wrapper if present.
        let lua_ty_expr = if let Some(inner) = inner_opt {
            quote! { <#inner as ::smelt_core::lua::lua_type::LuaType>::lua_type() }
        } else {
            let t = &f.ty;
            quote! { <#t as ::smelt_core::lua::lua_type::LuaType>::lua_type() }
        };
        field_descs.push(quote! {
            ::smelt_core::lua::lua_type::LuaClassField {
                name: #fname_str,
                ty: #lua_ty_expr,
                optional: #optional,
                doc: #fdoc,
            }
        });

        let read = if let Some(inner) = inner_opt {
            quote! {
                #fname: __t.get::<::std::option::Option<#inner>>(#fname_str)?
            }
        } else if lua_default {
            let t = &f.ty;
            quote! {
                #fname: __t
                    .get::<::std::option::Option<#t>>(#fname_str)?
                    .unwrap_or_default()
            }
        } else {
            let t = &f.ty;
            quote! { #fname: __t.get::<#t>(#fname_str)? }
        };
        from_lua_field_reads.push(read);
    }

    let rest_read = match rest_field {
        Some((fname, ty, _)) => {
            quote! {
                #fname: {
                    let mut __rest = <#ty as ::std::default::Default>::default();
                    let __known: &[&str] = &[ #( #named_keys ),* ];
                    for __pair in __t.clone().pairs::<::std::string::String, ::mlua::Value>() {
                        let (__k, __v) = __pair?;
                        if __known.contains(&__k.as_str()) {
                            continue;
                        }
                        let __decoded = ::mlua::FromLua::from_lua(__v, __lua)?;
                        __rest.insert(__k, __decoded);
                    }
                    __rest
                }
            }
        }
        None => quote! {},
    };
    let trailing_comma = if rest_read.is_empty() {
        quote! {}
    } else {
        quote! { , }
    };

    Ok(quote! {
        impl ::smelt_core::lua::lua_type::LuaType for #struct_ident {
            fn lua_type() -> ::std::string::String {
                ::smelt_core::lua::doc::record_class(
                    <Self as ::smelt_core::lua::lua_type::LuaOpts>::lua_class_decl()
                );
                ::std::string::String::from(#lua_name)
            }
        }

        impl ::smelt_core::lua::lua_type::LuaTypeTuple for #struct_ident {
            const ARITY: usize = 1;
            fn lua_param_list(param_names: &[&'static str]) -> ::std::string::String {
                let name = param_names.first().copied().unwrap_or("arg1");
                ::std::format!(
                    "{}: {}",
                    name,
                    <Self as ::smelt_core::lua::lua_type::LuaType>::lua_type()
                )
            }
        }

        impl ::smelt_core::lua::lua_type::LuaOpts for #struct_ident {
            fn lua_class_decl() -> ::smelt_core::lua::lua_type::LuaClassDecl {
                ::smelt_core::lua::lua_type::LuaClassDecl {
                    name: #lua_name,
                    doc: #class_doc,
                    fields: ::std::vec![ #( #field_descs ),* ],
                }
            }
        }

        impl ::mlua::FromLua for #struct_ident {
            fn from_lua(__value: ::mlua::Value, __lua: &::mlua::Lua) -> ::mlua::Result<Self> {
                let __t: ::mlua::Table = ::mlua::FromLua::from_lua(__value, __lua)?;
                ::std::result::Result::Ok(Self {
                    #( #from_lua_field_reads ),*
                    #trailing_comma
                    #rest_read
                })
            }
        }
    })
}

// ── LuaAlias ─────────────────────────────────────────────────────────

fn expand_lua_alias(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let enum_ident = &input.ident;
    let lua_name = lua_name_attr(&input.attrs)?
        .ok_or_else(|| syn::Error::new_spanned(enum_ident, "#[lua(name = \"...\")] required"))?;
    let alias_doc = doc_string(&input.attrs);
    let mirror_path = lua_mirror_attr(&input.attrs)?;

    let Data::Enum(data) = &input.data else {
        return Err(syn::Error::new_spanned(
            enum_ident,
            "#[derive(LuaAlias)] only supports enums",
        ));
    };

    let mut variants = Vec::new();
    let mut from_arms = Vec::new();
    let mut into_arms = Vec::new();
    let mut mirror_idents = Vec::new();
    for v in &data.variants {
        if !matches!(v.fields, syn::Fields::Unit) {
            return Err(syn::Error::new_spanned(
                v,
                "#[derive(LuaAlias)] requires unit-only variants",
            ));
        }
        let vident = &v.ident;
        let lua_string = lua_rename_attr(&v.attrs)?.unwrap_or_else(|| {
            // Default rename: snake_case the variant name.
            let mut out = String::new();
            for (i, ch) in vident.to_string().chars().enumerate() {
                if ch.is_ascii_uppercase() {
                    if i != 0 {
                        out.push('_');
                    }
                    out.push(ch.to_ascii_lowercase());
                } else {
                    out.push(ch);
                }
            }
            out
        });
        variants.push(quote! { #lua_string });
        from_arms.push(quote! { #lua_string => ::std::result::Result::Ok(Self::#vident) });
        into_arms.push(quote! { Self::#vident => #lua_string });
        mirror_idents.push(vident.clone());
    }

    let mirror_impls = mirror_path.map(|mirror| {
        let idents = &mirror_idents;
        quote! {
            impl ::std::convert::From<#mirror> for #enum_ident {
                fn from(v: #mirror) -> Self {
                    match v {
                        #( #mirror::#idents => Self::#idents, )*
                    }
                }
            }

            impl ::std::convert::From<#enum_ident> for #mirror {
                fn from(v: #enum_ident) -> Self {
                    match v {
                        #( #enum_ident::#idents => #mirror::#idents, )*
                    }
                }
            }
        }
    });

    Ok(quote! {
        impl ::smelt_core::lua::lua_type::LuaType for #enum_ident {
            fn lua_type() -> ::std::string::String {
                ::smelt_core::lua::doc::record_alias(
                    <Self as ::smelt_core::lua::lua_type::LuaAlias>::lua_alias_decl()
                );
                ::std::string::String::from(#lua_name)
            }
        }

        impl ::smelt_core::lua::lua_type::LuaTypeTuple for #enum_ident {
            const ARITY: usize = 1;
            fn lua_param_list(param_names: &[&'static str]) -> ::std::string::String {
                let name = param_names.first().copied().unwrap_or("arg1");
                ::std::format!(
                    "{}: {}",
                    name,
                    <Self as ::smelt_core::lua::lua_type::LuaType>::lua_type()
                )
            }
        }

        impl ::smelt_core::lua::lua_type::LuaAlias for #enum_ident {
            fn lua_alias_decl() -> ::smelt_core::lua::lua_type::LuaAliasDecl {
                ::smelt_core::lua::lua_type::LuaAliasDecl {
                    name: #lua_name,
                    doc: #alias_doc,
                    variants: ::std::vec![ #( #variants ),* ],
                    open: false,
                }
            }
        }

        impl ::mlua::FromLua for #enum_ident {
            fn from_lua(__value: ::mlua::Value, __lua: &::mlua::Lua) -> ::mlua::Result<Self> {
                let __s: ::std::string::String = ::mlua::FromLua::from_lua(__value, __lua)?;
                match __s.as_str() {
                    #( #from_arms , )*
                    other => ::std::result::Result::Err(::mlua::Error::FromLuaConversionError {
                        from: "string",
                        to: ::std::string::String::from(stringify!(#enum_ident)),
                        message: ::std::option::Option::Some(::std::format!(
                            "unknown variant `{}` for {}", other, #lua_name
                        )),
                    }),
                }
            }
        }

        impl ::mlua::IntoLua for #enum_ident {
            fn into_lua(self, __lua: &::mlua::Lua) -> ::mlua::Result<::mlua::Value> {
                let __s: &'static str = match self {
                    #( #into_arms , )*
                };
                ::mlua::IntoLua::into_lua(__s, __lua)
            }
        }

        #mirror_impls
    })
}
