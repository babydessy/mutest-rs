use std::iter;
use std::mem;
use std::panic;

use itertools::Itertools;
use rustc_data_structures::sync::Lrc;
use rustc_expand::base::{SyntaxExtension, SyntaxExtensionKind};
use rustc_hash::FxHashSet;
use rustc_hir_analysis::collect::ItemCtxt;
use rustc_metadata::creader::{CStore, LoadedMacro};
use rustc_span::edition::Edition;
use smallvec::{SmallVec, smallvec};
use thin_vec::{ThinVec, thin_vec};

use crate::analysis::ast_lowering;
use crate::analysis::hir::{self, LOCAL_CRATE, NodeExt};
use crate::analysis::res;
use crate::analysis::ty::{self, Ty, TyCtxt};
use crate::codegen::ast::{self, AstDeref, P};
use crate::codegen::ast::mut_visit::MutVisitor;
use crate::codegen::symbols::{DUMMY_SP, ExpnKind, Ident, Span, Symbol, sym, kw};
use crate::codegen::symbols::hygiene::{ExpnData, ExpnId, MacroKind, Transparency};

fn is_macro_expn(expn: &ExpnData) -> bool {
    match expn.kind {
        ExpnKind::Macro(_, _) => true,

        | ExpnKind::Root
        | ExpnKind::AstPass(_)
        | ExpnKind::Desugaring(_)
        => false,
    }
}

fn is_macro_expn_span(span: Span) -> bool {
    is_macro_expn(&span.ctxt().outer_expn_data())
}

enum IdentResKind {
    Local,
    Label,
    Def,
}

fn sanitize_ident_if_from_expansion(ident: &mut Ident, ident_res_kind: IdentResKind) {
    // `crate`, `self`, `super` and `Super` are keywords that cannot be raw identifiers, see
    // https://doc.rust-lang.org/reference/identifiers.html.
    if ident.name == kw::Crate { return; }
    if ident.name == kw::Super { return; }
    if ident.name == kw::SelfLower || ident.name == kw::SelfUpper { return; }
    // `_` is an ident but cannot be a raw identifier and cannot be used in paths.
    if ident.name == kw::Underscore || ident.name == kw::UnderscoreLifetime { return; }

    let syntax_ctxt = ident.span.ctxt();

    let mut res_ctxt_expn_id = None;
    for (expn_id, transparency) in syntax_ctxt.marks().into_iter().rev() {
        // Some identifiers produced by semi-transparent expansions (e.g. macro_rules macros)
        // may be resolved at call-site, rather than at definition-site, meaning that
        // they are not actually hygienic.
        if let Transparency::SemiTransparent = transparency {
            // Local variables and labels get resolved at definition-site (i.e. hygienic),
            // everything else at call-site (i.e. not hygienic).
            // NOTE: `$crate` also gets resolved at definition-site but is handled
            //       before our sanitization step.
            let (IdentResKind::Local | IdentResKind::Label) = ident_res_kind else { continue; };
        }

        res_ctxt_expn_id = Some(expn_id);
        break;
    }

    let Some(res_ctxt_expn_id) = res_ctxt_expn_id else { return; };
    let res_ctxt_expn = res_ctxt_expn_id.expn_data();
    if !is_macro_expn(&res_ctxt_expn) { return; }

    let (is_label, bare_ident) = match ident.as_str().strip_prefix("'") {
        Some(bare_ident) => (true, bare_ident),
        None => (false, ident.as_str()),
    };

    assert!(!bare_ident.starts_with("__rustc_expn_"), "encountered ident starting with `__rustc_expn_` at {:?}: the macro might have been sanitized twice", ident.span);

    ident.name = Symbol::intern(&format!("{prefix}__rustc_expn_{expn_crate_id}_{expn_local_id}_{bare_ident}",
        prefix = is_label.then(|| "'").unwrap_or(""),
        expn_crate_id = res_ctxt_expn_id.krate.index(),
        expn_local_id = res_ctxt_expn_id.local_id.index(),
    ));
}

/// Copy syntax context of definition-site ident span to use-site ident
/// so that the use-site ident can be appropriately sanitized by
/// `sanitize_ident_if_from_expansion`.
fn copy_def_span_ctxt(ident: &mut Ident, def_ident_span: Span) {
    ident.span = ident.span.with_ctxt(def_ident_span.ctxt());
}

pub fn sanitize_ident_if_def_from_expansion(ident: &mut Ident, def_ident_span: Span) {
    copy_def_span_ctxt(ident, def_ident_span);
    sanitize_ident_if_from_expansion(ident, IdentResKind::Def);
}

fn sanitize_standalone_ident_if_from_expansion(ident: &mut Ident, ident_res_kind: IdentResKind) {
    if ident.name == kw::StaticLifetime { return; }

    sanitize_ident_if_from_expansion(ident, ident_res_kind);
}

fn is_macro_helper_attr(syntax_extensions: &[SyntaxExtension], attr: &ast::Attribute) -> bool {
    syntax_extensions.iter().any(|syntax_extension| {
        syntax_extension.helper_attrs.iter().any(|&helper_attr| attr.has_name(helper_attr))
    })
}

#[derive(Clone, Copy)]
enum Macros2_0TopLevelRelativePathResHack {
    InTopLevelMacros2_0Scope { parent_module: hir::DefId },
    InNestedMacros2_0Scope(ExpnId),
    NotInMacros2_0Scope,
}

struct MacroExpansionSanitizer<'tcx, 'op> {
    tcx: TyCtxt<'tcx>,
    def_res: &'op ast_lowering::DefResolutions,
    /// Body resolutions for interfacing with the HIR.
    body_res: &'op ast_lowering::BodyResolutions<'tcx>,
    syntax_extensions: Vec<SyntaxExtension>,

    /// Keep track of the current scope (e.g. items and bodies) for relative name resolution.
    current_scope: Option<hir::DefId>,
    /// Keep track of the current type checking context for type-dependent name resolution.
    current_typeck_ctx: Option<&'tcx ty::TypeckResults<'tcx>>,

    /// In macros 2.0 scopes, relative paths get resolved in the scope of the macro definining module,
    /// but only at the top-level within the macro expansion.
    /// HACK: We use a marker to determine whether we are in a macros 2.0 scope, but not in a module nested inside.
    /// NOTE: This is because rustc currently does not accept (and correctly resolve) relative paths
    ///       in nested modules within macros 2.0 macro expansion scopes.
    ///       See `tests/ui/hygiene/rustc_res/relative_use_imports_do_not_work_in_nested_mod_within_macros_2_0`
    ///       to test whether this expectation is still met.
    macros_2_0_top_level_relative_path_res_hack: Macros2_0TopLevelRelativePathResHack,

    /// We do not want to sanitize some idents (mostly temporarily) in the AST.
    /// During the visit we keep track of these so that they can be exluded from sanitization.
    protected_idents: FxHashSet<Ident>,
}

