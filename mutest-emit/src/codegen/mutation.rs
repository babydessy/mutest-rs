use std::hash::{Hash, Hasher};

use rustc_ast_lowering::ResolverAstLowering;
use rustc_expand::base::ResolverExpand;
use rustc_hash::{FxHashSet, FxHashMap};
use rustc_middle::hir::nested_filter::OnlyBodies;
use rustc_resolve::Resolver;
use rustc_session::Session;
use smallvec::SmallVec;

use crate::analysis::ast_lowering::{self, AstDefItem};
use crate::analysis::diagnostic;
use crate::analysis::diagnostic::SessionRcSourceMap;
use crate::analysis::hir;
use crate::analysis::res;
use crate::analysis::tests::Test;
use crate::analysis::ty::{self, TyCtxt};
use crate::codegen::ast;
use crate::codegen::ast::P;
use crate::codegen::ast::visit::Visitor;
use crate::codegen::substitution::conflicting_substs;
use crate::codegen::symbols::{DUMMY_SP, Ident, Span, Symbol, sym};
use crate::codegen::symbols::hygiene::AstPass;

#[derive(Clone)]
pub struct Lowered<A, H> {
    pub ast: A,
    pub hir: H,
}

pub type LoweredFn<'ast, 'hir> = Lowered<ast::InlinedFn<'ast>, hir::InlinedFn<'hir>>;
pub type OwnedLoweredFn<'hir> = Lowered<ast::OwnedInlinedFn, hir::InlinedFn<'hir>>;
pub type LoweredParam<'ast, 'hir> = Lowered<&'ast ast::Param, &'hir hir::Param<'hir>>;
pub type OwnedLoweredParam<'hir> = Lowered<ast::Param, &'hir hir::Param<'hir>>;
pub type LoweredStmt<'ast, 'hir> = Lowered<&'ast ast::Stmt, &'hir hir::Stmt<'hir>>;
pub type OwnedLoweredStmt<'hir> = Lowered<ast::Stmt, &'hir hir::Stmt<'hir>>;
pub type LoweredExpr<'ast, 'hir> = Lowered<&'ast ast::Expr, &'hir hir::Expr<'hir>>;
pub type OwnedLoweredExpr<'hir> = Lowered<ast::Expr, &'hir hir::Expr<'hir>>;

pub enum MutLoc<'ast, 'hir> {
    Fn(LoweredFn<'ast, 'hir>),
    FnParam(LoweredParam<'ast, 'hir>, LoweredFn<'ast, 'hir>),
    FnBodyStmt(LoweredStmt<'ast, 'hir>, LoweredFn<'ast, 'hir>),
    FnBodyExpr(LoweredExpr<'ast, 'hir>, LoweredFn<'ast, 'hir>),
}

pub enum OwnedMutLoc<'hir> {
    Fn(OwnedLoweredFn<'hir>),
    FnParam(OwnedLoweredParam<'hir>, OwnedLoweredFn<'hir>),
    FnBodyStmt(OwnedLoweredStmt<'hir>, OwnedLoweredFn<'hir>),
    FnBodyExpr(OwnedLoweredExpr<'hir>, OwnedLoweredFn<'hir>),
}

impl<'ast, 'hir> MutLoc<'ast, 'hir> {
    pub fn into_owned(&self) -> OwnedMutLoc<'hir> {
        match self {
            Self::Fn(Lowered { ast: fn_ast, hir: fn_hir }) => {
                OwnedMutLoc::Fn(Lowered { ast: fn_ast.into_owned(), hir: *fn_hir })
            }
            Self::FnParam(Lowered { ast: param_ast, hir: param_hir }, Lowered { ast: fn_ast, hir: fn_hir }) => {
                OwnedMutLoc::FnParam(Lowered { ast: (*param_ast).clone(), hir: param_hir }, Lowered { ast: fn_ast.into_owned(), hir: *fn_hir })
            }
            Self::FnBodyStmt(Lowered { ast: stmt_ast, hir: stmt_hir }, Lowered { ast: fn_ast, hir: fn_hir }) => {
                OwnedMutLoc::FnBodyStmt(Lowered { ast: (*stmt_ast).clone(), hir: stmt_hir }, Lowered { ast: fn_ast.into_owned(), hir: *fn_hir })
            }
            Self::FnBodyExpr(Lowered { ast: expr_ast, hir: expr_hir }, Lowered { ast: fn_ast, hir: fn_hir }) => {
                OwnedMutLoc::FnBodyExpr(Lowered { ast: (*expr_ast).clone(), hir: expr_hir }, Lowered { ast: fn_ast.into_owned(), hir: *fn_hir })
            }
        }
    }
}

