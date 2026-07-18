//! Static `T` → LuaCATS type-string mapping.
//!
//! The impl table is vendored from
//! [tealr](https://github.com/lenscas/tealr/blob/master/src/type_representation.rs)
//! (MIT/Apache-2.0). We only need the type-name plumbing - tealr's
//! richer `Type` enum, the userdata-flavoured `TealData` trait, and
//! the `tealr_doc_gen` rendering pipeline don't fit smelt's
//! table-of-functions API shape, so we pulled the impl table inline
//! and emit LuaCATS strings directly.
//!
//! Vocabulary mirrors `mlua::Value::type_name()` plus the LuaCATS
//! suffixes `T?` (Option) and `T[]` (Vec). Composite container types
//! (`HashMap<K, V>`, etc.) round-trip through the trait recursively.

/// A Rust type that has a single LuaCATS type representation.
///
/// Containers are recursive: `Option<Vec<u64>>` resolves to
/// `"integer[]?"`. Tuple types are intentionally *not* `LuaType` -
/// they're handled by [`LuaTypeTuple`] for parameter lists and by
/// dedicated multi-return handling for `mlua::IntoLuaMulti` returns.
pub trait LuaType {
    fn lua_type() -> String;
}

macro_rules! impl_lua_type {
    ($name:literal, $($t:ty),+ $(,)?) => {
        $(
            impl LuaType for $t {
                #[inline]
                fn lua_type() -> String { String::from($name) }
            }
        )+
    };
}