impl<'tcx, 'op> MacroExpansionSanitizer<'tcx, 'op> {
    fn overwrite_path_with_def_path(&self, path: &mut ast::Path, def_path: &res::DefPath) {
        assert!(!def_path.segments.is_empty());

        let mut segments_with_generics = path.segments.iter()
            .filter_map(|segment| {
                let args = segment.args.as_ref()?;
                let res = self.def_res.node_res(segment.id)?;
                let mut def_id = res.opt_def_id()?;
                if let hir::DefKind::Ctor(..) = self.tcx.def_kind(def_id) {
                    // Adjust target definition to the parent, mirroring what is done in `adjust_path_from_expansion`,
                    // so that the segment def ids match up.
                    def_id = self.tcx.parent(def_id);
                }
                Some((def_id, args.clone()))
            })
            .collect::<Vec<_>>();

        let mut relative = !def_path.global;

        let mut segments = def_path.segments.iter()
            .flat_map(|def_path_segment| {
                let def_id = def_path_segment.def_id;

                let mut ident = match def_path_segment.reexport {
                    None => {
                        // NOTE: The only reason we do not use `tcx.opt_item_ident(def_id)?` here is that
                        //       it panics if no span is found, which happens for crate root defs.
                        let span = self.tcx.def_ident_span(def_id).unwrap_or(DUMMY_SP);
                        let name = match () {
                            _ if def_id == LOCAL_CRATE.as_def_id() => {
                                // We must not make the path global if we use the `crate` keyword.
                                relative = true;

                                kw::Crate
                            }

                            // Keep `Self` keyword segment in path from def path resolution.
                            _ if def_path_segment.ident.name == kw::SelfUpper => kw::SelfUpper,

                            _ => self.tcx.opt_item_name(def_id)?,
                        };
                        // TODO: Use path.span with copy_def_span_ctxt.
                        Ident::new(name, span)
                    }
                    Some(_) => def_path_segment.ident,
                };
                sanitize_ident_if_from_expansion(&mut ident, IdentResKind::Def);

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
                                            let Some(assoc_item_ident_span) = self.tcx.def_ident_span(assoc_item.def_id) else { unreachable!() };
                                            copy_def_span_ctxt(&mut assoc_constraint.ident, assoc_item_ident_span);
                                            sanitize_ident_if_from_expansion(&mut assoc_constraint.ident, IdentResKind::Def);
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
                } else {
                    // If there are no corresponding args in the original path, we may still have to
                    // add dummy generics with type inference holes for the required generics of the def
                    // to generate a valid path.
                    let has_generics = matches!(self.tcx.def_kind(def_id),
                        | hir::DefKind::Struct | hir::DefKind::Enum | hir::DefKind::TyAlias
                        | hir::DefKind::Trait | hir::DefKind::TraitAlias
                        | hir::DefKind::Fn | hir::DefKind::Const
                        | hir::DefKind::AssocTy | hir::DefKind::AssocFn | hir::DefKind::AssocConst
                    );
                    if has_generics {
                        let generics = self.tcx.generics_of(def_id);

                        let mut args = ThinVec::with_capacity(generics.own_params.len());
                        if generics.own_params.len() >= 1 {
                            for generic_param in &generics.own_params[(generics.has_self as usize)..] {
                                match &generic_param.kind {
                                    ty::GenericParamDefKind::Lifetime => {}
                                    ty::GenericParamDefKind::Type { has_default, synthetic } => {
                                        if *has_default || *synthetic { continue; }
                                        let ty = ast::mk::ty(DUMMY_SP, ast::TyKind::Infer);
                                        args.push(ast::AngleBracketedArg::Arg(ast::GenericArg::Type(ty)));
                                    }
                                    ty::GenericParamDefKind::Const { .. } => {}
                                }
                            }
                        }

                        if args.len() >= 1 {
                            segment.args = Some(P(ast::GenericArgs::AngleBracketed(ast::AngleBracketedArgs { span: DUMMY_SP, args })));
                        }
                    }
                }

                Some(segment)
            })
            .collect::<ThinVec<_>>();

        if let Some(root_ty) = def_path.type_root {
            let ident = root_ty.as_ident(DUMMY_SP);
            segments.insert(0, ast::PathSegment { id: ast::DUMMY_NODE_ID, ident, args: None });
            relative = true;
        }

        // Write non-relative paths as global paths to make sure that no name conflicts arise.
        if !relative {
            segments.insert(0, ast::PathSegment::path_root(path.span));
        }

        *path = ast::Path { span: path.span, segments, tokens: None };

