use std::collections::VecDeque;
use std::iter;

use itertools::Itertools;
use rustc_data_structures::sync::HashMapExt;
use rustc_hash::FxHashMap;
use rustc_index::IndexVec;
use rustc_middle::ty::ResolverAstLowering;

use crate::analysis::hir;
use crate::analysis::ty::TyCtxt;
use crate::analysis::res;
use crate::codegen::ast;
use crate::codegen::symbols::Span;

pub struct DefResolutions {
    pub node_id_to_def_id: FxHashMap<ast::NodeId, hir::LocalDefId>,
    pub def_id_to_node_id: IndexVec<hir::LocalDefId, ast::NodeId>,
}

impl DefResolutions {
    pub fn from_resolver(resolver: &ResolverAstLowering) -> Self {
        Self {
            node_id_to_def_id: resolver.node_id_to_def_id.clone(),
            def_id_to_node_id: resolver.def_id_to_node_id.clone(),
        }
    }
}

pub mod visit {
    use std::iter;

    use rustc_ast as ast;
    use rustc_hir as hir;
    use rustc_hir::intravisit::Map;
    use rustc_hir::intravisit::nested_filter::{self, NestedFilter};
    use rustc_span::Span;

    pub trait AstHirVisitor<'ast, 'hir>: Sized {
        type Map: hir::intravisit::Map<'hir> = <Self::NestedFilter as NestedFilter<'hir>>::Map;
        type NestedFilter: NestedFilter<'hir> = nested_filter::None;

        fn nested_visit_map(&mut self) -> Self::Map {
            panic!(
                "nested_visit_map must be implemented or consider using \
                `type NestedFilter = nested_filter::None` (the default)"
            );
        }

        fn nested_item(&mut self, id: hir::ItemId) -> Option<&'hir hir::Item<'hir>> {
            Self::NestedFilter::INTER.then(|| self.nested_visit_map().item(id))
        }

        fn nested_trait_item(&mut self, id: hir::TraitItemId) -> Option<&'hir hir::TraitItem<'hir>> {
            Self::NestedFilter::INTER.then(|| self.nested_visit_map().trait_item(id))
        }

        fn nested_impl_item(&mut self, id: hir::ImplItemId) -> Option<&'hir hir::ImplItem<'hir>> {
            Self::NestedFilter::INTER.then(|| self.nested_visit_map().impl_item(id))
        }

        fn nested_foreign_item(&mut self, id: hir::ForeignItemId) -> Option<&'hir hir::ForeignItem<'hir>> {
            Self::NestedFilter::INTER.then(|| self.nested_visit_map().foreign_item(id))
        }

        fn nested_body(&mut self, id: hir::BodyId) -> Option<&'hir hir::Body<'hir>> {
            Self::NestedFilter::INTRA.then(|| self.nested_visit_map().body(id))
        }

        fn visit_fn(&mut self, kind_ast: ast::visit::FnKind<'ast>, span_ast: Span, id_ast: ast::NodeId, kind_hir: hir::intravisit::FnKind<'hir>, decl_hir: &'hir hir::FnDecl<'hir>, body_hir: hir::BodyId, span_hir: Span, id_hir: hir::HirId) {
            walk_fn(self, kind_ast, span_ast, id_ast, kind_hir, decl_hir, body_hir, span_hir, id_hir);
        }

        fn visit_param(&mut self, param_ast: &'ast ast::Param, param_hir: &'hir hir::Param<'hir>) {
            walk_param(self, param_ast, param_hir);
        }

        fn visit_block(&mut self, block_ast: &'ast ast::Block, block_hir: &'hir hir::Block<'hir>) {
            walk_block(self, block_ast, block_hir);
        }

        fn visit_stmt(&mut self, stmt_ast: &'ast ast::Stmt, stmt_hir: &'hir hir::Stmt<'hir>) {
            walk_stmt(self, stmt_ast, stmt_hir);
        }

        fn visit_expr(&mut self, expr_ast: &'ast ast::Expr, expr_hir: &'hir hir::Expr<'hir>) {
            walk_expr(self, expr_ast, expr_hir);
        }
    }

    pub fn walk_fn<'ast, 'hir, T: AstHirVisitor<'ast, 'hir>>(visitor: &mut T, kind_ast: ast::visit::FnKind<'ast>, _span_ast: Span, _id_ast: ast::NodeId, kind_hir: hir::intravisit::FnKind<'hir>, _decl_hir: &'hir hir::FnDecl<'hir>, body_hir: hir::BodyId, _span_hir: Span, _id_hir: hir::HirId) {
        match (kind_ast, kind_hir) {
            | (ast::visit::FnKind::Fn(_, _, sig_ast, _, _, body_ast), hir::intravisit::FnKind::ItemFn(_, _, _))
            | (ast::visit::FnKind::Fn(_, _, sig_ast, _, _, body_ast), hir::intravisit::FnKind::Method(_, _)) => {
                if let Some(body_hir) = visitor.nested_body(body_hir) {
                    for (param_ast, param_hir) in iter::zip(&sig_ast.decl.inputs, body_hir.params) {
                        visitor.visit_param(param_ast, param_hir);
                    }

                    if let Some(body_ast) = body_ast {
                        visit_block_expr(visitor, body_ast, &body_hir.value);
                    }
                }
            }
            (ast::visit::FnKind::Closure(_, decl_ast, expr_ast), hir::intravisit::FnKind::Closure) => {
                if let Some(body_hir) = visitor.nested_body(body_hir) {
                    for (param_ast, param_hir) in iter::zip(&decl_ast.inputs, body_hir.params) {
                        visitor.visit_param(param_ast, param_hir);
                    }

                    visit_matching_expr(visitor, expr_ast, &body_hir.value);
                }
            }

            _ => unreachable!(),
        }
    }

