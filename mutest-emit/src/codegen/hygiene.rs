use std::mem;

use rustc_hash::FxHashSet;
use rustc_middle::ty::TyCtxt;
use smallvec::{SmallVec, smallvec};
use thin_vec::ThinVec;

use crate::analysis::ast_lowering;
use crate::analysis::hir::{self, LOCAL_CRATE};
use crate::analysis::res;
use crate::analysis::ty;
use crate::codegen::ast::{self, AstDeref, P};
use crate::codegen::ast::mut_visit::MutVisitor;
use crate::codegen::symbols::{DUMMY_SP, ExpnKind, Ident, MacroKind, Span, Symbol, kw};
use crate::codegen::symbols::hygiene::ExpnData;

fn is_macro_expn(expn: &ExpnData) -> bool {
    match expn.kind {
        ExpnKind::Macro(MacroKind::Bang, _) => true,

        | ExpnKind::Root
        | ExpnKind::Macro(_, _)
        | ExpnKind::AstPass(_)
        | ExpnKind::Desugaring(_)
        => false,
    }
}

fn is_macro_expn_span(span: Span) -> bool {
    is_macro_expn(&span.ctxt().outer_expn_data())
}

fn sanitize_ident_if_from_expansion(ident: &mut Ident) {
    let expn = ident.span.ctxt().outer_expn_data();
    if !is_macro_expn(&expn) { return; }

    let (is_label, bare_ident) = match ident.as_str().strip_prefix("'") {
        Some(bare_ident) => (true, bare_ident),
        None => (false, ident.as_str()),
    };

    assert!(!bare_ident.starts_with("__rustc_expn_"), "encountered ident starting with `__rustc_expn_` at {:?}: the macro might have been sanitized twice", ident.span);

    let expn_id = ident.span.ctxt().outer_expn();

    ident.name = Symbol::intern(&format!("{prefix}__rustc_expn_{expn_crate_id}_{expn_local_id}_{bare_ident}",
        prefix = is_label.then(|| "'").unwrap_or(""),
        expn_crate_id = expn_id.krate.index(),
        expn_local_id = expn_id.local_id.index(),
    ));
}

struct MacroExpansionSanitizer<'tcx, 'op> {
    tcx: TyCtxt<'tcx>,
    def_res: &'op ast_lowering::DefResolutions,

    /// Keep track of the current scope (e.g. items and bodies) for relative name resolution.
    current_scope: Option<hir::DefId>,
    /// We do not want to sanitize some idents (mostly temporarily) in the AST.
    /// During the visit we keep track of these so that they can be exluded from sanitization.
    protected_idents: FxHashSet<Ident>,
}