impl_lua_type!("nil", ());
impl_lua_type!("boolean", bool);
impl_lua_type!("integer", i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize,);
impl_lua_type!("number", f32, f64);
impl_lua_type!("string", String, &'static str, mlua::LuaString);
impl_lua_type!("table", mlua::Table);
impl_lua_type!("function", mlua::Function);
impl_lua_type!("thread", mlua::Thread);
impl_lua_type!("any", mlua::Value);

impl<T: LuaType> LuaType for Option<T> {
    #[inline]
    fn lua_type() -> String {
        format!("{}?", T::lua_type())
    }
}

impl<T: LuaType> LuaType for Vec<T> {
    #[inline]
    fn lua_type() -> String {
        format!("{}[]", T::lua_type())
    }
}

impl<T: LuaType, const N: usize> LuaType for [T; N] {
    #[inline]
    fn lua_type() -> String {
        format!("{}[]", T::lua_type())
    }
}

impl<K: LuaType, V: LuaType> LuaType for std::collections::HashMap<K, V> {
    #[inline]
    fn lua_type() -> String {
        format!("table<{}, {}>", K::lua_type(), V::lua_type())
    }
}

impl<K: LuaType, V: LuaType> LuaType for std::collections::BTreeMap<K, V> {
    #[inline]
    fn lua_type() -> String {
        format!("table<{}, {}>", K::lua_type(), V::lua_type())
    }
}

/// A tuple of types matching `mlua::FromLuaMulti`'s shape, rendered
/// as a comma-separated parameter list.
///
/// `param_names` is paired with the tuple positionally; entries
/// shorter than the tuple arity fall back to `argN`. The bound on
/// each element is just [`LuaType`] - keeping the surface small so
/// adding new param types means one `impl_lua_type!` line, no new
/// tuple impls.
pub trait LuaTypeTuple {
    /// Number of Lua parameters this tuple represents.
    const ARITY: usize;
    fn lua_param_list(param_names: &[&'static str]) -> String;
}

impl LuaTypeTuple for () {
    const ARITY: usize = 0;
    fn lua_param_list(_: &[&'static str]) -> String {
        String::new()
    }
}

// Single-type param lists (1-tuple arity without the wrapper).
// Every type that has `LuaType` also has a `LuaTypeTuple` impl as a lone
// parameter, so `LuaMod::fn_` works for both `|_, x: String|` and
// `|_, (x, y): (String, u64)|` shapes.
macro_rules! impl_lua_type_tuple_single {
    ($($t:ty),+ $(,)?) => {
        $(impl LuaTypeTuple for $t {
            const ARITY: usize = 1;
            fn lua_param_list(param_names: &[&'static str]) -> String {
                let name = param_names.first().copied().unwrap_or("arg1");
                format!("{}: {}", name, <$t as LuaType>::lua_type())
            }
        })+
    };
}

impl_lua_type_tuple_single!(
    bool,
    i8,
    i16,
    i32,
    i64,
    i128,
    isize,
    u8,
    u16,
    u32,
    u64,
    u128,
    usize,
    f32,
    f64,
    String,
    &'static str,
    mlua::LuaString,
    mlua::Table,
    mlua::Function,
    mlua::Thread,
    mlua::Value,
);

impl<T: LuaType> LuaTypeTuple for Option<T> {
    const ARITY: usize = 1;
    fn lua_param_list(param_names: &[&'static str]) -> String {
        let name = param_names.first().copied().unwrap_or("arg1");
        format!("{}: {}", name, <Option<T> as LuaType>::lua_type())
    }
}

impl<T: LuaType> LuaTypeTuple for Vec<T> {
    const ARITY: usize = 1;
    fn lua_param_list(param_names: &[&'static str]) -> String {
        let name = param_names.first().copied().unwrap_or("arg1");
        format!("{}: {}", name, <Vec<T> as LuaType>::lua_type())
    }
}

impl<T: LuaType, const N: usize> LuaTypeTuple for [T; N] {
    const ARITY: usize = 1;
    fn lua_param_list(param_names: &[&'static str]) -> String {
        let name = param_names.first().copied().unwrap_or("arg1");
        format!("{}: {}", name, <[T; N] as LuaType>::lua_type())
    }
}

impl<K: LuaType, V: LuaType> LuaTypeTuple for std::collections::HashMap<K, V> {
    const ARITY: usize = 1;
    fn lua_param_list(param_names: &[&'static str]) -> String {
        let name = param_names.first().copied().unwrap_or("arg1");
        format!(
            "{}: {}",
            name,
            <std::collections::HashMap<K, V> as LuaType>::lua_type()
        )
    }
}

impl<K: LuaType, V: LuaType> LuaTypeTuple for std::collections::BTreeMap<K, V> {
    const ARITY: usize = 1;
    fn lua_param_list(param_names: &[&'static str]) -> String {
        let name = param_names.first().copied().unwrap_or("arg1");
        format!(
            "{}: {}",
            name,
            <std::collections::BTreeMap<K, V> as LuaType>::lua_type()
        )
    }
}

macro_rules! impl_lua_type_tuple {
    ($arity:literal, $($idx:tt: $T:ident),+ $(,)?) => {
        impl<$($T: LuaType),+> LuaTypeTuple for ($($T,)+) {
            const ARITY: usize = $arity;
            fn lua_param_list(param_names: &[&'static str]) -> String {
                let parts: Vec<String> = vec![
                    $({
                        let name = param_names
                            .get($idx)
                            .copied()
                            .map(String::from)
                            .unwrap_or_else(|| format!("arg{}", $idx + 1));
                        format!("{}: {}", name, <$T as LuaType>::lua_type())
                    }),+
                ];
                parts.join(", ")
            }
        }
    };
}

impl_lua_type_tuple!(1, 0: A);
impl_lua_type_tuple!(2, 0: A, 1: B);
impl_lua_type_tuple!(3, 0: A, 1: B, 2: C);
impl_lua_type_tuple!(4, 0: A, 1: B, 2: C, 3: D);
impl_lua_type_tuple!(5, 0: A, 1: B, 2: C, 3: D, 4: E);
impl_lua_type_tuple!(6, 0: A, 1: B, 2: C, 3: D, 4: E, 5: F);
impl_lua_type_tuple!(7, 0: A, 1: B, 2: C, 3: D, 4: E, 5: F, 6: G);
impl_lua_type_tuple!(8, 0: A, 1: B, 2: C, 3: D, 4: E, 5: F, 6: G, 7: H);

// LuaType for tuples (multi-return values).
macro_rules! impl_lua_type_for_tuple {
    ($($idx:tt: $T:ident),+ $(,)?) => {
        impl<$($T: LuaType),+> LuaType for ($($T,)+) {
            fn lua_type() -> String {
                vec![$(<$T as LuaType>::lua_type()),+].join(", ")
            }
        }
    };
}

impl_lua_type_for_tuple!(0: A);
impl_lua_type_for_tuple!(0: A, 1: B);
impl_lua_type_for_tuple!(0: A, 1: B, 2: C);
impl_lua_type_for_tuple!(0: A, 1: B, 2: C, 3: D);
impl_lua_type_for_tuple!(0: A, 1: B, 2: C, 3: D, 4: E);
impl_lua_type_for_tuple!(0: A, 1: B, 2: C, 3: D, 4: E, 5: F);
impl_lua_type_for_tuple!(0: A, 1: B, 2: C, 3: D, 4: E, 5: F, 6: G);
impl_lua_type_for_tuple!(0: A, 1: B, 2: C, 3: D, 4: E, 5: F, 6: G, 7: H);

impl<T: LuaType> LuaType for mlua::Variadic<T> {
    fn lua_type() -> String {
        T::lua_type()
    }
}

impl<T: LuaType> LuaTypeTuple for mlua::Variadic<T> {
    const ARITY: usize = 1;
    fn lua_param_list(_param_names: &[&'static str]) -> String {
        format!("...: {}", T::lua_type())
    }
}

/// Typed wrapper around `mlua::Function` whose `LuaType` renders as a
/// full `fun(args): ret` signature instead of the bare `function`.
///
/// Use as a parameter type for handler callbacks (`smelt.signal.subscribe(name, handler)`,
/// `smelt.cmd.register`, …) so plugin authors see the callback shape in
/// IDE completion and reference docs.
///
/// `P` is a parameter tuple (use `()` for no args, `Foo` for a single
/// arg, `(A, B, ...)` for multiple). `R` is the return type. Internally
/// the value behaves exactly like `mlua::Function`.
#[derive(Clone, Debug)]
pub struct LuaCallback<P: LuaTypeTuple, R: LuaType> {
    inner: mlua::Function,
    _phantom: std::marker::PhantomData<fn(P) -> R>,
}

impl<P: LuaTypeTuple, R: LuaType> LuaCallback<P, R> {
    pub fn into_inner(self) -> mlua::Function {
        self.inner
    }

    pub fn as_function(&self) -> &mlua::Function {
        &self.inner
    }
}

impl<P: LuaTypeTuple, R: LuaType> std::ops::Deref for LuaCallback<P, R> {
    type Target = mlua::Function;
    fn deref(&self) -> &mlua::Function {
        &self.inner
    }
}

impl<P: LuaTypeTuple, R: LuaType> mlua::FromLua for LuaCallback<P, R> {
    fn from_lua(value: mlua::Value, lua: &mlua::Lua) -> mlua::Result<Self> {
        let inner: mlua::Function = mlua::FromLua::from_lua(value, lua)?;
        Ok(Self {
            inner,
            _phantom: std::marker::PhantomData,
        })
    }
}

impl<P: LuaTypeTuple, R: LuaType> mlua::IntoLua for LuaCallback<P, R> {
    fn into_lua(self, lua: &mlua::Lua) -> mlua::Result<mlua::Value> {
        self.inner.into_lua(lua)
    }
}

impl<P: LuaTypeTuple, R: LuaType> LuaType for LuaCallback<P, R> {
    fn lua_type() -> String {
        // Default single-arg callbacks to `value` instead of `arg1` for
        // a more idiomatic-looking `fun(value: any)`. Higher arities fall
        // back to the generic `arg1, arg2, …` numbering.
        let params = if P::ARITY == 1 {
            P::lua_param_list(&["value"])
        } else {
            P::lua_param_list(&[])
        };
        let ret = R::lua_type();
        if ret == "nil" {
            format!("fun({params})")
        } else {
            format!("fun({params}): {ret}")
        }
    }
}

impl<P: LuaTypeTuple, R: LuaType> LuaTypeTuple for LuaCallback<P, R> {
    const ARITY: usize = 1;
    fn lua_param_list(param_names: &[&'static str]) -> String {
        let name = param_names.first().copied().unwrap_or("handler");
        format!("{}: {}", name, <Self as LuaType>::lua_type())
    }
}

impl LuaType for crate::lua::api::layout::LuaBlockLayout {
    fn lua_type() -> String {
        crate::lua::doc::record_class(LuaClassDecl {
            name: "smelt.layout.Node",
            doc: "Opaque block-layout node returned by `smelt.layout.*` constructors and accepted by transcript renderers, tool previews, and other content-layout APIs.",
            fields: Vec::new(),
        });
        "smelt.layout.Node".into()
    }
}

// ── LuaOpts / LuaAlias for derived types ─────────────────────────────

/// One field of a Lua class record (`---@field name?: type` in LuaCATS).
#[derive(Clone, Debug)]
pub struct LuaClassField {
    pub name: &'static str,
    /// LuaCATS-rendered type without the trailing `?` for optional
    /// fields - `optional` carries that information separately so the
    /// emitter can place the `?` on the field name.
    pub ty: String,
    pub optional: bool,
    pub doc: &'static str,
}

/// A `---@class` block declaration. Built by the
/// [`#[derive(LuaOpts)]`][lua_doc_derive::LuaOpts] macro.
#[derive(Clone, Debug)]
pub struct LuaClassDecl {
    pub name: &'static str,
    pub doc: &'static str,
    pub fields: Vec<LuaClassField>,
}

/// Build a `Vec<LuaClassField>` from method signatures so Lua-side
/// type strings are derived from actual Rust types instead of
/// hand-written strings.
#[macro_export]
macro_rules! class_methods {
    ($($name:literal => fn($($param:ident: $pty:ty),* $(,)?) -> $rty:ty, $doc:literal),* $(,)?) => {
        vec![
            $(
                $crate::lua::lua_type::LuaClassField {
                    name: $name,
                    ty: {
                        #[allow(unused_mut)]
                        let mut __params: Vec<String> = Vec::new();
                        $(__params.push(format!("{}: {}", stringify!($param), <$pty as $crate::lua::lua_type::LuaType>::lua_type()));)*
                        let __ret = <$rty as $crate::lua::lua_type::LuaType>::lua_type();
                        if __params.is_empty() {
                            format!("fun(): {}", __ret)
                        } else {
                            let mut __sig = String::new();
                            for (i, p) in __params.iter().enumerate() {
                                if i > 0 { __sig.push_str(", "); }
                                __sig.push_str(p);
                            }
                            format!("fun({}): {}", __sig, __ret)
                        }
                    },
                    optional: false,
                    doc: $doc,
                },
            )*
        ]
    };
}

/// A struct that maps to a Lua opts-bag table. Implementing types are
/// usually generated by `#[derive(LuaOpts)]`; the trait carries the
/// metadata gen-lua-docs needs to render `---@class` records.
pub trait LuaOpts {
    fn lua_class_decl() -> LuaClassDecl;
}

/// A `---@alias` block declaration. Built by the
/// [`#[derive(LuaAlias)]`][lua_doc_derive::LuaAlias] macro on
/// unit-only enums.
#[derive(Clone, Debug)]
pub struct LuaAliasDecl {
    pub name: &'static str,
    pub doc: &'static str,
    /// Known string literals. For closed aliases this is the full
    /// accepted set; for open aliases (`open: true`) it's a
    /// non-exhaustive list of well-known names that surfaces as
    /// IDE autocomplete hints alongside an unconstrained `string`.
    pub variants: Vec<&'static str>,
    /// When true, the alias accepts any string at runtime - the
    /// LuaCATS form becomes `string | "literal1" | "literal2" | …`,
    /// which gives lua-language-server autocomplete for the known
    /// names without rejecting plugin-defined ones.
    pub open: bool,
}

/// An enum that maps to a string-literal union on the Lua side.
pub trait LuaAlias {
    fn lua_alias_decl() -> LuaAliasDecl;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primitives() {
        assert_eq!(<bool as LuaType>::lua_type(), "boolean");
        assert_eq!(<u64 as LuaType>::lua_type(), "integer");
        assert_eq!(<String as LuaType>::lua_type(), "string");
        assert_eq!(<() as LuaType>::lua_type(), "nil");
    }

    #[test]
    fn containers_recurse() {
        assert_eq!(<Option<u64> as LuaType>::lua_type(), "integer?");
        assert_eq!(<Vec<String> as LuaType>::lua_type(), "string[]");
        assert_eq!(<Option<Vec<u64>> as LuaType>::lua_type(), "integer[]?");
    }

    #[test]
    fn tuple_param_list_with_names() {
        let s = <(u64, bool) as LuaTypeTuple>::lua_param_list(&["buf", "ro"]);
        assert_eq!(s, "buf: integer, ro: boolean");
    }

    #[test]
    fn tuple_param_list_falls_back_to_arg_n() {
        let s = <(u64, String) as LuaTypeTuple>::lua_param_list(&["buf"]);
        assert_eq!(s, "buf: integer, arg2: string");
    }

    #[test]
    fn empty_tuple_renders_empty() {
        assert_eq!(<() as LuaTypeTuple>::lua_param_list(&[]), "");
    }
}