    pub fn walk_param<'ast, 'hir, T: AstHirVisitor<'ast, 'hir>>(_visitor: &mut T, _param_ast: &'ast ast::Param, _param_hir: &'hir hir::Param<'hir>) {}

    pub fn visit_block_expr<'ast, 'hir, T: AstHirVisitor<'ast, 'hir>>(visitor: &mut T, block_ast: &'ast ast::Block, expr_hir: &'hir hir::Expr<'hir>) {
        if let hir::ExprKind::Block(expr_hir_block, _) = expr_hir.kind {
            visitor.visit_block(block_ast, expr_hir_block);
        }
    }

    pub fn walk_block<'ast, 'hir, T: AstHirVisitor<'ast, 'hir>>(visitor: &mut T, block_ast: &'ast ast::Block, block_hir: &'hir hir::Block<'hir>) {
        let block_ast_stmts = block_ast.stmts.iter()
            // These nodes do not exist in the HIR.
            .filter(|stmt| !matches!(stmt.kind, ast::StmtKind::Empty | ast::StmtKind::MacCall(_)))
            // Some item nodes are incompatible across the AST and the HIR, so we skip visiting them.
            .filter(|stmt| !matches!(stmt.kind, ast::StmtKind::Item(_)));
        let block_hir_stmts = block_hir.stmts.iter()
            // See above.
            .filter(|stmt| !matches!(stmt.kind, hir::StmtKind::Item(_)));

        for (stmt_ast, stmt_hir) in iter::zip(block_ast_stmts.clone(), block_hir_stmts) {
            visitor.visit_stmt(stmt_ast, stmt_hir);
        }

        if let Some(block_hir_expr) = block_hir.expr && let Some(block_ast_stmt) = block_ast_stmts.last() {
            let ast::StmtKind::Expr(block_ast_stmt_expr) = &block_ast_stmt.kind else { unreachable!() };
            visit_matching_expr(visitor, block_ast_stmt_expr, block_hir_expr);
        }
    }

    pub fn walk_stmt<'ast, 'hir, T: AstHirVisitor<'ast, 'hir>>(visitor: &mut T, stmt_ast: &'ast ast::Stmt, stmt_hir: &'hir hir::Stmt<'hir>) {
        match (&stmt_ast.kind, &stmt_hir.kind) {
            | (ast::StmtKind::Expr(expr_ast), hir::StmtKind::Expr(expr_hir))
            | (ast::StmtKind::Semi(expr_ast), hir::StmtKind::Semi(expr_hir)) => {
                visit_matching_expr(visitor, expr_ast, expr_hir);
            }

            (ast::StmtKind::Local(local_ast), hir::StmtKind::Local(local_hir)) => {
                match (&local_ast.kind, local_hir.init) {
                    | (ast::LocalKind::Init(expr_ast), Some(expr_hir))
                    | (ast::LocalKind::InitElse(expr_ast, _), Some(expr_hir)) => {
                        visit_matching_expr(visitor, expr_ast, expr_hir);
                    }

                    (ast::LocalKind::Decl, None) => {}
                    _ => unreachable!(),
                }
            }

            (ast::StmtKind::Item(_item_ast), hir::StmtKind::Item(_item_hir)) => {
                // TODO
            }

            (ast::StmtKind::Empty, _) | (ast::StmtKind::MacCall(_), _) => {}
            _ => unreachable!(),
        }
    }

    pub fn visit_matching_expr<'ast, 'hir, T: AstHirVisitor<'ast, 'hir>>(visitor: &mut T, expr_ast: &'ast ast::Expr, expr_hir: &'hir hir::Expr<'hir>) {
        match (&expr_ast.kind, &expr_hir.kind) {
            (_, hir::ExprKind::DropTemps(expr_hir)) => {
                visit_matching_expr(visitor, expr_ast, expr_hir);
            }
            (ast::ExprKind::Paren(expr_ast), _) => {
                visit_matching_expr(visitor, expr_ast, expr_hir);
            }

            _ => visitor.visit_expr(expr_ast, expr_hir),
        }
    }

    pub fn walk_expr<'ast, 'hir, T: AstHirVisitor<'ast, 'hir>>(visitor: &mut T, expr_ast: &'ast ast::Expr, expr_hir: &'hir hir::Expr<'hir>) {
        match (&expr_ast.kind, &expr_hir.kind) {
            (_, hir::ExprKind::DropTemps(expr_hir)) => {
                visit_matching_expr(visitor, expr_ast, expr_hir);
            }
            (ast::ExprKind::Paren(expr_ast), _) => {
                visit_matching_expr(visitor, expr_ast, expr_hir);
            }

            (ast::ExprKind::Array(exprs_ast), hir::ExprKind::Array(exprs_hir)) => {
                for (expr_ast, expr_hir) in iter::zip(exprs_ast, *exprs_hir) {
                    visit_matching_expr(visitor, expr_ast, expr_hir);
                }
            }
            (ast::ExprKind::ConstBlock(_anon_const_ast), hir::ExprKind::ConstBlock(_anon_const_hir)) => {
                // TODO: Visit anonymous const
            }
            (ast::ExprKind::Call(expr_ast, args_ast), hir::ExprKind::Call(expr_hir, args_hir)) => {
                visit_matching_expr(visitor, expr_ast, expr_hir);
                for (arg_ast, arg_hir) in iter::zip(args_ast, *args_hir) {
                    visit_matching_expr(visitor, arg_ast, arg_hir);
                }
            }
            (ast::ExprKind::MethodCall(method_call_ast), hir::ExprKind::MethodCall(_, receiver_hir, args_hir, _)) => {
                let ast::MethodCall { seg: _, receiver: receiver_ast, args: args_ast, span: _ } = &**method_call_ast;

                visit_matching_expr(visitor, receiver_ast, receiver_hir);
                for (expr_ast, expr_hir) in iter::zip(args_ast, *args_hir) {
                    visit_matching_expr(visitor, expr_ast, expr_hir);
                }
            }
            (ast::ExprKind::Tup(exprs_ast), hir::ExprKind::Tup(exprs_hir)) => {
                for (expr_ast, expr_hir) in iter::zip(exprs_ast, *exprs_hir) {
                    visit_matching_expr(visitor, expr_ast, expr_hir);
                }
            }
            (ast::ExprKind::Binary(_, left_ast, right_ast), hir::ExprKind::Binary(_, left_hir, right_hir)) => {
                visit_matching_expr(visitor, left_ast, left_hir);
                visit_matching_expr(visitor, right_ast, right_hir);
            }
            (ast::ExprKind::Unary(_, expr_ast), hir::ExprKind::Unary(_, expr_hir)) => {
                visit_matching_expr(visitor, expr_ast, expr_hir);
            }
            (ast::ExprKind::Lit(_), hir::ExprKind::Lit(_)) => {
                // TODO
            }
            (ast::ExprKind::Cast(expr_ast, _), hir::ExprKind::Cast(expr_hir, _)) => {
                visit_matching_expr(visitor, expr_ast, expr_hir);
            }
            (ast::ExprKind::Type(expr_ast, _), hir::ExprKind::Type(expr_hir, _)) => {
                visit_matching_expr(visitor, expr_ast, expr_hir);
            }
            (ast::ExprKind::Let(_, expr_ast, _, _), hir::ExprKind::Let(let_hir)) => {
                visit_matching_expr(visitor, expr_ast, let_hir.init);
            }
            (ast::ExprKind::If(cond_ast, then_ast, els_ast), hir::ExprKind::If(cond_hir, then_hir, els_hir)) => {
                visit_matching_expr(visitor, cond_ast, cond_hir);
                visit_block_expr(visitor, then_ast, then_hir);
                if let Some(els_ast) = els_ast && let Some(els_hir) = els_hir {
                    visit_matching_expr(visitor, els_ast, els_hir);
                }
            }
            (ast::ExprKind::While(expr_ast, block_ast, _label_ast), hir::ExprKind::Loop(block_hir, _label_hir, hir::LoopSource::While, _)) => {
                // TODO: Visit label
                if let Some(block_expr_hir) = block_hir.expr
                    && let hir::ExprKind::If(cond_hir, then_hir, _) = block_expr_hir.kind
                {
                    visit_matching_expr(visitor, expr_ast, cond_hir);
                    if let hir::ExprKind::Block(block_hir, _) = then_hir.kind {
                        visitor.visit_block(block_ast, block_hir);
                    }
                }
            }
            (ast::ExprKind::ForLoop(_pat_ast, cond_ast, block_ast, _label_ast), hir::ExprKind::Match(expr_hir, [arm_hir], hir::MatchSource::ForLoopDesugar)) => {
                if let hir::ExprKind::Loop(block_hir, _label_hir, hir::LoopSource::ForLoop, _) = arm_hir.body.kind {
                    // TODO: Visit label
                    if let [block_stmt_hir] = block_hir.stmts
                        && let hir::StmtKind::Expr(block_expr_hir) = block_stmt_hir.kind
                        && let hir::ExprKind::Match(_, [_, some_arm_hir], hir::MatchSource::ForLoopDesugar) = block_expr_hir.kind
                    {
                        if let hir::PatKind::TupleStruct(_, [_pat_hir], _) = some_arm_hir.pat.kind {
                            // TODO: Visit pattern
                        }
                        if let hir::ExprKind::Call(_, [cond_hir]) = expr_hir.kind {
                            visit_matching_expr(visitor, cond_ast, cond_hir);
                        }
                        visit_block_expr(visitor, block_ast, some_arm_hir.body);
                    }
                }
            }
            (ast::ExprKind::Loop(block_ast, _, _), hir::ExprKind::Loop(block_hir, _, hir::LoopSource::Loop, _)) => {
                visitor.visit_block(block_ast, block_hir);
            }
            (ast::ExprKind::Match(expr_ast, arms_ast), hir::ExprKind::Match(expr_hir, arms_hir, hir::MatchSource::Normal)) => {
                visit_matching_expr(visitor, expr_ast, expr_hir);
                for (arm_ast, arm_hir) in iter::zip(arms_ast, *arms_hir) {
                    // TODO: Visit pattern
                    visit_matching_expr(visitor, &arm_ast.body, arm_hir.body);
                }
            }
            (ast::ExprKind::Closure(closure_ast), hir::ExprKind::Closure(closure_hir)) => {
                let ast::Closure { binder: _, capture_clause: _, constness: _, asyncness: _, movability: _, fn_decl: decl_ast, body: expr_ast, fn_decl_span: _, fn_arg_span: _ } = &**closure_ast;
                let hir::Closure { def_id: _, binder: _, constness: _, capture_clause: _, bound_generic_params: _, fn_decl: _, body: body_hir, fn_decl_span: _, fn_arg_span: _, movability: _ } = closure_hir;

                if let Some(body_hir) = visitor.nested_body(*body_hir) {
                    for (param_ast, param_hir) in iter::zip(&decl_ast.inputs, body_hir.params) {
                        visitor.visit_param(param_ast, param_hir);
                    }
                    visit_matching_expr(visitor, expr_ast, &body_hir.value);
                }
            }
            (ast::ExprKind::Block(block_ast, _), hir::ExprKind::Block(block_hir, _)) => {
                visitor.visit_block(block_ast, block_hir);
            }
            (ast::ExprKind::Async(_, _), _) => {
                // TODO
            }
            (ast::ExprKind::Await(expr_ast, _), hir::ExprKind::Match(expr_hir, _, hir::MatchSource::AwaitDesugar)) => {
                visit_matching_expr(visitor, expr_ast, expr_hir);
            }
            (ast::ExprKind::TryBlock(_), _) => {
                // TODO
            }
            (ast::ExprKind::Assign(left_ast, right_ast, _), hir::ExprKind::Assign(left_hir, right_hir, _)) => {
                visit_matching_expr(visitor, left_ast, left_hir);
                visit_matching_expr(visitor, right_ast, right_hir);
            }
            (ast::ExprKind::AssignOp(_, left_ast, right_ast), hir::ExprKind::AssignOp(_, left_hir, right_hir)) => {
                visit_matching_expr(visitor, left_ast, left_hir);
                visit_matching_expr(visitor, right_ast, right_hir);
            }
            (ast::ExprKind::Field(expr_ast, _), hir::ExprKind::Field(expr_hir, _)) => {
                visit_matching_expr(visitor, expr_ast, expr_hir);
            }
            (ast::ExprKind::Index(expr_ast, index_ast, _), hir::ExprKind::Index(expr_hir, index_hir, _)) => {
                visit_matching_expr(visitor, expr_ast, expr_hir);
                visit_matching_expr(visitor, index_ast, index_hir);
            }
            (ast::ExprKind::Range(Some(start_ast), Some(end_ast), ast::RangeLimits::Closed), hir::ExprKind::Call(path_expr_hir, [start_hir, end_hir])) => {
                if let hir::ExprKind::Path(qpath_hir) = &path_expr_hir.kind
                    && let hir::QPath::LangItem(hir::LangItem::RangeInclusiveNew, _, _) = qpath_hir
                {
                    visit_matching_expr(visitor, start_ast, start_hir);
                    visit_matching_expr(visitor, end_ast, end_hir);
                }
            }
            (ast::ExprKind::Range(start_ast, end_ast, limits), hir::ExprKind::Struct(qpath_hir, fields_hir, _)) => {
                let expected_lang_item = match (start_ast, end_ast, limits) {
                    (None, None, ast::RangeLimits::HalfOpen) => hir::LangItem::RangeFull,
                    (Some(_), None, ast::RangeLimits::HalfOpen) => hir::LangItem::RangeFrom,
                    (None, Some(_), ast::RangeLimits::HalfOpen) => hir::LangItem::RangeTo,
                    (Some(_), Some(_), ast::RangeLimits::HalfOpen) => hir::LangItem::Range,
                    (None, Some(_), ast::RangeLimits::Closed) => hir::LangItem::RangeToInclusive,
                    _ => unreachable!(),
                };

                if let hir::QPath::LangItem(lang_item_hir, _, _) = qpath_hir && *lang_item_hir == expected_lang_item {
                    let mut fields_hir_iter = fields_hir.iter();
                    if let Some(start_ast) = start_ast && let Some(start_field_hir) = fields_hir_iter.next() {
                        visit_matching_expr(visitor, start_ast, start_field_hir.expr);
                    }
                    if let Some(end_ast) = end_ast && let Some(end_field_hir) = fields_hir_iter.next() {
                        visit_matching_expr(visitor, end_ast, end_field_hir.expr);
                    }
                }
            }
            (ast::ExprKind::Underscore, _) => {}
            (ast::ExprKind::Path(_, _), hir::ExprKind::Path(_)) => {
                // TODO
            }
            (ast::ExprKind::AddrOf(_, _, expr_ast), hir::ExprKind::AddrOf(_, _, expr_hir)) => {
                visit_matching_expr(visitor, expr_ast, expr_hir);
            }
            (ast::ExprKind::Break(_, expr_ast), hir::ExprKind::Break(_, expr_hir)) => {
                if let Some(expr_ast) = expr_ast && let Some(expr_hir) = expr_hir {
                    visit_matching_expr(visitor, expr_ast, expr_hir);
                }
            }
            (ast::ExprKind::Continue(_), hir::ExprKind::Continue(_)) => {
                // TODO
            }
            (ast::ExprKind::Ret(expr_ast), hir::ExprKind::Ret(expr_hir)) => {
                if let Some(expr_ast) = expr_ast && let Some(expr_hir) = expr_hir {
                    visit_matching_expr(visitor, expr_ast, expr_hir);
                }
            }
            (ast::ExprKind::InlineAsm(_), hir::ExprKind::InlineAsm(_)) => {
                // TODO
            }
            (ast::ExprKind::MacCall(_), _) => {}
            (ast::ExprKind::Struct(struct_ast), hir::ExprKind::Struct(_, fields_hir, base_hir)) => {
                // TODO
                for (field_ast, field_hir) in iter::zip(&struct_ast.fields, *fields_hir) {
                    // TODO
                    visit_matching_expr(visitor, &field_ast.expr, field_hir.expr);
                }
                if let ast::StructRest::Base(base_ast) = &struct_ast.rest && let Some(base_hir) = base_hir {
                    visit_matching_expr(visitor, base_ast, base_hir);
                }
            }
            (ast::ExprKind::Repeat(expr_ast, _anon_const_ast), hir::ExprKind::Repeat(expr_hir, _anon_const_hir)) => {
                visit_matching_expr(visitor, expr_ast, expr_hir);
                // TODO: Visit anonymous const
            }
            (ast::ExprKind::Try(expr_ast), hir::ExprKind::Match(expr_hir, _, hir::MatchSource::TryDesugar(_))) => {
                if let hir::ExprKind::Call(path_expr_hir, [expr_hir]) = &expr_hir.kind
                    && let hir::ExprKind::Path(qpath_hir) = &path_expr_hir.kind
                    && let hir::QPath::LangItem(lang_item_hir, _, _) = qpath_hir && *lang_item_hir == hir::LangItem::TryTraitBranch
                {
                    visit_matching_expr(visitor, expr_ast, expr_hir);
                }
            }
            (ast::ExprKind::Yield(expr_ast), hir::ExprKind::Yield(expr_hir, _)) => {
                if let Some(expr_ast) = expr_ast {
                    visit_matching_expr(visitor, expr_ast, expr_hir);
                }
            }

            (ast::ExprKind::Err, _) | (_, hir::ExprKind::Err(_)) => {}
            _ => unreachable!(),
        }
    }
}

pub struct BodyResolutions<'tcx> {
    tcx: TyCtxt<'tcx>,
    node_id_to_hir_id: FxHashMap<ast::NodeId, hir::HirId>,
    hir_id_to_node_id: FxHashMap<hir::HirId, ast::NodeId>,
}

