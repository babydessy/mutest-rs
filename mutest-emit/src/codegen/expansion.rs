use std::mem;
use std::path::{self, Path, PathBuf};

use rustc_expand::base::ResolverExpand;
use rustc_resolve::Resolver;
use rustc_session::Session;
use rustc_session::parse::ParseSess;
use smallvec::{SmallVec, smallvec};

use crate::analysis::hir;
use crate::codegen::ast::{self, P};
use crate::codegen::ast::mut_visit::MutVisitor;
use crate::codegen::symbols::{DUMMY_SP, ExpnKind, FileName, Ident, MacroKind, Span, sym};
use crate::codegen::symbols::hygiene::AstPass;

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

#![feature(allocator_api)]
#![feature(box_syntax)]
#![feature(cfg_target_thread_local)]
#![feature(core_intrinsics)]
#![feature(core_panic)]
#![feature(derive_clone_copy)]
#![feature(derive_eq)]
#![feature(no_coverage)]
#![feature(rustc_private)]
#![feature(structural_match)]
#![feature(thread_local)]
"#;

pub fn insert_generated_code_crate_refs(resolver: &mut Resolver, krate: &mut ast::Crate) {
    let expn_id = resolver.expansion_for_ast_pass(
        DUMMY_SP,
        AstPass::StdImports,
        &[sym::rustc_attrs],
        None,
    );
    let def_site = DUMMY_SP.with_def_site_ctxt(expn_id.to_expn_id());

    // extern crate alloc;
    if !krate.items.iter().any(|item| ast::inspect::is_extern_crate_decl(item, sym::alloc)) {
        krate.items.push(ast::mk::item_extern_crate(def_site, sym::alloc, None));
    }
}

#[derive(Copy, Clone)]
enum DirOwnership {
    Owned {
        // `None` if `mod.rs`, `Some("foo")` if `foo.rs`.
        relative: Option<Ident>,
    },
    UnownedViaBlock,
}

fn mod_file_path_from_attr(
    dir_path: &Path,
    attrs: &[ast::Attribute],
) -> Option<PathBuf> {
    let first_path = attrs.iter().find(|attr| attr.has_name(sym::path))?;
    let Some(path_sym) = first_path.value_str() else { unreachable!() };
    let path_str = path_sym.as_str();

    // On windows, the base path might have the form `\\?\foo\bar` in which case it does not tolerate mixed `/` and `\`
    // separators, so canonicalize `/` to `\`.
    #[cfg(windows)]
    let path_str = path_str.replace("/", "\\");

    Some(dir_path.join(path_str))
}

struct ModulePath {
    pub file_path: PathBuf,
    pub dir_ownership: DirOwnership,
}

fn default_submod_path<'a>(
    sess: &'a ParseSess,
    ident: Ident,
    relative: Option<Ident>,
    dir_path: &Path,
) -> ModulePath {
    // If we are in a foo.rs file instead of a mod.rs file, we need to look for submodules in
    // `./foo/<ident>.rs` and `./foo/<ident>/mod.rs` rather than
    // `./<ident>.rs` and `./<ident>/mod.rs`.
    let relative_prefix = match relative {
        Some(ident) => format!("{}{}", ident.name, path::MAIN_SEPARATOR),
        None => "".to_owned(),
    };
    let mod_name = ident.name.to_string();

    let default_path = dir_path.join(format!("{relative_prefix}{mod_name}.rs"));
    let secondary_path = dir_path.join(format!("{relative_prefix}{mod_name}")).join("mod.rs");
    let default_exists = sess.source_map().file_exists(&default_path);
    let secondary_exists = sess.source_map().file_exists(&secondary_path);

    match (default_exists, secondary_exists) {
        (true, false) => ModulePath {
            file_path: default_path,
            dir_ownership: DirOwnership::Owned { relative: Some(ident) },
        },
        (false, true) => ModulePath {
            file_path: secondary_path,
            dir_ownership: DirOwnership::Owned { relative: None },
        },
        (false, false) => panic!("module file not found"),
        (true, true) => panic!("multiple module candidates"),
    }
}

fn mod_file_path(
    sess: &Session,
    dir_path: &Path,
    dir_ownership: DirOwnership,
    ident: Ident,
    attrs: &[ast::Attribute],
) -> ModulePath {
    if let Some(file_path) = mod_file_path_from_attr(dir_path, attrs) {
        let dir_ownership = DirOwnership::Owned { relative: None };
        return ModulePath { file_path, dir_ownership };
    }

    let relative = match dir_ownership {
        DirOwnership::Owned { relative } => relative,
        DirOwnership::UnownedViaBlock => None,
    };

    let module_path = default_submod_path(&sess.parse_sess, ident, relative, dir_path);

    match dir_ownership {
        DirOwnership::Owned { .. } => module_path,
        DirOwnership::UnownedViaBlock => panic!("non-inline module inside a block without a path attribute"),
    }
}