impl<'hir> OwnedMutLoc<'hir> {
    pub fn span(&self) -> Span {
        match self {
            Self::Fn(lowered_fn) => lowered_fn.ast.span,
            Self::FnParam(lowered_param, _) => lowered_param.ast.span,
            Self::FnBodyStmt(lowered_stmt, _) => lowered_stmt.ast.span,
            Self::FnBodyExpr(lowered_expr, _) => lowered_expr.ast.span,
        }
    }

    pub fn containing_fn(&self) -> Option<&OwnedLoweredFn> {
        match self {
            Self::Fn(lowered_fn) => Some(lowered_fn),
            Self::FnParam(_, lowered_fn) => Some(lowered_fn),
            Self::FnBodyStmt(_, lowered_fn) => Some(lowered_fn),
            Self::FnBodyExpr(_, lowered_fn) => Some(lowered_fn),
        }
    }
}

pub struct MutCtxt<'op, 'ast, 'tcx, 'r> {
    pub tcx: TyCtxt<'tcx>,
    pub resolver: &'op Resolver<'r>,
    pub def_site: Span,
    pub location: MutLoc<'ast, 'tcx>,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum SubstLoc {
    InsertBefore(ast::NodeId),
    InsertAfter(ast::NodeId),
    Replace(ast::NodeId),
}

pub enum Subst {
    AstExpr(ast::Expr),
    AstStmt(ast::Stmt),
    AstLocal(Ident, ast::Mutability, Option<P<ast::Ty>>, P<ast::Expr>, Option<P<ast::Expr>>),
}

pub struct SubstDef {
    pub location: SubstLoc,
    pub substitute: Subst,
}

impl SubstDef {
    pub fn new(location: SubstLoc, substitute: Subst) -> Self {
        Self { location, substitute }
    }
}

pub trait Mutation {
    fn display_name(&self) -> String;
}

pub trait Operator<'a>: Send + Sync {
    type Mutation: Mutation + 'a;

    fn try_apply(&self, mcx: &MutCtxt) -> Option<(Self::Mutation, SmallVec<[SubstDef; 1]>)>;
}

pub trait OperatorBoxed<'a>: Send + Sync {
    type Mutation: Mutation + ?Sized + 'a;

    fn try_apply_boxed(&self, mcx: &MutCtxt) -> Option<(Box<Self::Mutation>, SmallVec<[SubstDef; 1]>)>;
}

impl<'a, T: Operator<'a>> OperatorBoxed<'a> for T {
    type Mutation = dyn Mutation + 'a;

    fn try_apply_boxed(&self, mcx: &MutCtxt) -> Option<(Box<Self::Mutation>, SmallVec<[SubstDef; 1]>)>  {
        match self.try_apply(mcx) {
            Some((mutation, substs)) => Some((Box::new(mutation), substs)),
            None => None,
        }
    }
}

pub type Operators<'op, 'm> = &'op [&'op dyn OperatorBoxed<'m, Mutation = dyn Mutation + 'm>];
pub type BoxedMutation<'m> = Box<dyn Mutation + 'm>;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub struct MutId(u32);

impl MutId {
    pub fn index(&self) -> u32 {
        self.0
    }

    pub fn into_symbol_name(&self) -> String {
        format!("mut_{}", self.0)
    }

    pub fn into_symbol(&self) -> Symbol {
        Symbol::intern(&self.into_symbol_name())
    }
}

pub struct Mut<'m, 'hir> {
    pub id: MutId,
    pub location: OwnedMutLoc<'hir>,
    pub mutation: BoxedMutation<'m>,
    pub substs: SmallVec<[SubstDef; 1]>,
}

impl<'m, 'hir> Mut<'m, 'hir> {
    pub fn display_name(&self) -> String {
        self.mutation.display_name()
    }

    pub fn display_location(&self, sess: &Session) -> String {
        sess.source_map().span_to_embeddable_string(self.location.span())
    }
}