impl<'tcx> BodyResolutions<'tcx> {
    pub fn ast_id(&self, hir_id: hir::HirId) -> Option<ast::NodeId> {
        self.hir_id_to_node_id.get(&hir_id).copied()
    }

    pub fn hir_id(&self, node_id: ast::NodeId) -> Option<hir::HirId> {
        self.node_id_to_hir_id.get(&node_id).copied()
    }

    pub fn hir_node(&self, node_id: ast::NodeId) -> Option<hir::Node<'tcx>> {
        self.node_id_to_hir_id.get(&node_id).map(|&hir_id| self.tcx.hir().get(hir_id))
    }

    pub fn hir_param(&self, param: &ast::Param) -> Option<&'tcx hir::Param<'tcx>> {
        self.hir_node(param.id).map(|hir_node| hir_node.expect_param())
    }

    pub fn hir_expr(&self, expr: &ast::Expr) -> Option<&'tcx hir::Expr<'tcx>> {
        self.hir_node(expr.id).map(|hir_node| hir_node.expect_expr())
    }

    pub fn hir_stmt(&self, stmt: &ast::Stmt) -> Option<&'tcx hir::Stmt<'tcx>> {
        self.hir_node(stmt.id).map(|hir_node| hir_node.expect_stmt())
    }

    pub fn hir_block(&self, block: &ast::Block) -> Option<&'tcx hir::Block<'tcx>> {
        self.hir_node(block.id).map(|hir_node| hir_node.expect_block())
    }
}