impl<'tcx, 'op> MacroExpansionSanitizer<'tcx, 'op> {
    fn overwrite_path_with_def_path(&self, path: &mut ast::Path, def_id_path: &[hir::DefId], mut relative: bool) {
        let mut segments_with_generics = path.segments.iter()
            .filter_map(|segment| segment.args.as_ref().and_then(|args| self.def_res.node_res(segment.id).and_then(|res| res.opt_def_id()).map(|def_id| (def_id, args.clone()))))
            .collect::<Vec<_>>();

        let mut segments = def_id_path.iter()
            .flat_map(|&def_id| {
                // NOTE: The only reason we do not use `tcx.opt_item_ident(def_id)?` here is that
                //       it panics if no span is found, which happens for crate root defs.
                let span = self.tcx.def_ident_span(def_id).unwrap_or(DUMMY_SP);
                let name = match () {
                    _ if def_id == LOCAL_CRATE.as_def_id() => {
                        // We must not make the path global if we use the `crate` keyword.
                        relative = true;

                        kw::Crate
                    }
                    _ => self.tcx.opt_item_name(def_id)?,
                };
                let mut ident = Ident::new(name, span);
                sanitize_ident_if_from_expansion(&mut ident);

                let mut segment = ast::PathSegment { id: ast::DUMMY_NODE_ID, ident, args: None };

                // Copy matching generic args from the corresponding segment in the original path.
                if let Some((_, mut args)) = segments_with_generics.extract_if(|(segment_def_id, _)| *segment_def_id == def_id).next() {
                    // Sanitize associated constraint idents.
                    match args.ast_deref_mut() {
                        ast::GenericArgs::AngleBracketed(args) => 'arm: {
                            // Skip sanitization if it is only an argument list and there are no references to assoc items.
                            // NOTE: This is also needed to avoid attempting to fetch assoc items for e.g. generic function calls.
                            if !args.args.iter().any(|arg| matches!(arg, ast::AngleBracketedArg::Constraint(..))) { break 'arm; }

                            let assoc_items = self.tcx.associated_items(def_id);

                            for arg in &mut args.args {
                                match arg {
                                    ast::AngleBracketedArg::Constraint(assoc_constraint) => {
                                        let assoc_kind = match &assoc_constraint.kind {
                                            ast::AssocConstraintKind::Equality { term } => {
                                                match term {
                                                    ast::Term::Ty(..) => ty::AssocKind::Type,
                                                    ast::Term::Const(..) => ty::AssocKind::Const,
                                                }
                                            }
                                            ast::AssocConstraintKind::Bound { .. } => ty::AssocKind::Type,
                                        };

                                        if let Some(assoc_item) = assoc_items
                                            .filter_by_name_unhygienic(assoc_constraint.ident.name)
                                            .find(|assoc_item| assoc_item.kind == assoc_kind)
                                        {
                                            // Copy and sanitize assoc item definition ident.
                                            let Some(assoc_item_ident) = self.tcx.opt_item_ident(assoc_item.def_id) else { unreachable!() };
                                            assoc_constraint.ident = assoc_item_ident;
                                            sanitize_ident_if_from_expansion(&mut assoc_constraint.ident);
                                        }
                                    }
                                    ast::AngleBracketedArg::Arg(_) => {}
                                }
                            }
                        }
                        ast::GenericArgs::Parenthesized(_) => {}
                    }

                    // Copy modified args.
                    segment.args = Some(args);
                }

                Some(segment)
            })
            .collect::<ThinVec<_>>();

        // Write non-relative paths as global paths to make sure that no name conflicts arise.
        if !relative {
            segments.insert(0, ast::PathSegment::path_root(path.span));
        }

        *path = ast::Path { span: path.span, segments, tokens: None };

        assert!(segments_with_generics.is_empty(), "path at {span:?} contained segments with generics which could not be matched against the new path segments",
            span = path.span,
        );
    }

    fn adjust_path_from_expansion(&self, path: &mut ast::Path, res: hir::Res<ast::NodeId>) {
        match res {
            hir::Res::Local(_) => {
                let Some(last_segment) = path.segments.last_mut() else { unreachable!() };
                sanitize_ident_if_from_expansion(&mut last_segment.ident);
            }

            hir::Res::Def(def_kind, def_id) => {
                match def_kind {
                    | hir::DefKind::Mod
                    | hir::DefKind::Struct
                    | hir::DefKind::Union
                    | hir::DefKind::Enum
                    | hir::DefKind::Variant
                    | hir::DefKind::Trait
                    | hir::DefKind::TyAlias { .. }
                    | hir::DefKind::TraitAlias
                    | hir::DefKind::ForeignMod
                    | hir::DefKind::ForeignTy
                    | hir::DefKind::Fn
                    | hir::DefKind::Const
                    | hir::DefKind::Static { .. }
                    | hir::DefKind::Ctor(..)
                    | hir::DefKind::AssocTy
                    | hir::DefKind::AssocFn
                    | hir::DefKind::AssocConst
                    => {
                        let visible_paths = res::visible_def_paths(self.tcx, def_id, self.current_scope);

                        match &visible_paths[..] {
                            [visible_path, ..] => {
                                let def_id_path = visible_path.def_id_path().collect::<Vec<_>>();
                                self.overwrite_path_with_def_path(path, &def_id_path, false);
                            }
                            [] => {
                                // Ensure that the def is in the current scope, otherwise it really is not visible from here.
                                let Some(mut current_scope) = self.current_scope else {
                                    panic!("{def_id:?} is not accessible in this crate at {span:?}",
                                        span = path.span,
                                    );
                                };
                                if !self.tcx.is_descendant_of(def_id, current_scope) {
                                    'fail: {
                                        // For impls, we can make an adjustment and try to find a relative path from the parent scope.
                                        if matches!(self.tcx.def_kind(current_scope), hir::DefKind::Impl { .. }) {
                                            let parent_scope = self.tcx.parent(current_scope);
                                            // Adjustment succeeded, escape failing case.
                                            if self.tcx.is_descendant_of(def_id, parent_scope) {
                                                current_scope = parent_scope;
                                                break 'fail;
                                            }
                                        }

                                        panic!("{def_id:?} is not defined in the scope {current_scope:?} and is not otherwise accessible at {span:?}",
                                            span = path.span,
                                        );
                                    }
                                }

                                let def_id_path = res::def_id_path(self.tcx, def_id);
                                let scope_def_id_path = res::def_id_path(self.tcx, current_scope);
                                let relative_def_id_path = &def_id_path[scope_def_id_path.len()..];
                                self.overwrite_path_with_def_path(path, &relative_def_id_path, true);
                            }
                        }
                    }

                    | hir::DefKind::TyParam
                    | hir::DefKind::LifetimeParam
                    | hir::DefKind::ConstParam
                    => {
                        let Some(last_segment) = path.segments.last_mut() else { unreachable!() };
                        sanitize_ident_if_from_expansion(&mut last_segment.ident);
                    }

                    hir::DefKind::Field => {
                        // TODO
                    }

                    | hir::DefKind::Macro(..)
                    | hir::DefKind::ExternCrate
                    | hir::DefKind::Use
                    | hir::DefKind::AnonConst
                    | hir::DefKind::InlineConst
                    | hir::DefKind::OpaqueTy
                    | hir::DefKind::GlobalAsm
                    | hir::DefKind::Impl { .. }
                    | hir::DefKind::Closure
                    => {}
                }
            }

            | hir::Res::PrimTy(..)
            | hir::Res::SelfTyParam { .. }
            | hir::Res::SelfTyAlias { .. }
            | hir::Res::SelfCtor(..)
            | hir::Res::ToolMod
            | hir::Res::NonMacroAttr(..)
            | hir::Res::Err
            => {}
        }
    }
}