impl<'m, 'hir> Eq for Mut<'m, 'hir> {}
impl<'m, 'hir> PartialEq for Mut<'m, 'hir> {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl<'m, 'hir> Hash for Mut<'m, 'hir> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

struct MutationCollector<'ast, 'tcx, 'r, 'op, 'm> {
    pub operators: &'op Operators<'op, 'm>,
    pub tcx: TyCtxt<'tcx>,
    pub resolver: &'op Resolver<'r>,
    pub def_site: Span,
    pub current_fn: Option<LoweredFn<'ast, 'tcx>>,
    pub next_mut_index: u32,
    pub mutations: Vec<Mut<'m, 'tcx>>,
}

/// Macro used during mutation collection to apply every mutation operator using the given mutation
/// context.
///
/// This is a macro because the equivalent associated function would require a mutable borrow of
/// the entire `self` struct, which would disallow certain patterns used in the AST visitor.
macro register_mutations($self:ident, $($mcx:tt)+) {
    {
        let mcx = $($mcx)+;

        for operator in *$self.operators {
            if let Some((mutation, substs)) = operator.try_apply_boxed(&mcx) {
                $self.mutations.push(Mut {
                    id: MutId($self.next_mut_index),
                    location: mcx.location.into_owned(),
                    mutation,
                    substs,
                });

                $self.next_mut_index += 1;
            }
        }
    }
}

impl<'ast, 'tcx, 'r, 'op, 'm> ast::visit::Visitor<'ast> for MutationCollector<'ast, 'tcx, 'r, 'op, 'm> {
    fn visit_fn(&mut self, kind: ast::visit::FnKind<'ast>, span: Span, id: ast::NodeId) {
        let ast::visit::FnKind::Fn(ref ctx, ref ident, sig, vis, generics, body) = kind else { return; };

        let fn_ast = ast::InlinedFn { id, span, ctx: *ctx, ident: *ident, vis, generics, sig, body };

        let fn_hir = match self.tcx.hir().get_by_def_id(self.resolver.local_def_id(fn_ast.id)) {
            hir::Node::Item(&hir::Item { def_id, span, vis_span, ident, ref kind }) => {
                let hir::ItemKind::Fn(sig, generics, body) = kind else { unreachable!(); };
                let body = self.tcx.hir().body(*body);
                let fn_kind = hir::intravisit::FnKind::ItemFn(ident, generics, sig.header);
                hir::InlinedFn { def_id, span, ident, kind: fn_kind, vis_span, sig, generics, body }
            }
            hir::Node::ImplItem(&hir::ImplItem { def_id, span, vis_span, ident, ref generics, ref kind }) => {
                let hir::ImplItemKind::Fn(sig, body) = kind else { unreachable!(); };
                let body = self.tcx.hir().body(*body);
                let fn_kind = hir::intravisit::FnKind::Method(ident, sig);
                hir::InlinedFn { def_id, span, ident, kind: fn_kind, vis_span, sig, generics, body }
            }
            _ => unreachable!(),
        };

        let lowered_fn = Lowered { ast: fn_ast, hir: fn_hir };

        register_mutations!(self, MutCtxt {
            tcx: self.tcx,
            resolver: self.resolver,
            def_site: self.def_site,
            location: MutLoc::Fn(lowered_fn.clone()),
        });

        let kind_ast = kind;
        let span_ast = lowered_fn.ast.span;
        let id_ast = lowered_fn.ast.id;
        let kind_hir = lowered_fn.hir.kind;
        let decl_hir = lowered_fn.hir.sig.decl;
        let body_hir = lowered_fn.hir.body.id();
        let span_hir = lowered_fn.hir.span;
        let id_hir = self.tcx.hir().local_def_id_to_hir_id(lowered_fn.hir.def_id);
        self.current_fn = Some(lowered_fn);
        ast_lowering::visit::AstHirVisitor::visit_fn(self, kind_ast, span_ast, id_ast, kind_hir, decl_hir, body_hir, span_hir, id_hir);
        self.current_fn = None;
    }
}

impl<'ast, 'hir, 'r, 'op, 'm> ast_lowering::visit::AstHirVisitor<'ast, 'hir> for MutationCollector<'ast, 'hir, 'r, 'op, 'm> {
    type NestedFilter = OnlyBodies;

    fn nested_visit_map(&mut self) -> Self::Map {
        self.tcx.hir()
    }