fn mod_dir_path(
    sess: &Session,
    dir_path: &Path,
    mut dir_ownership: DirOwnership,
    ident: Ident,
    attrs: &[ast::Attribute],
    inline: ast::Inline,
) -> (PathBuf, DirOwnership) {
    match inline {
        ast::Inline::Yes if let Some(file_path) = mod_file_path_from_attr(dir_path, attrs) => {
            (file_path, DirOwnership::Owned { relative: None })
        }
        ast::Inline::Yes => {
            let mut dir_path = dir_path.to_owned();
            if let DirOwnership::Owned { relative } = &mut dir_ownership {
                if let Some(ident) = relative.take() {
                    dir_path.push(ident.as_str());
                }
            }
            dir_path.push(ident.as_str());

            (dir_path, dir_ownership)
        }
        ast::Inline::No => {
            let module_path = mod_file_path(sess, dir_path, dir_ownership, ident, &attrs);
            let file_path = module_path.file_path;
            dir_ownership = module_path.dir_ownership;

            // Extract the directory path for submodules of the module.
            let dir_path = file_path.parent().unwrap_or(&file_path).to_owned();

            (dir_path, dir_ownership)
        }
    }
}

struct ExternalMod {
    pub file_path: PathBuf,
    pub dir_path: PathBuf,
    pub dir_ownership: DirOwnership,
    pub spans: ast::ModSpans,
    pub items: Vec<P<ast::Item>>,
}

fn parse_external_mod(
    sess: &Session,
    dir_path: &Path,
    dir_ownership: DirOwnership,
    ident: Ident,
    span: Span,
    attrs: &mut Vec<ast::Attribute>,
) -> ExternalMod {
    let module_path = mod_file_path(sess, dir_path, dir_ownership, ident, attrs);

    let mut parser = rustc_parse::new_parser_from_file(&sess.parse_sess, &module_path.file_path, Some(span));
    let (mut inner_attrs, items, inner_span) = parser.parse_mod(&ast::token::Eof).expect("parsing module failed");
    attrs.append(&mut inner_attrs);

    // Extract the directory path for submodules of the module.
    let dir_path = module_path.file_path.parent().unwrap_or(&module_path.file_path).to_owned();

    let dir_ownership = module_path.dir_ownership;
    let file_path = module_path.file_path;
    let spans = inner_span;
    ExternalMod { file_path, dir_path, dir_ownership, spans, items }
}

struct ModuleLoader<'tcx> {
    sess: &'tcx Session,
    current_dir_path: PathBuf,
    current_dir_ownership: DirOwnership,
}

impl<'tcx> ast::mut_visit::MutVisitor for ModuleLoader<'tcx> {
    fn flat_map_item(&mut self, item: P<ast::Item>) -> SmallVec<[P<ast::Item>; 1]> {
        let mut item = item.into_inner();

        let ast::ItemKind::Mod(_, mod_kind) = &mut item.kind else {
            return ast::mut_visit::noop_flat_map_item(P(item), self);
        };

        let (_file_path, dir_path, dir_ownership) = match mod_kind {
            ast::ModKind::Loaded(_, inline, _) => {
                let (dir_path, dir_ownership) = mod_dir_path(self.sess, &self.current_dir_path, self.current_dir_ownership, item.ident, &item.attrs, *inline);
                (None, dir_path, dir_ownership)
            }
            ast::ModKind::Unloaded => {
                let external_mod = parse_external_mod(self.sess, &self.current_dir_path, self.current_dir_ownership, item.ident, item.span, &mut item.attrs);
                *mod_kind = ast::ModKind::Loaded(external_mod.items, ast::Inline::No, external_mod.spans);
                (Some(external_mod.file_path), external_mod.dir_path, external_mod.dir_ownership)
            }
        };

        let original_dir_path = mem::replace(&mut self.current_dir_path, dir_path);
        let original_dir_ownership = mem::replace(&mut self.current_dir_ownership, dir_ownership);

        let item = ast::mut_visit::noop_flat_map_item(P(item), self);

        self.current_dir_path = original_dir_path;
        self.current_dir_ownership = original_dir_ownership;

        item
    }

    fn visit_block(&mut self, block: &mut P<ast::Block>) {
        let original_dir_ownership = mem::replace(&mut self.current_dir_ownership, DirOwnership::UnownedViaBlock);
        ast::mut_visit::noop_visit_block(block, self);
        self.current_dir_ownership = original_dir_ownership;
    }
}