impl<'tcx, 'op> ast::mut_visit::MutVisitor for MacroExpansionSanitizer<'tcx, 'op> {
    fn flat_map_item(&mut self, item: P<ast::Item>) -> SmallVec<[P<ast::Item>; 1]> {
        // Skip generated items corresponding to compiler (and mutest-rs) internals.
        if item.id == ast::DUMMY_NODE_ID || item.span == DUMMY_SP { return smallvec![item]; }

        let Some(def_id) = self.def_res.node_id_to_def_id.get(&item.id) else { unreachable!() };
        let previous_scope = mem::replace(&mut self.current_scope, Some(def_id.to_def_id()));
        let item = ast::mut_visit::noop_flat_map_item(item, self);
        self.current_scope = previous_scope;

        item
    }

    fn visit_attribute(&mut self, _attr: &mut ast::Attribute) {
        // NOTE: We do not descend into attributes, there is nothing we
        //       would want to sanitize in them, or nested in them.
    }

    fn visit_constraint(&mut self, assoc_constraint: &mut ast::AssocConstraint) {
        // NOTE: We do not alter the idents of associated constraints here.
        //       These get resolved in `adjust_path_from_expansion`.
        self.protected_idents.insert(assoc_constraint.ident);
        ast::mut_visit::noop_visit_constraint(assoc_constraint, self);
        self.protected_idents.remove(&assoc_constraint.ident);
    }

    fn visit_expr(&mut self, expr: &mut P<ast::Expr>) {
        let mut protected_ident = None;

        match &mut expr.kind {
            ast::ExprKind::Struct(struct_expr) => 'arm: {
                let Some(res) = self.def_res.node_res(struct_expr.path.segments.last().unwrap().id) else { break 'arm; };

                // Expect a struct, union, or enum variant, and get the corresponding ADT variant.
                let variant_def = self.tcx.expect_variant_res(res.expect_non_local());
                for field in &mut struct_expr.fields {
                    let Some(field_def) = variant_def.fields.iter().find(|field_def| self.tcx.hygienic_eq(field.ident, field_def.ident(self.tcx), variant_def.def_id)) else {
                        panic!("field {ident} at {span:?} does not match any field of {variant_def_id:?}",
                            ident = field.ident,
                            span = field.span,
                            variant_def_id = variant_def.def_id,
                        );
                    };
                    // HACK: Copy ident from definition for correct sanitization later.
                    field.ident = field_def.ident(self.tcx);
                    // NOTE: We have to disable shorthand syntax to ensure that
                    //       the correct field ident appears in printed code.
                    field.is_shorthand = false;
                }
            }
            ast::ExprKind::Field(_, field_ident) => 'arm: {
                // Idents cannot start with a digit, therefore they must correspond
                // to an unnamed field reference which must not be sanitized.
                if field_ident.name.as_str().starts_with(|c: char| c.is_ascii_digit()) {
                    protected_ident = Some(*field_ident);
                    break 'arm;
                }

                // NOTE: We need to eventually resolve fields through the typed HIR, as these are
                //       highly type-dependent and their name resolution is performed during type checking.
                // TODO: Use the HIR Field node to call `tcx.typeck_results().field_index(expr_hir.hir_id)`.
                //       Sanitize the field ident if the referenced field is from a macro expansion.
            }
            ast::ExprKind::MethodCall(call) => {
                // NOTE: We need to eventually resolve method calls through the typed HIR, as these are
                //       highly type-dependent and their name resolution is performed during type checking.
                // TODO: Use the HIR MethodCall node to call `tcx.typeck_results().type_dependent_def_id(expr_hir.hir_id)`.
                //       Sanitize the function ident if the called definition is from a macro expansion.
                // HACK: For now, we do not sanitize method call idents, which is the more likely scenario
                //       (i.e. the associated function is not defined in a trait defined by a macro).
                protected_ident = Some(call.seg.ident);
            }
            _ => {}
        }