struct BodyResolutionsCollector<'tcx> {
    tcx: TyCtxt<'tcx>,
    node_id_to_hir_id: FxHashMap<ast::NodeId, hir::HirId>,
    hir_id_to_node_id: FxHashMap<hir::HirId, ast::NodeId>,
}

impl<'ast, 'hir> visit::AstHirVisitor<'ast, 'hir> for BodyResolutionsCollector<'hir> {
    type NestedFilter = rustc_middle::hir::nested_filter::OnlyBodies;

    fn nested_visit_map(&mut self) -> Self::Map {
        self.tcx.hir()
    }

    fn visit_fn(&mut self, kind_ast: ast::visit::FnKind<'ast>, span_ast: Span, id_ast: ast::NodeId, kind_hir: hir::intravisit::FnKind<'hir>, decl_hir: &'hir hir::FnDecl<'hir>, body_hir: hir::BodyId, span_hir: Span, id_hir: hir::HirId) {
        visit::walk_fn(self, kind_ast, span_ast, id_ast, kind_hir, decl_hir, body_hir, span_hir, id_hir);
    }

    fn visit_param(&mut self, param_ast: &'ast ast::Param, param_hir: &'hir hir::Param<'hir>) {
        self.node_id_to_hir_id.insert_same(param_ast.id, param_hir.hir_id);
        self.hir_id_to_node_id.insert_same(param_hir.hir_id, param_ast.id);

        visit::walk_param(self, param_ast, param_hir);
    }

    fn visit_block(&mut self, block_ast: &'ast ast::Block, block_hir: &'hir hir::Block<'hir>) {
        self.node_id_to_hir_id.insert_same(block_ast.id, block_hir.hir_id);
        self.hir_id_to_node_id.insert_same(block_hir.hir_id, block_ast.id);

        visit::walk_block(self, block_ast, block_hir);
    }

    fn visit_stmt(&mut self, stmt_ast: &'ast ast::Stmt, stmt_hir: &'hir hir::Stmt<'hir>) {
        self.node_id_to_hir_id.insert_same(stmt_ast.id, stmt_hir.hir_id);
        self.hir_id_to_node_id.insert_same(stmt_hir.hir_id, stmt_ast.id);

        visit::walk_stmt(self, stmt_ast, stmt_hir);
    }

    fn visit_expr(&mut self, expr_ast: &'ast ast::Expr, expr_hir: &'hir hir::Expr<'hir>) {
        self.node_id_to_hir_id.insert_same(expr_ast.id, expr_hir.hir_id);
        self.hir_id_to_node_id.insert_same(expr_hir.hir_id, expr_ast.id);

        visit::walk_expr(self, expr_ast, expr_hir);
    }
}