pub fn load_modules(sess: &Session, krate: &mut ast::Crate) {
    let file_path = match sess.source_map().span_to_filename(krate.spans.inner_span) {
        FileName::Real(name) => {
            name.into_local_path().expect("attempting to resolve a file path in an external file")
        }
        other => PathBuf::from(other.prefer_local().to_string()),
    };
    let dir_path = file_path.parent().unwrap_or(&file_path).to_owned();

    let mut loader = ModuleLoader {
        sess,
        current_dir_path: dir_path,
        current_dir_ownership: DirOwnership::Owned { relative: None },
    };

    loader.visit_crate(krate);
}

struct MacroExpansionReverter<'ast> {
    original_crate: &'ast ast::Crate,
    current_scope_in_original: &'ast [P<ast::Item>],
}

impl<'ast> ast::mut_visit::MutVisitor for MacroExpansionReverter<'ast> {
    fn visit_crate(&mut self, krate: &mut ast::Crate) {
        self.current_scope_in_original = &self.original_crate.items;
        ast::mut_visit::noop_visit_crate(krate, self);
    }

    fn flat_map_item(&mut self, mut item: P<ast::Item>) -> SmallVec<[P<ast::Item>; 1]> {
        let expn = item.span.ctxt().outer_expn_data();

        match expn.kind {
            ExpnKind::Root => {
                match &item.kind {
                    | ast::ItemKind::Use(_)
                    | ast::ItemKind::MacroDef(_) => { return smallvec![item]; }
                    _ => {}
                }

                let Some(original_item) = self.current_scope_in_original.iter().find(|i| i.span == item.span) else {
                    return smallvec![item];
                };

                match &item.kind {
                    | ast::ItemKind::Struct(_, _)
                    | ast::ItemKind::Enum(_, _)
                    | ast::ItemKind::Union(_, _) => {
                        // Copy definition body from original item.
                        item.kind = original_item.kind.clone();
                    }
                    _ => {}
                }

                // Copy attributes from original item.
                item.attrs = original_item.attrs.clone();

                match &original_item.kind {
                    ast::ItemKind::Mod(_, ast::ModKind::Unloaded) => panic!("encountered unloaded module"),

                    ast::ItemKind::Mod(_, ast::ModKind::Loaded(items, _, _)) => {
                        let original_scope_in_original = mem::replace(&mut self.current_scope_in_original, items);
                        let item = ast::mut_visit::noop_flat_map_item(item, self);
                        self.current_scope_in_original = original_scope_in_original;

                        item
                    }

                    // TODO: Descend into associated and foreign item scopes.
                    | ast::ItemKind::ForeignMod(_)
                    | ast::ItemKind::Trait(_)
                    | ast::ItemKind::Impl(_) => smallvec![item],

                    // TODO: Descend into bodies and handle nested items.
                    | ast::ItemKind::Static(_, _, _)
                    | ast::ItemKind::Const(_, _, _)
                    | ast::ItemKind::Fn(_) => smallvec![item],

                    _ => ast::mut_visit::noop_flat_map_item(item, self),
                }
            }

            ExpnKind::Macro(_, _) if expn.macro_def_id.map(hir::DefId::is_local).unwrap_or(false) => {
                // TODO: The "original" items produced by local macros would have to be created by partially expanding
                //       the macro calls. This is a large undertaking that we will get to eventually. For now, we ignore
                //       local macro subtrees.
                smallvec![item]
            }

            // TODO: Revert bang macro expansions.
            ExpnKind::Macro(MacroKind::Bang, _) => smallvec![item],

            | ExpnKind::Macro(MacroKind::Attr, _)
            | ExpnKind::Macro(MacroKind::Derive, _) => {
                // Only remove the macro expansion if we can recover the attributes from the original item.
                let Some(_original_item) = self.current_scope_in_original.iter().find(|i| i.span_with_attributes().contains(expn.call_site)) else {
                    return smallvec![item];
                };

                smallvec![]
            }

            ExpnKind::AstPass(AstPass::StdImports) => smallvec![item],
            ExpnKind::AstPass(AstPass::TestHarness) => smallvec![],
            ExpnKind::AstPass(AstPass::ProcMacroHarness) => smallvec![item],

            // HIR and MIR expansions are not performed on the AST.
            ExpnKind::Desugaring(_) | ExpnKind::Inlined => smallvec![item],
        }
    }
}

pub fn revert_non_local_macro_expansions<'ast>(expanded_crate: &mut ast::Crate, original_crate: &ast::Crate) {
    let mut reverter = MacroExpansionReverter {
        original_crate,
        current_scope_in_original: &[],
    };

    reverter.visit_crate(expanded_crate);
}