    fn visit_param(&mut self, param_ast: &'ast ast::Param, param_hir: &'hir hir::Param<'hir>) {
        if let Some(lowered_fn) = &self.current_fn {
            let lowered_param = Lowered { ast: param_ast, hir: param_hir };

            register_mutations!(self, MutCtxt {
                tcx: self.tcx,
                resolver: self.resolver,
                def_site: self.def_site,
                location: MutLoc::FnParam(lowered_param, lowered_fn.clone()),
            });
        }

        ast_lowering::visit::walk_param(self, param_ast, param_hir);
    }

    fn visit_stmt(&mut self, stmt_ast: &'ast ast::Stmt, stmt_hir: &'hir hir::Stmt<'hir>) {
        if let Some(lowered_fn) = &self.current_fn {
            let lowered_stmt = Lowered { ast: stmt_ast, hir: stmt_hir };

            register_mutations!(self, MutCtxt {
                tcx: self.tcx,
                resolver: self.resolver,
                def_site: self.def_site,
                location: MutLoc::FnBodyStmt(lowered_stmt, lowered_fn.clone()),
            });
        }

        ast_lowering::visit::walk_stmt(self, stmt_ast, stmt_hir);
    }

    fn visit_expr(&mut self, expr_ast: &'ast ast::Expr, expr_hir: &'hir hir::Expr<'hir>) {
        if let Some(lowered_fn) = &self.current_fn {
            let lowered_expr = Lowered { ast: expr_ast, hir: expr_hir };

            register_mutations!(self, MutCtxt {
                tcx: self.tcx,
                resolver: self.resolver,
                def_site: self.def_site,
                location: MutLoc::FnBodyExpr(lowered_expr, lowered_fn.clone()),
            });
        }

        ast_lowering::visit::walk_expr(self, expr_ast, expr_hir);
    }
}