pub fn resolve_body<'tcx>(tcx: TyCtxt<'tcx>, fn_ast: &ast::FnItem, fn_hir: &hir::FnItem<'tcx>) -> BodyResolutions<'tcx> {
    let mut collector = BodyResolutionsCollector {
        tcx,
        node_id_to_hir_id: Default::default(),
        hir_id_to_node_id: Default::default(),
    };

    let kind_ast = ast::visit::FnKind::Fn(fn_ast.ctx, fn_ast.ident, &fn_ast.sig, &fn_ast.vis, &fn_ast.generics, fn_ast.body.as_ref());
    let span_ast = fn_ast.span;
    let id_ast = fn_ast.id;
    let kind_hir = fn_hir.kind;
    let decl_hir = fn_hir.sig.decl;
    let body_hir = fn_hir.body.id();
    let span_hir = fn_hir.span;
    let id_hir = tcx.hir().local_def_id_to_hir_id(fn_hir.owner_id.def_id);
    visit::AstHirVisitor::visit_fn(&mut collector, kind_ast, span_ast, id_ast, kind_hir, decl_hir, body_hir, span_hir, id_hir);

    BodyResolutions {
        tcx: collector.tcx,
        node_id_to_hir_id: collector.node_id_to_hir_id,
        hir_id_to_node_id: collector.hir_id_to_node_id,
    }
}

struct AstBodyChildItemCollector<'ast> {
    result: Vec<ast::DefItem<'ast>>,
}

impl<'ast> ast::visit::Visitor<'ast> for AstBodyChildItemCollector<'ast> {
    fn visit_item(&mut self, item: &'ast ast::Item) {
        self.result.push(ast::DefItem::Item(item));
    }

    fn visit_foreign_item(&mut self, item: &'ast ast::ForeignItem) {
        self.result.push(ast::DefItem::ForeignItem(item));
    }

    fn visit_assoc_item(&mut self, item: &'ast ast::AssocItem, ctx: rustc_ast::visit::AssocCtxt) {
        self.result.push(ast::DefItem::AssocItem(item, ctx));
    }
}

fn ast_body_child_items<'ast>(block: &'ast ast::Block) -> Vec<ast::DefItem<'ast>> {
    use ast::visit::Visitor;

    let mut collector = AstBodyChildItemCollector { result: vec![] };
    collector.visit_block(block);
    collector.result
}

struct HirBodyChildItemCollector<'tcx> {
    tcx: TyCtxt<'tcx>,
    result: Vec<hir::DefItem<'tcx>>,
}

impl<'hir> hir::intravisit::Visitor<'hir> for HirBodyChildItemCollector<'hir> {
    type NestedFilter = rustc_middle::hir::nested_filter::All;

    fn nested_visit_map(&mut self) -> Self::Map {
        self.tcx.hir()
    }

    fn visit_item(&mut self, item: &'hir hir::Item<'hir>) {
        self.result.push(hir::DefItem::Item(item));
    }

    fn visit_foreign_item(&mut self, item: &'hir hir::ForeignItem<'hir>) {
        self.result.push(hir::DefItem::ForeignItem(item));
    }

    fn visit_trait_item(&mut self, item: &'hir hir::TraitItem<'hir>) {
        self.result.push(hir::DefItem::TraitItem(item));
    }

    fn visit_impl_item(&mut self, item: &'hir hir::ImplItem<'hir>) {
        self.result.push(hir::DefItem::ImplItem(item));
    }
}

fn hir_body_child_items<'tcx>(tcx: TyCtxt<'tcx>, body: &'tcx hir::Body<'tcx>) -> Vec<hir::DefItem<'tcx>> {
    use hir::intravisit::Visitor;

    let mut collector = HirBodyChildItemCollector { tcx, result: vec![] };
    collector.visit_body(body);
    collector.result
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct DefDisambiguator {
    pub kind: hir::definitions::DefPathData,
    pub disambiguator: usize,
}