        assert!(!path.segments.is_empty(), "path at {span:?} was sanitized into an empty path",
            span = path.span,
        );
    }

    fn expect_visible_def_path(&self, def_id: hir::DefId, span: Span, ignore_reexport: Option<hir::DefId>) -> res::DefPath {
        // Prefer using a direct, local path to local items within the same module as the enclosing module of the current scope.
        // NOTE: This helps avoid visibility-related resolution issues in local items, see
        //       `tests/ui/hygiene/rustc_res/private_ctor_not_available_in_same_scope_through_reexport.rs`.
        if let Some(current_scope) = self.current_scope && let Some(local_def_id) = def_id.as_local() && def_id != current_scope {
            let mod_scope = match self.tcx.def_kind(current_scope) {
                hir::DefKind::Mod => current_scope,
                _ => self.tcx.parent_module_from_def_id(current_scope.expect_local()).to_def_id(),
            };
            let containing_mod = self.tcx.parent_module_from_def_id(local_def_id).to_def_id();

            if containing_mod == mod_scope {
                if let Ok(visible_path) = res::locally_visible_def_path(self.tcx, def_id, current_scope) {
                    return visible_path;
                }
            }
        }

        let mut visible_paths = res::visible_def_paths(self.tcx, def_id, self.current_scope, ignore_reexport);
        if let Some(visible_path) = visible_paths.drain(..).next() { return visible_path; }

        // Ensure that the def is in the current scope, otherwise it really is not visible from here.
        let Some(current_scope) = self.current_scope else {
            panic!("{def_id:?} is not accessible in this crate at {span:?}");
        };
        match res::locally_visible_def_path(self.tcx, def_id, current_scope) {
            Ok(visible_path) => { return visible_path; }
            Err(adjusted_scope) => {
                panic!("{def_id:?} is not defined in the scope {adjusted_scope:?} and is not otherwise accessible at {span:?}");
            }
        }
    }

    fn adjust_path_from_expansion(&self, path: &mut ast::Path, res: hir::Res<ast::NodeId>, ignore_reexport: Option<hir::DefId>) {
        match res {
            hir::Res::Local(node_id) => {
                let Some(hir_id) = self.body_res.hir_id(node_id) else { return; };
                let def_ident = self.tcx.hir().ident(hir_id);

                let Some(last_segment) = path.segments.last_mut() else { unreachable!() };
                copy_def_span_ctxt(&mut last_segment.ident, def_ident.span);
                sanitize_ident_if_from_expansion(&mut last_segment.ident, IdentResKind::Local);
            }

            hir::Res::Def(def_kind, mut def_id) => {
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
                    | hir::DefKind::Macro(..)
                    => {
                        if let hir::DefKind::Ctor(..) = def_kind {
                            // Adjust target definition to the parent to avoid naming the unnamed constructors.
                            def_id = self.tcx.parent(def_id);
                        }

                        let mut impl_parents = res::parent_iter(self.tcx, def_id).enumerate().filter(|(_, def_id)| matches!(self.tcx.def_kind(def_id), hir::DefKind::Impl { of_trait: _ }));
                        match impl_parents.next() {
                            // `..::{impl#?}::$assoc_item::..` path.
                            Some((1.., impl_parent_def_id)) => {
                                let impl_subject = self.tcx.impl_subject(impl_parent_def_id);
                                if let ty::ImplSubject::Inherent(ty) = impl_subject.instantiate_identity() {
                                    if let ty::TyKind::Slice(_) | ty::TyKind::Array(_, _) = ty.kind() {
                                        unreachable!("encountered def path with impl at unexpected position: {def_id:?}")
                                    }
                                }
                            }
                            // `..::{impl#?}::$assoc_item` path.
                            Some((0, impl_parent_def_id)) => {
                                let impl_subject = self.tcx.impl_subject(impl_parent_def_id);
                                if let ty::ImplSubject::Inherent(ty) = impl_subject.instantiate_identity() {
                                    // `<[_]>::$assoc_item` path.
                                    // HACK: Paths to assoc items in inherent impls of slices and arrays use a special syntax based on qualified paths.
                                    //       This is handled here by only creating a path relative to the slice and array types respectively
                                    //       (esentially just naming the assoc item itself), and
                                    //       later in `sanitize_qualified_path` by appending the appropriate path qualification with the corresponding type.
                                    if let ty::TyKind::Slice(_) | ty::TyKind::Array(_, _) = ty.kind() {
                                        let Some(relative_def_path) = res::relative_def_path(self.tcx, def_id, impl_parent_def_id) else { unreachable!() };
                                        return self.overwrite_path_with_def_path(path, &relative_def_path);
                                    }
                                }
                            }
                            None => {}
                        }

                        let visible_def_path = self.expect_visible_def_path(def_id, path.span, ignore_reexport);
                        self.overwrite_path_with_def_path(path, &visible_def_path);
                    }

                    | hir::DefKind::TyParam
                    | hir::DefKind::LifetimeParam
                    | hir::DefKind::ConstParam
                    => {
                        let Some(last_segment) = path.segments.last_mut() else { unreachable!() };
                        sanitize_ident_if_from_expansion(&mut last_segment.ident, IdentResKind::Def);
                    }

                    hir::DefKind::Field => {
                        // TODO
                    }

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

    fn sanitize_path(&mut self, path: &mut ast::Path, res: hir::Res<ast::NodeId>, ignore_reexport: Option<hir::DefId>) {
        // NOTE: We explicitly only visit the generic arguments, as we will
        //       sanitize the ident segments afterwards.
        for segment in &mut path.segments {
            if let Some(args) = &mut segment.args {
                self.visit_generic_args(args);
            }
        }

        // Short-circuit if self.
        if let [path_segment] = &path.segments[..] && path_segment.ident.name == kw::SelfLower { return; }

        self.adjust_path_from_expansion(path, res, ignore_reexport);
    }

    fn typeck_for(&self, owner_id: hir::OwnerId) -> Option<&'tcx ty::TypeckResults<'tcx>> {
        self.current_typeck_ctx.filter(|typeck| typeck.hir_owner == owner_id)
    }

    fn lookup_hir_node_ty(&self, ty_hir: &hir::Ty<'tcx>) -> Ty<'tcx> {
        if let Some(typeck) = self.typeck_for(ty_hir.hir_id.owner) {
            if let Some(ty) = typeck.node_type_opt(ty_hir.hir_id) {
                return ty;
            }
        }

        let icx = ItemCtxt::new(self.tcx, ty_hir.hir_id.owner.def_id);
        icx.lower_ty(ty_hir)
    }

    fn sanitize_region(&self, region: ty::Region<'tcx>) -> Option<ast::Lifetime> {
        let span = match ty::region_opt_param_def_id(region) {
            Some(def_id) => self.tcx.def_ident_span(def_id).unwrap_or(DUMMY_SP),
            None => DUMMY_SP,
        };
        let mut ident = Ident::new(region.get_name()?, span);
        sanitize_ident_if_from_expansion(&mut ident, IdentResKind::Def);

        Some(ast::mk::lifetime(DUMMY_SP, ident))
    }

    fn sanitize_ty(&self, ty: Ty<'tcx>) -> P<ast::Ty> {
        let def_path_handling = ty::print::DefPathHandling::PreferVisible(ty::print::ScopedItemPaths::Trimmed);
        let opaque_ty_handling = ty::print::OpaqueTyHandling::Infer;
        let Some(ty_ast) = ty::ast_repr(self.tcx, self.def_res, self.current_scope, DUMMY_SP, ty, def_path_handling, opaque_ty_handling, true) else { unreachable!() };

        ty_ast
    }

    /// Extract bound params from local trait bounds corresponding to the parameter,
    /// which must be appended to the trait subpath if the parameter is in a qualified self position:
    /// `<T as Trait<'a, 'b>>::$assoc where T: Trait<'a, 'b>`.
    fn extract_local_trait_bound_params(&self, trait_def_id: hir::DefId, param_res: hir::Res<ast::NodeId>) -> Option<P<ast::GenericArgs>> {
        let (generic_predicates, param_index) = match param_res {
            hir::Res::SelfTyAlias { alias_to: impl_def_id, is_trait_impl: true, .. } => {
                let Some(trait_ref) = self.tcx.impl_trait_ref(impl_def_id) else { unreachable!() };

                // We instantiate the trait predicates with the impl's trait arguments
                // for correct resolution of the local bounds later
                // (e.g. if a lifetime is bound with a different name in the impl than in the trait).
                // NOTE: To easily find the `Self` predicates later using the 0 parameter index,
                //       we replace the concrete `Self` argument with the generic `Self` parameter type
                //       that appears in the trait def.
                let dummy_self_param_ty = ty::ParamTy::new(0, kw::SelfUpper).to_ty(self.tcx);
                let args = self.tcx.mk_args_trait(dummy_self_param_ty, trait_ref.skip_binder().args.iter().skip(1));
                let generic_predicates = self.tcx.predicates_of(trait_ref.skip_binder().def_id).instantiate(self.tcx, args);

                (generic_predicates, 0)
            }
            | hir::Res::SelfTyAlias { alias_to: trait_def_id, is_trait_impl: false, .. }
            | hir::Res::SelfTyParam { trait_: trait_def_id } => {
                let generic_predicates = self.tcx.predicates_of(trait_def_id).instantiate_identity(self.tcx);
                (generic_predicates, 0)
            }
            hir::Res::Def(hir::DefKind::TyParam, param_def_id) => {
                let generics = self.tcx.generics_of(self.tcx.parent(param_def_id));
                let Some(&param_index) = generics.param_def_id_to_index.get(&param_def_id) else { unreachable!() };
                let generic_predicates = self.tcx.predicates_of(self.tcx.parent(param_def_id)).instantiate_identity(self.tcx);
                (generic_predicates, param_index)
            }
            _ => { return None; }
        };

        let predicates = generic_predicates.predicates.iter()
            .filter_map(|&clause| clause.as_trait_clause().map(|p| p.skip_binder()))
            .filter(|trait_predicate| {
                let ty::TyKind::Param(param_ty) = trait_predicate.self_ty().kind() else { return false; };
                param_ty.index == param_index
            })
            .collect::<Vec<_>>();

        let Some(trait_predicate) = predicates.iter().find(|trait_predicate| trait_predicate.def_id() == trait_def_id) else {
            // No trait predicates for the trait, skip.
            return None;
        };

        let args_ast = trait_predicate.trait_ref.args[1..].iter()
            .filter_map(|generic_arg| {
                match generic_arg.unpack() {
                    ty::GenericArgKind::Lifetime(region) => {
                        let lifetime = self.sanitize_region(region)?;
                        Some(ast::AngleBracketedArg::Arg(ast::GenericArg::Lifetime(lifetime)))
                    }
                    ty::GenericArgKind::Type(ty) => {
                        let ty_ast = self.sanitize_ty(ty);
                        Some(ast::AngleBracketedArg::Arg(ast::GenericArg::Type(ty_ast)))
                    }
                    ty::GenericArgKind::Const(_) => {
                        None // TODO
                    }
                }
            })
            .collect::<ThinVec<_>>();

        if args_ast.is_empty() { return None; }

        Some(P(ast::GenericArgs::AngleBracketed(ast::AngleBracketedArgs { span: DUMMY_SP, args: args_ast })))
    }

    fn sanitize_qualified_path(&mut self, qself: &mut Option<P<ast::QSelf>>, path: &mut ast::Path, node_id: ast::NodeId) {
        // Short-circuit if not in a macro expansion.
        if !is_macro_expn_span(path.span) {
            // HACK: The non-macro path will not be adjusted by `sanitize_path`,
            //       so the resolution argument will not be used.
            self.sanitize_path(path, hir::Res::Err, None);
            return;
        }

        // Short-circuit if self.
        if let [path_segment] = &path.segments[..] && path_segment.ident.name == kw::SelfLower { return; }

        match (qself, self.def_res.node_res(node_id)) {
            // If the path can be resolved without type-checking, then it will be handled like in `visit_path`.
            (None, Some(res)) => self.sanitize_path(path, res, None),

            // Otherwise, resolve it with the help of the corresponding HIR QPath.
            (qself @ _, _) => {
                let Some(node_hir_id) = self.body_res.hir_id(node_id) else { return; };
                let Some(qpath_hir) = self.tcx.hir_node(node_hir_id).qpath() else { unreachable!() };

                match qpath_hir {
                    // NOTE: This corresponds to the already handled case where
                    //       a non-qualified path has a concrete AST resolution.
                    hir::QPath::Resolved(None, _) => unreachable!(),

                    | hir::QPath::Resolved(Some(qself_ty_hir), _)
                    | hir::QPath::TypeRelative(qself_ty_hir, _) => {
                        let mut qres = match self.typeck_for(node_hir_id.owner) {
                            Some(typeck) => typeck.qpath_res(&qpath_hir, node_hir_id),
                            None => {
                                match qpath_hir {
                                    hir::QPath::Resolved(_, path_hir) => path_hir.res,
                                    _ => hir::Res::Err,
                                }
                            }
                        };
                        if let hir::Res::Err = qres {
                            match self.tcx.hir_node(node_hir_id) {
                                hir::Node::Ty(ty_hir) => {
                                    let icx = ItemCtxt::new(self.tcx, node_hir_id.owner.def_id);
                                    let ty = icx.lower_ty(ty_hir);
                                    let ty::TyKind::Alias(ty::AliasTyKind::Projection, alias_ty) = ty.kind() else { unreachable!() };

                                    let trait_item_def_id = alias_ty.def_id;
                                    let trait_item_def_kind = self.tcx.def_kind(alias_ty.def_id);

                                    qres = hir::Res::Def(trait_item_def_kind, trait_item_def_id);
                                }

                                _ => panic!("BUG: path resolved to `Err`"),
                            }
                        }

                        let qself_ty = self.lookup_hir_node_ty(qself_ty_hir);

                        // NOTE: All nested qpaths can be reduced down to a simply qualified path by resolving the definition.
                        match self.tcx.def_kind(self.tcx.parent(qres.def_id())) {
                            hir::DefKind::Trait | hir::DefKind::TraitAlias => {
                                let qself_ty_ast = self.sanitize_ty(qself_ty);

                                let parent_path_segment_res = match &path.segments[..] {
                                    [.., parent_path_segment, _] => self.def_res.node_res(parent_path_segment.id),
                                    _ => None,
                                };

                                self.sanitize_path(path, qres.expect_non_local(), None);
                                // NOTE: There is no need to append bound params from local trait bounds corresponding to parameter types
                                //       if the path is already qualified, since
                                //       the necessary trait arguments should already be present in the corresponding path segment.
                                if qself.is_none() {
                                    if let Some(parent_path_segment_res) = parent_path_segment_res {
                                        // Append bound params from local trait bounds corresponding to parameter types to the trait subpath.
                                        if let Some(generic_args_ast) = self.extract_local_trait_bound_params(self.tcx.parent(qres.def_id()), parent_path_segment_res) {
                                            let [.., parent_path_segment, _] = &mut path.segments[..] else { unreachable!() };
                                            parent_path_segment.args = Some(generic_args_ast);
                                        }
                                    }
                                }

                                *qself = Some(P(ast::QSelf {
                                    ty: qself_ty_ast,
                                    path_span: DUMMY_SP,
                                    position: path.segments.len() - 1,
                                }));
                            }

                            // `<[_]>::$assoc_item` path.
                            // HACK: Paths to assoc items in inherent impls of slices and arrays use a special syntax based on qualified paths.
                            //       This is handled in `adjust_path_from_expansion` by only creating a path relative to the slice and array types respectively
                            //       (esentially just naming the assoc item itself), and
                            //       here by appending the appropriate path qualification with the corresponding type.
                            hir::DefKind::Impl { of_trait: false }
                                if let ty::ImplSubject::Inherent(ty) = self.tcx.impl_subject(self.tcx.parent(qres.def_id())).instantiate_identity()
                                && let ty::TyKind::Slice(_) | ty::TyKind::Array(_, _) = ty.kind()
                            => {
                                let qself_ty_ast = self.sanitize_ty(qself_ty);
                                self.sanitize_path(path, qres.expect_non_local(), None);
                                // NOTE: This syntax can only appear at the root of the path and the qualification cannot contain any part of the path.
                                *qself = Some(P(ast::QSelf {
                                    ty: qself_ty_ast,
                                    path_span: DUMMY_SP,
                                    position: 0,
                                }));
                            }

                            hir::DefKind::Impl { of_trait: false } => {
                                let qself_ty_ast = self.sanitize_ty(qself_ty);
                                self.sanitize_path(path, qres.expect_non_local(), None);

                                // Remove the non-assoc part of the path and replace it with
                                // a "dummy" qualified self using the type derived from the type system
                                // corresponding to the HIR self type, like so:
                                // `<$ty>::$assoc` (note the lack of the `as` qualifier).
                                path.segments.splice(0..(path.segments.len() - 1), []);
                                *qself = Some(P(ast::QSelf {
                                    ty: qself_ty_ast,
                                    path_span: DUMMY_SP,
                                    position: 0,
                                }));
                            }

                            // If the portion of the path within the qself does not refer to a trait or impl,
                            // then the resolved path no longer needs a qualified self.
                            _ => {
                                self.sanitize_path(path, qres.expect_non_local(), None);
                                *qself = None;
                            }
                        }
                    }

                    hir::QPath::LangItem(_, _) => unreachable!(),
                }
            }
        }
    }

    fn sanitize_import_path(&mut self, scope: hir::HirId, import_node_id: ast::NodeId, path: &mut ast::Path, res: hir::Res<ast::NodeId>) {
        let Some(&import_def_id) = self.def_res.node_id_to_def_id.get(&import_node_id) else { unreachable!() };

        let hir::Res::Def(def_kind, def_id) = res else { panic!("import path has non-def last segment") };
        // If the whole import path (besides any glob suffix) points to a module, then the resolution is much more simple,
        // and we can simply sanitize the whole path in one go.
        if let hir::DefKind::Mod = def_kind {
            self.sanitize_path(path, res, Some(import_def_id.to_def_id()));
            return;
        }

        let [mod_path_segments @ .., item_path_segment] = &mut path.segments[..] else { panic!("empty import path") };

        let mod_scope = self.tcx.parent_module(scope).to_def_id();
        let overlay_mod_scope = match self.macros_2_0_top_level_relative_path_res_hack {
            Macros2_0TopLevelRelativePathResHack::InTopLevelMacros2_0Scope { parent_module: macros_2_0_def_parent_mod } => Some(macros_2_0_def_parent_mod),
            _ => None,
        };

        let (parent_mod_def_id, referenced_mod_child) = match mod_path_segments {
            [.., parent_mod_path_segment] if let Some(parent_mod_res) = self.def_res.node_res(parent_mod_path_segment.id) => {
                let hir::Res::Def(hir::DefKind::Mod, parent_mod_def_id) = parent_mod_res else { panic!("import path has non-mod prefix segment") };
                let Some(referenced_mod_child) = res::lookup_mod_child(self.tcx, parent_mod_def_id, res.expect_non_local(), item_path_segment.ident.name) else { unreachable!() };
                (parent_mod_def_id, referenced_mod_child)
            }

            // `$crate::Item` paths.
            [dollar_crate_segment]  if dollar_crate_segment.ident.name == kw::DollarCrate => {
                let crate_num = dollar_crate_segment.ident.span.ctxt().outer_expn_data().macro_def_id.unwrap().krate;
                let Some(referenced_mod_child) = res::lookup_mod_child(self.tcx, crate_num.as_def_id(), res.expect_non_local(), item_path_segment.ident.name) else { unreachable!() };
                (crate_num.as_def_id(), referenced_mod_child)
            }

            // `crate::Item` paths.
            [crate_segment] if crate_segment.ident.name == kw::Crate => {
                let Some(referenced_mod_child) = res::lookup_mod_child(self.tcx, def_id.krate.as_def_id(), res.expect_non_local(), item_path_segment.ident.name) else { unreachable!() };
                (def_id.krate.as_def_id(), referenced_mod_child)
            }

            // `super{::super}*::Item` paths.
            [super_segments @ ..] if !super_segments.is_empty() && super_segments.iter().all(|segment| segment.ident.name == kw::Super) => {
                let mut parent_mod_def_id = overlay_mod_scope.unwrap_or(mod_scope);
                for _ in 0..super_segments.len() {
                    parent_mod_def_id = self.tcx.parent_module_from_def_id(parent_mod_def_id.expect_local()).to_def_id();
                }
                let Some(referenced_mod_child) = res::lookup_mod_child(self.tcx, parent_mod_def_id, res.expect_non_local(), item_path_segment.ident.name) else { unreachable!() };
                (parent_mod_def_id, referenced_mod_child)
            }

            // `{self::}?Item` paths.
            [] | [ast::PathSegment { ident: Ident { name: kw::SelfLower, .. }, .. }] => {
                let Some((parent_mod_def_id, referenced_mod_child)) = [overlay_mod_scope, Some(mod_scope)].into_iter().flatten().find_map(|scope| {
                    let referenced_mod_child = res::lookup_mod_child(self.tcx, scope, res.expect_non_local(), item_path_segment.ident.name)?;
                    Some((scope, referenced_mod_child))
                }) else { unreachable!() };

                (parent_mod_def_id, referenced_mod_child)
            }

            _ => unreachable!("unhandled import path root with missing parent mod resolutions: {}", ast::print::path_to_string(path)),
        };

        if def_id.is_local() {
            let mut def_ident = referenced_mod_child.ident;
            sanitize_ident_if_from_expansion(&mut def_ident, IdentResKind::Def);
            item_path_segment.ident = def_ident.with_span_pos(item_path_segment.ident.span);
        }

        if parent_mod_def_id == mod_scope {
            path.segments.splice(0..(path.segments.len() - 1), []);
            return;
        }

        let mut parent_mod_path = ast::Path { span: DUMMY_SP, segments: thin_vec![], tokens: None };
        self.sanitize_path(&mut parent_mod_path, hir::Res::Def(hir::DefKind::Mod, parent_mod_def_id), Some(import_def_id.to_def_id()));
        path.segments.splice(0..(path.segments.len() - 1), parent_mod_path.segments);
    }

    fn sanitize_use_tree(&mut self, use_tree: &mut ast::UseTree, node_id: ast::NodeId, span: Span) {
        #[derive(PartialEq, Eq)]
        enum UseKind {
            Glob,
            Single {
                /// Name originally introduced by this use import, before import path sanitization.
                /// Either the last path segment, or the name alias.
                ident: Ident,
                /// Namespace in which the name is introduced by this use import.
                namespace: hir::Namespace,
                rename: Option<Ident>,
            }
        }

        struct Import {
            res: hir::Res<ast::NodeId>,
            /// Path segments of the import item, assembled from the nested use tree prefixes that lead to this import item.
            path_segments: ThinVec<ast::PathSegment>,
            node_id: ast::NodeId,
            use_kind: UseKind,
        }

        fn extract_imports_from_use_tree<'tcx>(tcx: TyCtxt<'tcx>, def_res: &ast_lowering::DefResolutions, imports: &mut Vec<Import>, mut root_path_segments: ThinVec<ast::PathSegment>, use_tree: &ast::UseTree, node_id: ast::NodeId) {
            root_path_segments.extend(use_tree.prefix.segments.iter().cloned());
            let mut path_segments = root_path_segments;

            // NOTE: `*::self` paths are only valid when the self appears in a nested use tree, and
            //       refer to the same item as the path segment before.
            //       Since they complicate path segment resolution and printing of valid paths,
            //       it is easiest to just strip these no-op suffix segments here.
            if let [.., last_path_segment] = &path_segments[..] && last_path_segment.ident.name == kw::SelfLower {
                path_segments.pop();
            }

            match &use_tree.kind {
                ast::UseTreeKind::Glob => {
                    let Some(res) = def_res.node_res(node_id).or_else(|| {
                        let [.., last_prefix_segment] = &use_tree.prefix.segments[..] else { return None; };
                        def_res.node_res(last_prefix_segment.id)
                    }) else { unreachable!() };

                    imports.push(Import { res, path_segments, node_id, use_kind: UseKind::Glob });
                }

                ast::UseTreeKind::Simple(rename) => {
                    let Some(import_res) = def_res.import_res(node_id) else { unreachable!() };

                    let import_res_with_ns = [
                        (import_res.type_ns.map(|res| (hir::Namespace::TypeNS, res))),
                        (import_res.value_ns.map(|res| (hir::Namespace::ValueNS, res))),
                        (import_res.macro_ns.map(|res| (hir::Namespace::MacroNS, res))),
                    ].into_iter().flatten();

                    let ident = rename.unwrap_or_else(|| path_segments.last().unwrap().ident);
                    import_res_with_ns
                        .map(|(namespace, res)| Import { res, path_segments: path_segments.clone(), node_id, use_kind: UseKind::Single { ident, namespace, rename: *rename } })
                        .collect_into(imports);
                }

                ast::UseTreeKind::Nested { items, span: _ } => {
                    for (nested_use_tree, inner_node_id) in items {
                        extract_imports_from_use_tree(tcx, def_res, imports, path_segments.clone(), nested_use_tree, *inner_node_id);
                    }
                }
            }
        }

        let Some(&scope_def_id) = self.def_res.node_id_to_def_id.get(&node_id) else { unreachable!() };
        let scope = self.tcx.local_def_id_to_hir_id(scope_def_id);

        let mut imports = vec![];
        extract_imports_from_use_tree(self.tcx, self.def_res, &mut imports, thin_vec![], &use_tree, node_id);

        let import_paths = imports.into_iter()
            .map(|import| {
                let mut path = ast::Path { span: DUMMY_SP, segments: import.path_segments, tokens: None };
                self.sanitize_import_path(scope, import.node_id, &mut path, import.res);

                let mut use_kind = import.use_kind;
                if let UseKind::Single { ident, rename, .. } = &mut use_kind {
                    // Retain name introduced by the original import, but sanitize the name.
                    match rename {
                        Some(rename) => sanitize_ident_if_from_expansion(rename, IdentResKind::Def),
                        None => {
                            let [.., last_path_segment] = &path.segments[..] else { unreachable!() };
                            sanitize_ident_if_from_expansion(ident, IdentResKind::Def);
                            if last_path_segment.ident.name != ident.name {
                                *rename = Some(*ident);
                            }
                        }
                    }
                }

                (path, use_kind)
            })
            // Dedupe imports based on idents introduced into namespaces.
            .dedup_by(|a, b| {
                match (a, b) {
                    // NOTE: Dublicate glob imports of the same module are already invalid before sanitization.
                    ((_, UseKind::Glob), (_, UseKind::Glob)) => false,

                    ((path, UseKind::Single { ident, namespace, .. }), (next_path, UseKind::Single{ ident: next_ident, namespace: next_namespace, .. })) => {
                        #[inline]
                        fn matching_paths(path: &ast::Path, next_path: &ast::Path) -> bool {
                            path.segments.len() == next_path.segments.len()
                                && iter::zip(&path.segments, &next_path.segments).all(|(segment, next_segment)| segment.ident.name == next_segment.ident.name)
                        }

                        // NOTE: If we encounter the same name relating to two different namespaces,
                        //       then the paths must be different, otherwise
                        //       the two imports will refer to the same names in the same namespaces.
                        ident == next_ident && (namespace == next_namespace || matching_paths(path, next_path))
                    }

                    _ => false,
                }
            });

        // Convert the top-level use tree into a prefixless group of resolved import paths,
        // i.e. `use {crate::$path::$to::$item, $relative::$path::$to::$local::$item, ...}`.
        use_tree.prefix.segments = thin_vec![];
        let use_tree_items = import_paths
            .map(|(path, use_kind)| {
                let nested_use_tree = ast::UseTree {
                    prefix: path,
                    kind: match use_kind {
                        UseKind::Glob => ast::UseTreeKind::Glob,
                        UseKind::Single { rename, .. } => ast::UseTreeKind::Simple(rename),
                    },
                    span: DUMMY_SP,
                };

                (nested_use_tree, ast::DUMMY_NODE_ID)
            })
            .collect::<ThinVec<_>>();
        use_tree.kind = ast::UseTreeKind::Nested { items: use_tree_items, span };
    }
}

pub fn sanitize_path<'tcx>(tcx: TyCtxt<'tcx>, def_res: &ast_lowering::DefResolutions, scope: Option<hir::DefId>, path: &mut ast::Path, res: hir::Res<ast::NodeId>, descend_into_args: bool) {
    let mut sanitizer = MacroExpansionSanitizer {
        tcx,
        def_res,
        body_res: &ast_lowering::BodyResolutions::empty(tcx),
        syntax_extensions: vec![],
        current_scope: scope,
        current_typeck_ctx: None,
        macros_2_0_top_level_relative_path_res_hack: Macros2_0TopLevelRelativePathResHack::NotInMacros2_0Scope,
        protected_idents: Default::default(),
    };

    if descend_into_args {
        // NOTE: We explicitly only visit the generic arguments, as we will
        //       sanitize the ident segments afterwards.
        for segment in &mut path.segments {
            if let Some(args) = &mut segment.args {
                sanitizer.visit_generic_args(args);
            }
        }
    }

    sanitizer.adjust_path_from_expansion(path, res, None);
}

macro def_flat_map_item_fns(
    $(fn $ident:ident: $item_kind:ident |$self:ident, $item:ident| {
        $(check $check:block)?
    });+;
) {
    $(
        fn $ident(&mut $self, mut $item: P<ast::Item<ast::$item_kind>>) -> SmallVec<[P<ast::Item<ast::$item_kind>>; 1]> {
            // Skip generated items corresponding to compiler (and mutest-rs) internals.
            if $item.id == ast::DUMMY_NODE_ID || $item.span == DUMMY_SP { return smallvec![$item]; }

            $($check)?

            let Some(&def_id) = $self.def_res.node_id_to_def_id.get(&$item.id) else { unreachable!() };

            // Store new context scope.
            let previous_scope = mem::replace(&mut $self.current_scope, Some(def_id.to_def_id()));

            // Store new typeck scope, if needed.
            let previous_typeck_ctx = $self.current_typeck_ctx;
            let typeck_root_def_id = $self.tcx.typeck_root_def_id(def_id.to_def_id());
            let Some(typeck_root_local_def_id) = typeck_root_def_id.as_local() else { unreachable!() };
            if let Some(typeck_root_body_id) = $self.tcx.hir_node_by_def_id(typeck_root_local_def_id).body_id() {
                if !previous_typeck_ctx.is_some_and(|previous_typeck_ctx| typeck_root_def_id == previous_typeck_ctx.hir_owner.to_def_id()) {
                    $self.current_typeck_ctx = Some($self.tcx.typeck_body(typeck_root_body_id));
                }
            }

            // Match definition ident if this is an assoc item corresponding to a trait.
            if let Some(assoc_item) = $self.tcx.opt_associated_item(def_id.to_def_id()) {
                match assoc_item.container {
                    ty::AssocItemContainer::TraitContainer => {
                        // Trait items make new standalone definitions rather than referring to another definition,
                        // and so their ident does not have to be adjusted to another definition's.
                    }
                    ty::AssocItemContainer::ImplContainer => {
                        // Adjustment only needed if this assoc item is in a trait impl, not a bare impl.
                        if let Some(trait_item_def_id) = assoc_item.trait_item_def_id {
                            // HACK: Copy ident syntax context from trait item definition for correct sanitization later.
                            let Some(trait_item_ident_span) = $self.tcx.def_ident_span(trait_item_def_id) else { unreachable!() };
                            copy_def_span_ctxt(&mut $item.ident, trait_item_ident_span);
                        }
                    }
                }
            }

            let item = ast::mut_visit::noop_flat_map_item($item, $self);

            // Restore previous context.
            $self.current_typeck_ctx = previous_typeck_ctx;
            $self.current_scope = previous_scope;

            item
        }
    )+
}

impl<'tcx, 'op> ast::mut_visit::MutVisitor for MacroExpansionSanitizer<'tcx, 'op> {
    def_flat_map_item_fns! {
        fn flat_map_item: ItemKind |self, item| {
            check {
                let id = item.id;
                let name = item.ident.name;
                let span = item.span;

                let macros_2_0_ctxt = item.span.ctxt().normalize_to_macros_2_0();
                let macros_2_0_expn_id = macros_2_0_ctxt.outer_expn();
                let macros_2_0_expn = macros_2_0_ctxt.outer_expn_data();
                if let ExpnKind::Macro(MacroKind::Bang, _) = macros_2_0_expn.kind {
                    match (self.macros_2_0_top_level_relative_path_res_hack, &item.kind) {
                        (Macros2_0TopLevelRelativePathResHack::InNestedMacros2_0Scope(expn_id), _) if expn_id == macros_2_0_expn_id => {}
                        (_, ast::ItemKind::Mod(_, _)) => {
                            self.macros_2_0_top_level_relative_path_res_hack = Macros2_0TopLevelRelativePathResHack::InNestedMacros2_0Scope(macros_2_0_expn_id);
                        }
                        _ => {
                            self.macros_2_0_top_level_relative_path_res_hack = Macros2_0TopLevelRelativePathResHack::InTopLevelMacros2_0Scope {
                                parent_module: macros_2_0_expn.parent_module.unwrap(),
                            };
                        }
                    }
                }

                if let ast::ItemKind::ExternCrate(symbol) = &mut item.kind {
                    // Retain original crate name if not already aliased.
                    if symbol.is_none() { *symbol = Some(name); }

                    // Sanitize local name of extern.
                    sanitize_ident_if_from_expansion(&mut item.ident, IdentResKind::Def);

                    return smallvec![item];
                }

                if let ast::ItemKind::Use(use_tree) = &mut item.kind {
                    self.sanitize_use_tree(use_tree, id, span);
                    return smallvec![item];
                }
            }
        };

        fn flat_map_trait_item: AssocItemKind |self, item| {};
        fn flat_map_impl_item: AssocItemKind |self, item| {};
    }

    fn visit_attribute(&mut self, attr: &mut ast::Attribute) {
        // Some attributes correspond to already expanded derive proc macros, and thus need to be removed.
        if is_macro_helper_attr(&self.syntax_extensions, attr) {
            // Disable attribute by overriding it with an empty doc-comment.
            // This is easier than modifying every visit function to properly remove the attribute nodes.
            attr.kind = ast::AttrKind::DocComment(ast::token::CommentKind::Line, kw::Empty)
        }
    }

    fn visit_ty(&mut self, ty: &mut P<ast::Ty>) {
        // HACK: Even though NodeId is a Copy type, the copy itself
        //       counts as an immutable borrow, so we have to do it
        //       before `&mut ty.kind`.
        let ty_id = ty.id;

        match &mut ty.kind {
            ast::TyKind::Path(qself, path) => {
                return self.sanitize_qualified_path(qself, path, ty_id);
            }
            _ => {}
        }

        ast::mut_visit::noop_visit_ty(ty, self);
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

        // HACK: Even though NodeId is a Copy type, the copy itself
        //       counts as an immutable borrow, so we have to do it
        //       before `&mut expr.kind`.
        let expr_id = expr.id;

        match &mut expr.kind {
            ast::ExprKind::Path(qself, path) => {
                return self.sanitize_qualified_path(qself, path, expr_id);
            }
            ast::ExprKind::Struct(struct_expr) => 'arm: {
                let Some(res) = self.def_res.node_res(struct_expr.path.segments.last().unwrap().id) else { break 'arm; };

                let variant_def = match res {
                    | hir::Res::SelfTyAlias { alias_to: alias_def_id, .. }
                    | hir::Res::Def(hir::DefKind::TyAlias, alias_def_id) => {
                        let self_ty = self.tcx.type_of(alias_def_id).instantiate_identity();
                        let ty::TyKind::Adt(adt_def, _) = self_ty.kind() else { unreachable!() };
                        adt_def.variant_of_res(res.expect_non_local())
                    }
                    // Expect a struct, union, or enum variant, and get the corresponding ADT variant.
                    _ => self.tcx.expect_variant_res(res.expect_non_local()),
                };

                for field in &mut struct_expr.fields {
                    let Some(field_def) = variant_def.fields.iter().find(|field_def| self.tcx.hygienic_eq(field.ident, field_def.ident(self.tcx), variant_def.def_id)) else {
                        panic!("field {ident} at {span:?} does not match any field of {variant_def_id:?}",
                            ident = field.ident,
                            span = field.span,
                            variant_def_id = variant_def.def_id,
                        );
                    };
                    // HACK: Copy ident syntax context from definition for correct sanitization later.
                    copy_def_span_ctxt(&mut field.ident, field_def.ident(self.tcx).span);
                    // NOTE: We have to disable shorthand syntax to ensure that
                    //       the correct field ident appears in printed code.
                    field.is_shorthand = false;
                }
            }
            ast::ExprKind::Field(base_expr, field_ident) => 'arm: {
                // Idents cannot start with a digit, therefore they must correspond
                // to an unnamed field reference which must not be sanitized.
                if field_ident.name.as_str().starts_with(|c: char| c.is_ascii_digit()) {
                    protected_ident = Some(*field_ident);
                    break 'arm;
                }

                let Some(base_expr_hir) = self.body_res.hir_expr(&base_expr) else { break 'arm; };
                // HACK: The borrow checker does not allow for immutably referencing
                //       the expression for the `hir_expr` call because of
                //       the `&mut expr.kind` partial borrow above.
                let Some(expr_hir) = self.body_res.hir_node(expr_id).map(|hir_node| hir_node.expect_expr()) else { break 'arm; };

                let Some(typeck) = self.typeck_for(expr_hir.hir_id.owner) else { break 'arm; };

                let field_idx = typeck.field_index(expr_hir.hir_id);

                let base_ty = typeck.expr_ty_adjusted(base_expr_hir);
                let ty::TyKind::Adt(adt_def, _) = base_ty.kind() else { unreachable!() };
                let field_def = &adt_def.non_enum_variant().fields[field_idx];

                // HACK: Copy ident syntax context from definition for correct sanitization later.
                copy_def_span_ctxt(field_ident, field_def.ident(self.tcx).span);
            }
            ast::ExprKind::MethodCall(call) => 'arm: {
                // HACK: The borrow checker does not allow for immutably referencing
                //       the expression for the `hir_expr` call because of
                //       the `&mut expr.kind` partial borrow above.
                let Some(expr_hir) = self.body_res.hir_node(expr_id).map(|hir_node| hir_node.expect_expr()) else { break 'arm; };

                let Some(typeck) = self.typeck_for(expr_hir.hir_id.owner) else { break 'arm; };

                let Some(call_def_id) = typeck.type_dependent_def_id(expr_hir.hir_id) else { unreachable!() };

                // HACK: Copy ident syntax context from definition for correct sanitization later.
                let Some(call_item_ident_span) = self.tcx.def_ident_span(call_def_id) else { unreachable!() };
                copy_def_span_ctxt(&mut call.seg.ident, call_item_ident_span);
            }
            _ => {}
        }

        if let Some(protected_ident) = protected_ident { self.protected_idents.insert(protected_ident); }
        ast::mut_visit::noop_visit_expr(expr, self);
        if let Some(protected_ident) = protected_ident { self.protected_idents.remove(&protected_ident); }
    }

    fn visit_pat(&mut self, pat: &mut P<ast::Pat>) {
        let mut protected_ident = None;

        // HACK: Even though NodeId is a Copy type, the copy itself
        //       counts as an immutable borrow, so we have to do it
        //       before `&mut pat.kind`.
        let pat_id = pat.id;

        match &mut pat.kind {
            ast::PatKind::Ident(_, ident, _) => {
                if let Some(hir::Res::Local(..)) = self.def_res.node_res(pat_id) {
                    sanitize_standalone_ident_if_from_expansion(ident, IdentResKind::Local);
                    protected_ident = Some(*ident);
                }
            }
            ast::PatKind::Path(qself, path) => {
                return self.sanitize_qualified_path(qself, path, pat_id);
            }
            ast::PatKind::Struct(_, path, fields, _) => 'arm: {
                let Some(res) = self.def_res.node_res(path.segments.last().unwrap().id) else { break 'arm; };

                let variant_def = match res {
                    | hir::Res::SelfTyAlias { alias_to: alias_def_id, .. }
                    | hir::Res::Def(hir::DefKind::TyAlias, alias_def_id) => {
                        let self_ty = self.tcx.type_of(alias_def_id).instantiate_identity();
                        let ty::TyKind::Adt(adt_def, _) = self_ty.kind() else { unreachable!() };
                        adt_def.variant_of_res(res.expect_non_local())
                    }
                    // Expect a struct, union, or enum variant, and get the corresponding ADT variant.
                    _ => self.tcx.expect_variant_res(res.expect_non_local()),
                };

                for field in fields {
                    let Some(field_def) = variant_def.fields.iter().find(|field_def| self.tcx.hygienic_eq(field.ident, field_def.ident(self.tcx), variant_def.def_id)) else {
                        panic!("field {ident} at {span:?} does not match any field of {variant_def_id:?}",
                            ident = field.ident,
                            span = field.span,
                            variant_def_id = variant_def.def_id,
                        );
                    };
                    // HACK: Copy ident syntax context from definition for correct sanitization later.
                    copy_def_span_ctxt(&mut field.ident, field_def.ident(self.tcx).span);
                    // NOTE: We have to disable shorthand syntax to ensure that
                    //       the correct field ident appears in printed code.
                    field.is_shorthand = false;
                }
            }
            _ => {}
        }

        if let Some(protected_ident) = protected_ident { self.protected_idents.insert(protected_ident); }
        ast::mut_visit::noop_visit_pat(pat, self);
        if let Some(protected_ident) = protected_ident { self.protected_idents.remove(&protected_ident); }
    }

    fn visit_path(&mut self, path: &mut ast::Path) {
        let Some(last_segment) = path.segments.last() else { return; };
        let Some(res) = self.def_res.node_res(last_segment.id) else { return; };
        self.sanitize_path(path, res, None);
    }

    fn visit_label(&mut self, label: &mut ast::Label) {
        sanitize_standalone_ident_if_from_expansion(&mut label.ident, IdentResKind::Label);
    }

    fn visit_ident(&mut self, ident: &mut Ident) {
        if self.protected_idents.contains(ident) { return; }
        sanitize_standalone_ident_if_from_expansion(ident, IdentResKind::Def);
    }

    fn visit_vis(&mut self, vis: &mut ast::Visibility) {
        // Short-circuit if not in a macro expansion, as there is no
        // other child node which could be from a macro expansion.
        if !is_macro_expn_span(vis.span) { return; }

        match &mut vis.kind {
            ast::VisibilityKind::Restricted { path, id, .. } => {
                let Some(res) = self.def_res.node_res(*id) else { return; };
                self.adjust_path_from_expansion(path, res, None);
            }
            _ => ast::mut_visit::noop_visit_vis(vis, self),
        }
    }
}

fn register_builtin_macros(syntax_extensions: &mut Vec<SyntaxExtension>) {
    fn dummy_syntax_extension(name: Symbol, helper_attrs: Vec<Symbol>, allow_internal_unstable: Option<Lrc<[Symbol]>>) -> SyntaxExtension {
        SyntaxExtension {
            kind: SyntaxExtensionKind::NonMacroAttr,
            span: DUMMY_SP,
            allow_internal_unstable,
            stability: None,
            deprecation: None,
            helper_attrs,
            edition: Edition::Edition2021,
            builtin_name: Some(name),
            allow_internal_unsafe: false,
            local_inner_macros: false,
            collapse_debuginfo: true,
        }
    }

    syntax_extensions.extend([
        dummy_syntax_extension(sym::Default, vec![kw::Default], None), // #[derive(Default)]
    ]);
}

pub fn sanitize_macro_expansions<'tcx>(tcx: TyCtxt<'tcx>, def_res: &ast_lowering::DefResolutions, body_res: &ast_lowering::BodyResolutions<'tcx>, krate: &mut ast::Crate) {
    let mut syntax_extensions = vec![];
    register_builtin_macros(&mut syntax_extensions);

    let cstore = CStore::from_tcx(tcx);

    // HACK: The only way to enumerate the definitions of an external crate though the public API is
    //       by enumerating the def indices up to `CStore::num_def_ids_untracked`.
    //       See https://github.com/rust-lang/rust/pull/85889.
    let crate_def_indices = |cnum: hir::CrateNum| {
        const BASE_DEF_IDX: usize = hir::CRATE_DEF_INDEX.as_usize();
        let max_def_idx = cstore.num_def_ids_untracked(cnum);

        (BASE_DEF_IDX..max_def_idx).map(hir::DefIndex::from_usize)
    };

    // Find all loaded proc macro syntax extensions.
    tcx.crates(()).iter()
        // Ensure that we are only looking at crates with `CrateKind::ProcMacro`.
        .filter(|&&cnum| tcx.dep_kind(cnum).macros_only())
        .flat_map(|&cnum| crate_def_indices(cnum).map(move |index| hir::DefId { krate: cnum, index }))
        .filter(|&def_id| {
            // HACK: Not all def indices are encoded for proc macro crates, only the macro definitions themselves.
            //       To avoid crashes due to missing def index lookups, we must only generate "valid" indices,
            //       however, there is no stable mechanism for this.
            //       Instead, we use a def kind query to trigger a crate metadata lookup, and
            //       catch the panic, if it occurs.
            //       We also disable printing of panic messages with an empty hook.
            //       See https://github.com/rust-lang/rust/pull/76897.
            let builtin_panic_hook = panic::take_hook();
            panic::set_hook(Box::new(|_panic_info| {}));
            let Ok(def_kind) = panic::catch_unwind(panic::AssertUnwindSafe(|| cstore.def_kind_untracked(def_id))) else { return false; };
            panic::set_hook(builtin_panic_hook);

            matches!(def_kind, hir::DefKind::Macro(_))
        })
        .filter_map(|def_id| {
            match cstore.load_macro_untracked(def_id, tcx) {
                LoadedMacro::ProcMacro(syntax_extension) => Some(syntax_extension),
                // TODO: Generate syntax extensions from regular macros.
                LoadedMacro::MacroDef(..) => None,
            }
        })
        .collect_into(&mut syntax_extensions);

    let mut sanitizer = MacroExpansionSanitizer {
        tcx,
        def_res,
        body_res,
        syntax_extensions,
        current_scope: Some(LOCAL_CRATE.as_def_id()),
        current_typeck_ctx: None,
        macros_2_0_top_level_relative_path_res_hack: Macros2_0TopLevelRelativePathResHack::NotInMacros2_0Scope,
        protected_idents: Default::default(),
    };
    sanitizer.visit_crate(krate);
}
