//@ build
//@ stderr: empty
//@ aux-build: proc_macro_derive_with_helper_attrs.rs

extern crate proc_macro_derive_with_helper_attrs;
use proc_macro_derive_with_helper_attrs::*;

#[allow(unused)]
#[derive(DeriveMacroWithAttr)]
#[foo]
#[bar]
struct A;