fn disambiguate_hir_def_item_node_path_components<'tcx>(tcx: TyCtxt<'tcx>, path: &[hir::Node]) -> Vec<Option<DefDisambiguator>> {
    fn disambiguated_kind<'hir>(def_item: hir::DefItem<'hir>) -> Option<hir::definitions::DefPathData> {
        match def_item {
            hir::DefItem::Item(item) => match item.kind {
                hir::ItemKind::ExternCrate(_) => Some(hir::definitions::DefPathData::TypeNs(item.ident.name)),
                hir::ItemKind::Use(_, _) => None,
                hir::ItemKind::Static(_, _, _) => Some(hir::definitions::DefPathData::ValueNs(item.ident.name)),
                hir::ItemKind::Const(_, _, _) => Some(hir::definitions::DefPathData::ValueNs(item.ident.name)),
                hir::ItemKind::Fn(_, _, _) => Some(hir::definitions::DefPathData::ValueNs(item.ident.name)),
                hir::ItemKind::Macro(_, _) => Some(hir::definitions::DefPathData::MacroNs(item.ident.name)),
                hir::ItemKind::Mod(_) => Some(hir::definitions::DefPathData::TypeNs(item.ident.name)),
                hir::ItemKind::ForeignMod { .. } => Some(hir::definitions::DefPathData::ForeignMod),
                hir::ItemKind::GlobalAsm(_) => Some(hir::definitions::DefPathData::GlobalAsm),
                hir::ItemKind::TyAlias(_, _) => Some(hir::definitions::DefPathData::TypeNs(item.ident.name)),
                hir::ItemKind::OpaqueTy(_) => Some(hir::definitions::DefPathData::ImplTrait),
                hir::ItemKind::Enum(_, _) => Some(hir::definitions::DefPathData::TypeNs(item.ident.name)),
                hir::ItemKind::Struct(_, _) => Some(hir::definitions::DefPathData::TypeNs(item.ident.name)),
                hir::ItemKind::Union(_, _) => Some(hir::definitions::DefPathData::TypeNs(item.ident.name)),
                hir::ItemKind::Trait(_, _, _, _, _) => Some(hir::definitions::DefPathData::TypeNs(item.ident.name)),
                hir::ItemKind::TraitAlias(_, _) => Some(hir::definitions::DefPathData::TypeNs(item.ident.name)),
                hir::ItemKind::Impl(_) => Some(hir::definitions::DefPathData::Impl),
            }
            hir::DefItem::ForeignItem(item) => match item.kind {
                hir::ForeignItemKind::Fn(_, _, _) => Some(hir::definitions::DefPathData::ValueNs(item.ident.name)),
                hir::ForeignItemKind::Static(_, _) => Some(hir::definitions::DefPathData::ValueNs(item.ident.name)),
                hir::ForeignItemKind::Type => Some(hir::definitions::DefPathData::TypeNs(item.ident.name)),
            }
            hir::DefItem::TraitItem(item) => match item.kind {
                hir::TraitItemKind::Const(_, _) => Some(hir::definitions::DefPathData::ValueNs(item.ident.name)),
                hir::TraitItemKind::Fn(_, _) => Some(hir::definitions::DefPathData::ValueNs(item.ident.name)),
                hir::TraitItemKind::Type(_, _) => Some(hir::definitions::DefPathData::TypeNs(item.ident.name)),
            }
            hir::DefItem::ImplItem(item) => match item.kind {
                hir::ImplItemKind::Const(_, _) => Some(hir::definitions::DefPathData::ValueNs(item.ident.name)),
                hir::ImplItemKind::Fn(_, _) => Some(hir::definitions::DefPathData::ValueNs(item.ident.name)),
                hir::ImplItemKind::Type(_) => Some(hir::definitions::DefPathData::TypeNs(item.ident.name)),
            }
        }
    }

    fn disambiguate_child_item<'hir, I>(def_item: hir::DefItem<'hir>, items: I) -> Option<DefDisambiguator>
    where
        I: IntoIterator<Item = hir::DefItem<'hir>>,
    {
        let kind = disambiguated_kind(def_item);

        let disambiguator = items.into_iter()
            .filter(|&item| disambiguated_kind(item) == kind)
            .position(|item| item.def_id() == def_item.def_id());

        match (kind, disambiguator) {
            (Some(kind), Some(disambiguator)) => Some(DefDisambiguator { kind, disambiguator }),
            _ => None,
        }
    }

    path.iter()
        .tuple_windows::<(_, _)>()
        .map(|(parent_node, current_node)| {
            let Some(current_def_item) = hir::DefItem::from_node(current_node) else { return None; };

            match parent_node {
                hir::Node::Crate(parent_mod) => {
                    let items = parent_mod.item_ids.iter().map(|item_id| tcx.hir().item(*item_id)).map(hir::DefItem::Item);
                    disambiguate_child_item(current_def_item, items)
                }
                hir::Node::Item(parent_item) => match &parent_item.kind {
                    hir::ItemKind::Mod(parent_mod) => {
                        let items = parent_mod.item_ids.iter().map(|item_id| tcx.hir().item(*item_id)).map(hir::DefItem::Item);
                        disambiguate_child_item(current_def_item, items)
                    }
                    hir::ItemKind::ForeignMod { items: parent_items, .. } => {
                        let items = parent_items.iter().map(|item_ref| tcx.hir().foreign_item(item_ref.id)).map(hir::DefItem::ForeignItem);
                        disambiguate_child_item(current_def_item, items)
                    }
                    hir::ItemKind::Trait(_, _, _, _, parent_items) => {
                        let items = parent_items.iter().map(|item_ref| tcx.hir().trait_item(item_ref.id)).map(hir::DefItem::TraitItem);
                        disambiguate_child_item(current_def_item, items)
                    }
                    hir::ItemKind::Impl(parent_impl) => {
                        let items = parent_impl.items.iter().map(|item_ref| tcx.hir().impl_item(item_ref.id)).map(hir::DefItem::ImplItem);
                        disambiguate_child_item(current_def_item, items)
                    }
                    | hir::ItemKind::Static(_, _, parent_body)
                    | hir::ItemKind::Const(_, _, parent_body)
                    | hir::ItemKind::Fn(_, _, parent_body) => {
                        disambiguate_child_item(current_def_item, hir_body_child_items(tcx, tcx.hir().body(*parent_body)))
                    }
                    _ => None,
                }
                hir::Node::TraitItem(parent_item) => match &parent_item.kind {
                    | hir::TraitItemKind::Const(_, Some(parent_body))
                    | hir::TraitItemKind::Fn(_, hir::TraitFn::Provided(parent_body)) => {
                        disambiguate_child_item(current_def_item, hir_body_child_items(tcx, tcx.hir().body(*parent_body)))
                    }
                    _ => None,
                }
                hir::Node::ImplItem(parent_item) => match &parent_item.kind {
                    | hir::ImplItemKind::Const(_, parent_body)
                    | hir::ImplItemKind::Fn(_, parent_body) => {
                        disambiguate_child_item(current_def_item, hir_body_child_items(tcx, tcx.hir().body(*parent_body)))
                    }
                    _ => None,
                }
                _ => None,
            }
        })
        .collect()
}

