use mutest_emit::{Mutation, Operator};
use mutest_emit::analysis::hir;
use mutest_emit::analysis::res;
use mutest_emit::analysis::ty::{self, Ty, TyCtxt};
use mutest_emit::codegen::ast;
use mutest_emit::codegen::mutation::{MutCtxt, MutLoc, Subst, SubstDef, SubstLoc};
use mutest_emit::codegen::symbols::sym;
use mutest_emit::smallvec::{SmallVec, smallvec};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpKind {
    Standalone,
    Assign,
}

impl OpKind {
    pub fn desc(&self) -> &str {
        match self {
            Self::Standalone => "operator",
            Self::Assign => "assignment operator",
        }
    }
}

fn impls_matching_op<'tcx>(tcx: TyCtxt<'tcx>, param_env: ty::ParamEnv<'tcx>, lhs_ty: Ty<'tcx>, rhs_ty: Ty<'tcx>, expr_ty: Ty<'tcx>, op_trait: hir::DefId, op_kind: OpKind) -> bool {
    if !ty::impls_trait_with_env(tcx, param_env, lhs_ty, op_trait, vec![rhs_ty.into()]) { return false; }

    match op_kind {
        OpKind::Standalone => {
            ty::impl_assoc_ty(tcx, param_env, lhs_ty, op_trait, vec![rhs_ty.into()], sym::Output)
                .map(|ty| ty == expr_ty).unwrap_or(false)
        }
        OpKind::Assign => true,
    }
}