        if let Some(protected_ident) = protected_ident { self.protected_idents.insert(protected_ident); }
        ast::mut_visit::noop_visit_expr(expr, self);
        if let Some(protected_ident) = protected_ident { self.protected_idents.remove(&protected_ident); }
    }

    fn visit_pat(&mut self, pat: &mut P<ast::Pat>) {
        match &mut pat.kind {
            ast::PatKind::Struct(_, path, fields, _) => 'arm: {
                let Some(res) = self.def_res.node_res(path.segments.last().unwrap().id) else { break 'arm; };

                // Expect a struct, union, or enum variant, and get the corresponding ADT variant.
                let variant_def = self.tcx.expect_variant_res(res.expect_non_local());
                for field in fields {
                    let Some(field_def) = variant_def.fields.iter().find(|field_def| self.tcx.hygienic_eq(field.ident, field_def.ident(self.tcx), variant_def.def_id)) else {
                        panic!("field {ident} at {span:?} does not match any field of {variant_def_id:?}",
                            ident = field.ident,
                            span = field.span,
                            variant_def_id = variant_def.def_id,
                        );
                    };
                    // HACK: Copy ident from definition for correct sanitization later.
                    field.ident = field_def.ident(self.tcx);
                    // NOTE: We have to disable shorthand syntax to ensure that
                    //       the correct field ident appears in printed code.
                    field.is_shorthand = false;
                }
            }
            _ => {}
        }

        ast::mut_visit::noop_visit_pat(pat, self);
    }

    fn visit_path(&mut self, path: &mut ast::Path) {
        // NOTE: We explicitly only visit the generic arguments, as we will
        //       sanitize the ident segments afterwards.
        for segment in &mut path.segments {
            if let Some(args) = &mut segment.args {
                self.visit_generic_args(args);
            }
        }

        // Short-circuit if not in a macro expansion, as there is no
        // other child node which could be from a macro expansion.
        if !is_macro_expn_span(path.span) { return; }

        let Some(last_segment) = path.segments.last() else { return; };
        let Some(res) = self.def_res.node_res(last_segment.id) else { return; };
        self.adjust_path_from_expansion(path, res);
    }

    fn visit_ident(&mut self, ident: &mut Ident) {
        if self.protected_idents.contains(ident) { return; }
        if ident.name == kw::SelfLower { return; }
        sanitize_ident_if_from_expansion(ident);
    }

    fn visit_vis(&mut self, vis: &mut ast::Visibility) {
        // Short-circuit if not in a macro expansion, as there is no
        // other child node which could be from a macro expansion.
        if !is_macro_expn_span(vis.span) { return; }

        match &mut vis.kind {
            ast::VisibilityKind::Restricted { path, id, .. } => {
                let Some(res) = self.def_res.node_res(*id) else { return; };
                self.adjust_path_from_expansion(path, res);
            }
            _ => ast::mut_visit::noop_visit_vis(vis, self),
        }
    }
}

pub fn sanitize_macro_expansions<'tcx>(tcx: TyCtxt<'tcx>, def_res: &ast_lowering::DefResolutions, krate: &mut ast::Crate) {
    let mut sanitizer = MacroExpansionSanitizer {
        tcx,
        def_res,
        current_scope: Some(LOCAL_CRATE.as_def_id()),
        protected_idents: Default::default(),
    };
    sanitizer.visit_crate(krate);
}