struct AstDefFinder<'ast, 'hir> {
    path_stack: VecDeque<(hir::Node<'hir>, Option<DefDisambiguator>)>,
    result: Option<ast::DefItem<'ast>>,
}

fn find_hir_def_item_in_ast<'ast, 'hir, I>(item_hir: hir::DefItem<'hir>, disambiguator: Option<DefDisambiguator>, items_ast: I) -> Option<ast::DefItem<'ast>>
where
    I: IntoIterator<Item = ast::DefItem<'ast>>,
{
    macro matching_item {
        ($(|)? $($pat:pat_param)|+ $(if $guard:expr)? $(,)?) => {
            items_ast.into_iter().find(|&item_ast| matches!(&item_ast.kind(), $($pat)|+ $(if $guard)?))
        },
        ($(|)? $($pat:pat_param)|+ $(if $guard:expr)? => |$item_ast:ident| $filter:expr $(,)?) => {
            items_ast.into_iter().find(|&$item_ast| matches!(&$item_ast.kind(), $($pat)|+ $(if $guard)?) && $filter)
        },
    }

    match item_hir {
        hir::DefItem::Item(item_hir) => match &item_hir.kind {
            hir::ItemKind::ExternCrate(symbol_hir) => {
                matching_item!(ast::ItemKind::ExternCrate(symbol_ast) if symbol_ast == symbol_hir)
            }
            hir::ItemKind::Use(_, _) => None,
            hir::ItemKind::Static(_, _, _) => {
                matching_item!(ast::ItemKind::Static(_) => |item_ast| item_ast.ident() == item_hir.ident)
            }
            hir::ItemKind::Const(_, _, _) => {
                matching_item!(ast::ItemKind::Const(_) => |item_ast| item_ast.ident() == item_hir.ident)
            }
            hir::ItemKind::Fn(_, _, _) => {
                matching_item!(ast::ItemKind::Fn(_) => |item_ast| item_ast.ident() == item_hir.ident)
            }
            hir::ItemKind::Macro(def_hir, _) => {
                matching_item!(ast::ItemKind::MacroDef(def_ast) if std::ptr::eq(*def_hir, def_ast))
            }
            hir::ItemKind::Mod(_) => {
                matching_item!(ast::ItemKind::Mod(_, _) => |item_ast| item_ast.ident() == item_hir.ident)
            }
            hir::ItemKind::ForeignMod { abi: _, items: _ } => {
                let Some(disambiguator) = disambiguator else { return None; };

                let hir::definitions::DefPathData::ForeignMod = disambiguator.kind else { return None; };
                let index = disambiguator.disambiguator;

                items_ast.into_iter().filter(|&item_ast| matches!(&item_ast.kind(), ast::ItemKind::ForeignMod(_))).nth(index)
            }
            hir::ItemKind::GlobalAsm(_) => {
                let Some(disambiguator) = disambiguator else { return None; };

                let hir::definitions::DefPathData::GlobalAsm = disambiguator.kind else { return None; };
                let index = disambiguator.disambiguator;

                items_ast.into_iter().filter(|&item_ast| matches!(&item_ast.kind(), ast::ItemKind::GlobalAsm(_))).nth(index)
            }
            hir::ItemKind::TyAlias(_, _) => {
                matching_item!(ast::ItemKind::TyAlias(_) => |item_ast| item_ast.ident() == item_hir.ident)
            }
            hir::ItemKind::OpaqueTy(_) => None,
            hir::ItemKind::Enum(_, _) => {
                matching_item!(ast::ItemKind::Enum(_, _) => |item_ast| item_ast.ident() == item_hir.ident)
            }
            hir::ItemKind::Struct(_, _) => {
                matching_item!(ast::ItemKind::Struct(_, _) => |item_ast| item_ast.ident() == item_hir.ident)
            }
            hir::ItemKind::Union(_, _) => {
                matching_item!(ast::ItemKind::Union(_, _) => |item_ast| item_ast.ident() == item_hir.ident)
            }
            hir::ItemKind::Trait(_, _, _, _, _) => {
                matching_item!(ast::ItemKind::Trait(_) => |item_ast| item_ast.ident() == item_hir.ident)
            }
            hir::ItemKind::TraitAlias(_, _) => {
                matching_item!(ast::ItemKind::TraitAlias(_, _) => |item_ast| item_ast.ident() == item_hir.ident)
            }
            hir::ItemKind::Impl(_) => {
                let Some(disambiguator) = disambiguator else { return None; };

                let hir::definitions::DefPathData::Impl = disambiguator.kind else { return None; };
                let index = disambiguator.disambiguator;

                items_ast.into_iter().filter(|&item_ast| matches!(&item_ast.kind(), ast::ItemKind::Impl(_))).nth(index)
            }
        }
        hir::DefItem::ForeignItem(item_hir) => match &item_hir.kind {
            hir::ForeignItemKind::Fn(_, _, _) => {
                matching_item!(ast::ItemKind::Fn(_) => |item_ast| item_ast.ident() == item_hir.ident)
            }
            hir::ForeignItemKind::Static(_, _) => {
                matching_item!(ast::ItemKind::Static(_) => |item_ast| item_ast.ident() == item_hir.ident)
            }
            hir::ForeignItemKind::Type => {
                matching_item!(ast::ItemKind::TyAlias(_) => |item_ast| item_ast.ident() == item_hir.ident)
            }
        }
        hir::DefItem::TraitItem(item_hir) => match &item_hir.kind {
            hir::TraitItemKind::Const(_, _) => {
                matching_item!(ast::ItemKind::Const(_) => |item_ast| item_ast.ident() == item_hir.ident)
            }
            hir::TraitItemKind::Fn(_, _) => {
                matching_item!(ast::ItemKind::Fn(_) => |item_ast| item_ast.ident() == item_hir.ident)
            }
            hir::TraitItemKind::Type(_, _) => {
                matching_item!(ast::ItemKind::TyAlias(_) => |item_ast| item_ast.ident() == item_hir.ident)
            }
        }
        hir::DefItem::ImplItem(item_hir) => match &item_hir.kind {
            hir::ImplItemKind::Const(_, _) => {
                matching_item!(ast::ItemKind::Const(_) => |item_ast| item_ast.ident() == item_hir.ident)
            }
            hir::ImplItemKind::Fn(_, _) => {
                matching_item!(ast::ItemKind::Fn(_) => |item_ast| item_ast.ident() == item_hir.ident)
            }
            hir::ImplItemKind::Type(_) => {
                matching_item!(ast::ItemKind::TyAlias(_) => |item_ast| item_ast.ident() == item_hir.ident)
            }
        }
    }
}

impl<'ast, 'hir> ast::visit::Visitor<'ast> for AstDefFinder<'ast, 'hir> {
    fn visit_crate(&mut self, krate: &'ast ast::Crate) {
        let Some(&(node_hir, None)) = self.path_stack.front() else { return; };

        let hir::Node::Crate(_) = node_hir else { return; };
        self.path_stack.pop_front();

        let Some(&(node_hir, disambiguator)) = self.path_stack.front() else { return; };
        let Some(def_item_hir) = hir::DefItem::from_node(&node_hir) else { return; };

        let def_item_ast = match def_item_hir {
            hir::DefItem::Item(_) => {
                let def_items_ast = krate.items.iter().map(ast::AstDeref::ast_deref).map(ast::DefItem::Item);
                find_hir_def_item_in_ast(def_item_hir, disambiguator, def_items_ast)
            }
            _ => { return; }
        };

        let Some(def_item_ast) = def_item_ast else { return; };
        self.path_stack.pop_front();

        if self.path_stack.is_empty() {
            self.result = Some(def_item_ast);
            return;
        }

        match def_item_ast {
            ast::DefItem::Item(item_ast) => self.visit_item(item_ast),
            ast::DefItem::ForeignItem(item_ast) => self.visit_foreign_item(item_ast),
            ast::DefItem::AssocItem(item_ast, assoc_ctx_ast) => self.visit_assoc_item(item_ast, assoc_ctx_ast),
        }
    }

    fn visit_item(&mut self, item: &'ast ast::Item) {
        let Some(&(node_hir, disambiguator)) = self.path_stack.front() else { return; };
        let Some(def_item_hir) = hir::DefItem::from_node(&node_hir) else { return; };

        let def_item_ast = match (def_item_hir, &item.kind) {
            (hir::DefItem::Item(_), ast::ItemKind::Mod(_, ast::ModKind::Loaded(items_ast, _, _))) => {
                let def_items_ast = items_ast.iter().map(ast::AstDeref::ast_deref).map(ast::DefItem::Item);
                find_hir_def_item_in_ast(def_item_hir, disambiguator, def_items_ast)
            }
            (hir::DefItem::ForeignItem(_), ast::ItemKind::ForeignMod(foreign_mod_ast)) => {
                let def_items_ast = foreign_mod_ast.items.iter().map(ast::AstDeref::ast_deref).map(ast::DefItem::ForeignItem);
                find_hir_def_item_in_ast(def_item_hir, disambiguator, def_items_ast)
            }
            (hir::DefItem::TraitItem(_), ast::ItemKind::Trait(trait_ast)) => {
                let def_items_ast = trait_ast.items.iter().map(ast::AstDeref::ast_deref)
                    .map(|item_ast| ast::DefItem::AssocItem(item_ast, ast::visit::AssocCtxt::Trait));
                find_hir_def_item_in_ast(def_item_hir, disambiguator, def_items_ast)
            }
            (hir::DefItem::ImplItem(_), ast::ItemKind::Impl(impl_ast)) => {
                let def_items_ast = impl_ast.items.iter().map(ast::AstDeref::ast_deref)
                    .map(|item_ast| ast::DefItem::AssocItem(item_ast, ast::visit::AssocCtxt::Impl));
                find_hir_def_item_in_ast(def_item_hir, disambiguator, def_items_ast)
            }
            (hir::DefItem::Item(_), ast::ItemKind::Fn(fn_ast)) => {
                let Some(body_ast) = &fn_ast.body else { return; };
                find_hir_def_item_in_ast(def_item_hir, disambiguator, ast_body_child_items(body_ast))
            }
            _ => { return; }
        };

        let Some(def_item_ast) = def_item_ast else { return; };
        self.path_stack.pop_front();

        if self.path_stack.is_empty() {
            self.result = Some(def_item_ast);
            return;
        }

        match def_item_ast {
            ast::DefItem::Item(item_ast) => self.visit_item(item_ast),
            ast::DefItem::ForeignItem(item_ast) => self.visit_foreign_item(item_ast),
            ast::DefItem::AssocItem(item_ast, assoc_ctx_ast) => self.visit_assoc_item(item_ast, assoc_ctx_ast),
        }
    }

    fn visit_foreign_item(&mut self, item: &'ast ast::ForeignItem) {
        let Some(&(node_hir, disambiguator)) = self.path_stack.front() else { return; };
        let Some(def_item_hir) = hir::DefItem::from_node(&node_hir) else { return; };

        let def_item_ast = match (def_item_hir, &item.kind) {
            (hir::DefItem::Item(_), ast::ForeignItemKind::Fn(fn_ast)) => {
                let Some(body_ast) = &fn_ast.body else { return; };
                find_hir_def_item_in_ast(def_item_hir, disambiguator, ast_body_child_items(body_ast))
            }
            _ => { return; }
        };

        let Some(def_item_ast) = def_item_ast else { return; };
        self.path_stack.pop_front();

        if self.path_stack.is_empty() {
            self.result = Some(def_item_ast);
            return;
        }

        match def_item_ast {
            ast::DefItem::Item(item_ast) => self.visit_item(item_ast),
            ast::DefItem::ForeignItem(item_ast) => self.visit_foreign_item(item_ast),
            ast::DefItem::AssocItem(item_ast, assoc_ctx_ast) => self.visit_assoc_item(item_ast, assoc_ctx_ast),
        }
    }

    fn visit_assoc_item(&mut self, item: &'ast ast::AssocItem, _ctx: ast::visit::AssocCtxt) {
        let Some(&(node_hir, disambiguator)) = self.path_stack.front() else { return; };
        let Some(def_item_hir) = hir::DefItem::from_node(&node_hir) else { return; };

        let def_item_ast = match (def_item_hir, &item.kind) {
            (hir::DefItem::Item(_), ast::AssocItemKind::Fn(fn_ast)) => {
                let Some(body_ast) = &fn_ast.body else { return; };
                find_hir_def_item_in_ast(def_item_hir, disambiguator, ast_body_child_items(body_ast))
            }
            _ => { return; }
        };

        let Some(def_item_ast) = def_item_ast else { return; };
        self.path_stack.pop_front();

        if self.path_stack.is_empty() {
            self.result = Some(def_item_ast);
            return;
        }

        match def_item_ast {
            ast::DefItem::Item(item_ast) => self.visit_item(item_ast),
            ast::DefItem::ForeignItem(item_ast) => self.visit_foreign_item(item_ast),
            ast::DefItem::AssocItem(item_ast, assoc_ctx_ast) => self.visit_assoc_item(item_ast, assoc_ctx_ast),
        }
    }
}

pub fn find_def_in_ast<'ast, 'tcx>(tcx: TyCtxt<'tcx>, def_id: hir::LocalDefId, krate: &'ast ast::Crate) -> Option<ast::DefItem<'ast>> {
    use ast::visit::Visitor;

    // The definition HIR path will contain the full HIR tree walk, including blocks, statements, expressions, etc.
    // (e.g. for items nested inside function bodies). However, it is sufficient to only look at the item-like path
    // components to resolve definitions. This is also how the compiler's own `DefPath`s are defined.
    let def_hir_item_node_path = res::def_hir_path(tcx, def_id).into_iter()
        .map(|(_, node)| node)
        .filter(|node| matches!(node, hir::Node::Crate(_)) || hir::DefItem::from_node(node).is_some())
        .collect::<Vec<_>>();

    let disambiguators = iter::once(None).chain(disambiguate_hir_def_item_node_path_components(tcx, &def_hir_item_node_path));
    let path_stack = iter::zip(def_hir_item_node_path, disambiguators).collect::<VecDeque<_>>();

    let mut finder = AstDefFinder {
        path_stack,
        result: None,
    };
    finder.visit_crate(krate);

    finder.result
}