macro define_op_swap_operator(
    $(#[$meta:meta])*
    $vis:vis $operator:ident, $mutation:ident $([$bin_op_group:expr])? {
        $($bin_op_from:pat $(if impl $bin_op_to_trait:ident, $bin_assign_op_to_trait:ident)? => $bin_op_to:expr),+ $(,)?
    }
) {
    $vis struct $mutation {
        pub op_kind: OpKind,
        pub original_bin_op: ast::BinOpKind,
        pub replacement_bin_op: ast::BinOpKind,
    }

    impl Mutation for $mutation {
        fn display_name(&self) -> String {
            format!(concat!("swap ", $($bin_op_group, " ",)? "{op_kind} `{original_bin_op}` for `{replacement_bin_op}`"),
                op_kind = self.op_kind.desc(),
                original_bin_op = self.original_bin_op.to_string(),
                replacement_bin_op = self.replacement_bin_op.to_string(),
            )
        }

        fn span_label(&self) -> String {
            format!(concat!("swap ", $($bin_op_group, " ",)? "{op_kind} for `{replacement_bin_op}`"),
                op_kind = self.op_kind.desc(),
                replacement_bin_op = self.replacement_bin_op.to_string(),
            )
        }
    }

    $(#[$meta])*
    $vis struct $operator;

    impl<'a> Operator<'a> for $operator {
        type Mutation = $mutation;

        fn try_apply(&self, mcx: &MutCtxt) -> Option<(Self::Mutation, SmallVec<[SubstDef; 1]>)> {
            let MutCtxt { tcx, resolutions: _, def_site: def, ref location } = *mcx;

            let MutLoc::FnBodyExpr(expr, f) = location else { return None; };

            let (bin_op, op_kind) = match &expr.ast.kind {
                ast::ExprKind::Binary(bin_op, _, _) => (bin_op.node, OpKind::Standalone),
                ast::ExprKind::AssignOp(bin_op, _, _) => (bin_op.node, OpKind::Assign),
                _ => { return None; }
            };

            let param_env = tcx.param_env(f.hir.def_id);
            let typeck = tcx.typeck_body(f.hir.body.id());

            let expr_ty = typeck.expr_ty(expr.hir);
            let (lhs_ty, rhs_ty) = match expr.hir.kind {
                | hir::ExprKind::Binary(_, lhs, rhs)
                | hir::ExprKind::AssignOp(_, lhs, rhs) => {
                    (typeck.expr_ty(lhs), typeck.expr_ty(rhs))
                }
                _ => unreachable!(),
            };

            #[allow(unused_variables)]
            let expr_impls_matching_op = |op_trait| impls_matching_op(tcx, param_env, lhs_ty, rhs_ty, expr_ty, op_trait, op_kind);

            let mapped_bin_op = match (bin_op, op_kind) {
                $(
                    ($bin_op_from, OpKind::Standalone) $(if expr_impls_matching_op(res::traits::$bin_op_to_trait(tcx)))? => $bin_op_to,
                    ($bin_op_from, OpKind::Assign) $(if expr_impls_matching_op(res::traits::$bin_assign_op_to_trait(tcx)))? => $bin_op_to,
                )+
                _ => { return None; }
            };

            let mapped_bin_expr = match &expr.ast.kind {
                ast::ExprKind::Binary(_, lhs, rhs) => ast::mk::expr_binary(def, mapped_bin_op, lhs.clone(), rhs.clone()),
                ast::ExprKind::AssignOp(_, lhs, rhs) => ast::mk::expr_assign_op(def, mapped_bin_op, lhs.clone(), rhs.clone()),
                _ => unreachable!(),
            };

            let mutation = Self::Mutation {
                op_kind,
                original_bin_op: bin_op,
                replacement_bin_op: mapped_bin_op,
            };

            Some((mutation, smallvec![
                SubstDef::new(
                    SubstLoc::Replace(expr.ast.id),
                    Subst::AstExpr(mapped_bin_expr.into_inner()),
                ),
            ]))
        }
    }
}

define_op_swap_operator! {
    /// Swap addition for subtraction and vice versa.
    pub OpAddSubSwap, OpAddSubSwapMutation {
        ast::BinOpKind::Add if impl Sub, SubAssign => ast::BinOpKind::Sub,
        ast::BinOpKind::Sub if impl Add, AddAssign => ast::BinOpKind::Add,
    }
}

define_op_swap_operator! {
    /// Swap addition for multiplication and vice versa.
    pub OpAddMulSwap, OpAddMulSwapMutation {
        ast::BinOpKind::Add if impl Mul, MulAssign => ast::BinOpKind::Mul,
        ast::BinOpKind::Mul if impl Add, AddAssign => ast::BinOpKind::Add,
    }
}

define_op_swap_operator! {
    /// Swap multiplication for division and vice versa.
    pub OpMulDivSwap, OpMulDivSwapMutation {
        ast::BinOpKind::Mul if impl Div, DivAssign => ast::BinOpKind::Div,
        ast::BinOpKind::Div if impl Mul, MulAssign => ast::BinOpKind::Mul,
    }
}

define_op_swap_operator! {
    /// Swap division for modulus and vice versa.
    pub OpDivRemSwap, OpDivRemSwapMutation {
        ast::BinOpKind::Div if impl Rem, RemAssign => ast::BinOpKind::Rem,
        ast::BinOpKind::Rem if impl Div, DivAssign => ast::BinOpKind::Div,
    }
}

define_op_swap_operator! {
    /// Swap bitwise OR for bitwise XOR and vice versa.
    pub BitOpOrXorSwap, BitOpOrXorSwapMutation ["bitwise"] {
        ast::BinOpKind::BitOr if impl BitXor, BitXorAssign => ast::BinOpKind::BitXor,
        ast::BinOpKind::BitXor if impl BitOr, BitOrAssign => ast::BinOpKind::BitOr,
    }
}

define_op_swap_operator! {
    /// Swap bitwise OR for bitwise AND and vice versa.
    pub BitOpOrAndSwap, BitOpOrAndSwapMutation ["bitwise"] {
        ast::BinOpKind::BitOr if impl BitAnd, BitAndAssign => ast::BinOpKind::BitAnd,
        ast::BinOpKind::BitAnd if impl BitOr, BitOrAssign => ast::BinOpKind::BitOr,
    }
}

define_op_swap_operator! {
    /// Swap bitwise XOR for bitwise AND and vice versa.
    pub BitOpXorAndSwap, BitOpXorAndSwapMutation ["bitwise"] {
        ast::BinOpKind::BitXor if impl BitAnd, BitAndAssign => ast::BinOpKind::BitAnd,
        ast::BinOpKind::BitAnd if impl BitXor, BitXorAssign => ast::BinOpKind::BitXor,
    }
}

define_op_swap_operator! {
    /// Swap the direction of bitwise shift operators.
    pub BitOpShiftDirSwap, BitOpShiftDirSwapMutation ["bitwise"] {
        ast::BinOpKind::Shl if impl Shr, ShrAssign => ast::BinOpKind::Shr,
        ast::BinOpKind::Shr if impl Shl, ShlAssign => ast::BinOpKind::Shl,
    }
}

define_op_swap_operator! {
    /// Swap logical && for logical || and vice versa.
    pub LogicalOpAndOrSwap, LogicalOpAndOrSwapMutation ["logical"] {
        ast::BinOpKind::And => ast::BinOpKind::Or,
        ast::BinOpKind::Or => ast::BinOpKind::And,
    }
}
