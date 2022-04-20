use mutest_emit::{Mutation, Operator};
use mutest_emit::codegen::ast;
use mutest_emit::codegen::mutation::{MutCtxt, MutLoc, Subst, SubstDef, SubstLoc};
use smallvec::{SmallVec, smallvec};

pub struct RelationalOpInvertMutation {
    pub original_bin_op: ast::BinOpKind,
    pub replacement_bin_op: ast::BinOpKind,
}

impl Mutation for RelationalOpInvertMutation {
    fn display_name(&self) -> String {
        format!("invert relational operator `{original_bin_op}` for `{replacement_bin_op}`",
            original_bin_op = self.original_bin_op.to_string(),
            replacement_bin_op = self.replacement_bin_op.to_string(),
        )
    }
}

/// Invert relational operators, also chaning their boundary (equality).
pub struct RelationalOpInvert;

impl<'a> Operator<'a> for RelationalOpInvert {
    type Mutation = RelationalOpInvertMutation;

    fn try_apply(&self, mcx: &MutCtxt) -> Option<(Self::Mutation, SmallVec<[SubstDef; 1]>)> {
        let MutCtxt { tcx: _, resolver: _, def_site: def, ref location } = *mcx;

        let MutLoc::FnBodyExpr(expr, _) = location else { return None; };

        let ast::ExprKind::Binary(bin_op, lhs, rhs) = &expr.ast.kind else { return None; };

        let inverted_bin_op = match bin_op.node {
            ast::BinOpKind::Lt => ast::BinOpKind::Ge,
            ast::BinOpKind::Le => ast::BinOpKind::Gt,
            ast::BinOpKind::Gt => ast::BinOpKind::Le,
            ast::BinOpKind::Ge => ast::BinOpKind::Lt,
            _ => { return None; },
        };
        let inverted_bin_expr = ast::mk::expr_binary(def, inverted_bin_op, lhs.clone(), rhs.clone());

        let mutation = Self::Mutation {
            original_bin_op: bin_op.node,
            replacement_bin_op: inverted_bin_op,
        };

        Some((mutation, smallvec![
            SubstDef::new(
                SubstLoc::Replace(expr.ast.id),
                Subst::AstExpr(inverted_bin_expr.into_inner()),
            ),
        ]))
    }
}
