pub const GENERATED_CODE_PRELUDE: &str = r#"
#![allow(unused_features)]
#![allow(unused_imports)]

#![feature(rustc_attrs)]
#![feature(int_error_internals)]
#![feature(fmt_internals)]
#![feature(str_internals)]
#![feature(sort_internals)]
#![feature(print_internals)]
#![feature(allocator_internals)]
#![feature(char_error_internals)]
#![feature(libstd_sys_internals)]
#![feature(thread_local_internals)]
#![feature(libstd_thread_internals)]

#![feature(box_syntax)]
#![feature(core_intrinsics)]
#![feature(core_panic)]
#![feature(derive_clone_copy)]
#![feature(derive_eq)]
#![feature(no_coverage)]
#![feature(rustc_private)]
#![feature(structural_match)]
"#;

pub const GENERATED_CODE_CRATE_REFS: &str = r#"
extern crate alloc;
"#;