pub struct Target<'ast, 'tst> {
    pub def_id: hir::LocalDefId,
    pub item: AstDefItem<'ast>,
    pub reachable_from: Vec<(&'tst Test, usize)>,
    pub distance: usize,
}

pub fn reachable_fns<'tcx, 'ast, 'tst>(tcx: TyCtxt<'tcx>, resolver: &mut Resolver, krate: &'ast ast::Crate, tests: &'tst [Test], depth: usize) -> Vec<Target<'ast, 'tst>> {
    let mut previously_found_callees: FxHashMap<(hir::LocalDefId, Option<ty::SubstsRef<'tcx>>), Vec<&'tst Test>> = Default::default();

    for test in tests {
        let Some(def_id) = resolver.opt_local_def_id(test.item.id) else { continue; };
        let body = tcx.hir().body(tcx.hir().get_by_def_id(def_id).body_id().unwrap());

        let mut callees = FxHashSet::from_iter(res::collect_callees(tcx, body));

        for (callee, substs) in callees.drain() {
            let Some(callee_def_id) = callee.as_local() else { continue; };

            previously_found_callees.entry((callee_def_id, substs))
                .and_modify(|reachable_from| reachable_from.push(test))
                .or_insert_with(|| vec![test]);
        }
    }

    let mut targets: FxHashMap<hir::LocalDefId, Target> = Default::default();

    for distance in 0..depth {
        let mut newly_found_callees: FxHashMap<(hir::LocalDefId, Option<ty::SubstsRef<'tcx>>), Vec<&'tst Test>> = Default::default();

        for ((callee_def_id, outer_substs), reachable_from) in previously_found_callees.drain() {
            let Some(body_id) = tcx.hir().get_by_def_id(callee_def_id).body_id() else { continue; };
            let body = tcx.hir().body(body_id);

            let Some(callee_def_item) = ast_lowering::find_def_in_ast(tcx, callee_def_id, krate) else { continue; };

            targets.entry(callee_def_id)
                .and_modify(|target| {
                    for &test in &reachable_from {
                        if target.reachable_from.iter().any(|&(t, _)| t == test) { continue; }
                        target.reachable_from.push((test, distance));
                    }
                })
                .or_insert_with(|| Target {
                    def_id: callee_def_id,
                    item: callee_def_item,
                    reachable_from: reachable_from.iter().map(|&test| (test, distance)).collect(),
                    distance,
                });

            if distance < depth {
                let mut callees = FxHashSet::from_iter(res::collect_callees(tcx, body));

                for (callee, substs) in callees.drain() {
                    let Some(callee_def_id) = callee.as_local() else { continue; };

                    let param_env = tcx.param_env(callee);
                    let instance = substs.or(outer_substs).and_then(|substs| tcx.resolve_instance(param_env.and((callee, substs))).ok().flatten());

                    let callee_def_id = instance.as_ref().map(ty::Instance::def_id).and_then(hir::DefId::as_local).unwrap_or(callee_def_id);

                    newly_found_callees.entry((callee_def_id, substs))
                        .and_modify(|previously_reachable_from| previously_reachable_from.extend(reachable_from.clone()))
                        .or_insert_with(|| reachable_from.clone());
                }
            }
        }

        previously_found_callees.extend(newly_found_callees.drain());
    }

    targets.into_values().collect()
}

pub fn apply_mutation_operators<'tcx, 'm>(tcx: TyCtxt<'tcx>, resolver: &mut Resolver, targets: &[Target], ops: &Operators<'_, 'm>) -> Vec<Mut<'m, 'tcx>> {
    let expn_id = resolver.expansion_for_ast_pass(
        DUMMY_SP,
        AstPass::TestHarness,
        &[sym::rustc_attrs],
        None,
    );
    let def_site = DUMMY_SP.with_def_site_ctxt(expn_id.to_expn_id());

    let mut collector = MutationCollector {
        operators: ops,
        tcx,
        resolver,
        def_site,
        current_fn: None,
        next_mut_index: 1,
        mutations: vec![],
    };

    for target in targets {
        match target.item {
            AstDefItem::Item(item) => collector.visit_item(item),
            AstDefItem::ForeignItem(item) => collector.visit_foreign_item(item),
            AstDefItem::AssocItem(item, ctx) => collector.visit_assoc_item(item, ctx),
        }
    }

    collector.mutations
}

pub struct Mutant<'m, 'tcx> {
    pub mutations: Vec<Mut<'m, 'tcx>>,
}

impl<'m, 'tcx> Mutant<'m, 'tcx> {
    pub fn iter_mutations(&self) -> impl Iterator<Item = &BoxedMutation<'m>> {
        self.mutations.iter().map(|m| &m.mutation)
    }

    pub fn iter_substitutions(&self) -> impl Iterator<Item = &SubstDef> {
        self.mutations.iter().flat_map(|m| &m.substs)
    }

    pub fn undetected_diagnostic(&self, sess: &Session) -> String {
        let mut diagnostic = sess.struct_warn("the following mutations were not detected");
        for mutation in &self.mutations {
            diagnostic.span_note(mutation.location.span(), &mutation.display_name());
        }
        diagnostic::emit_str(diagnostic, sess.rc_source_map())
    }
}

pub fn conflicting_targets(a: &Target, b: &Target) -> bool {
    let reachable_from_a = a.reachable_from.iter().map(|(test, _)| test.item.id).collect();
    let reachable_from_b = b.reachable_from.iter().map(|(test, _)| test.item.id).collect();
    !FxHashSet::is_disjoint(&reachable_from_a, &reachable_from_b)
}

pub fn batch_mutations<'m, 'tcx>(targets: &[Target], mutations: Vec<Mut<'m, 'tcx>>, mutant_max_mutations_count: usize) -> Vec<Mutant<'m, 'tcx>> {
    let mut mutants: Vec<Mutant<'m, 'tcx>> = vec![];

    let mut target_map: FxHashMap<MutId, &Target> = Default::default();
    for mutation in &mutations {
        let Some(containing_fn) = mutation.location.containing_fn() else { continue; };
        let Some(target) = targets.iter().find(|target| target.def_id == containing_fn.hir.def_id) else { continue };
        target_map.insert(mutation.id, target);
    }

    'mutation: for mutation in mutations {
        'mutant: for mutant in &mut mutants {
            if mutant.mutations.len() >= mutant_max_mutations_count { continue 'mutant; }

            for subst in &mutation.substs {
                if mutant.iter_substitutions().any(|s| conflicting_substs(s, subst)) {
                    continue 'mutant;
                }
            }

            let Some(target) = target_map.get(&mutation.id) else { break 'mutant; };
            if mutant.mutations.iter().any(|m| target_map.get(&m.id).is_some_and(|t| conflicting_targets(t, target))) {
                continue 'mutant;
            }

            mutant.mutations.push(mutation);
            continue 'mutation;
        }

        mutants.push(Mutant {
            mutations: vec![mutation],
        });
    }

    mutants
}
