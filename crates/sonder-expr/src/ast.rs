//! Abstract syntax tree produced by the parser.

use crate::lexer::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Pow,
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
    And,
    Or,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::Pow => "^",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::And => "&&",
            BinOp::Or => "||",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Num { value: f64, span: Span },
    Str { value: String, span: Span },
    Ident { name: String, span: Span },
    Call { name: String, name_span: Span, args: Vec<Expr>, span: Span },
    Unary { op: UnOp, rhs: Box<Expr>, span: Span },
    Binary { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr>, span: Span },
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Num { span, .. }
            | Expr::Str { span, .. }
            | Expr::Ident { span, .. }
            | Expr::Call { span, .. }
            | Expr::Unary { span, .. }
            | Expr::Binary { span, .. } => *span,
        }
    }

    /// Visit every node depth-first (pre-order).
    pub fn walk<'a>(&'a self, f: &mut dyn FnMut(&'a Expr)) {
        f(self);
        match self {
            Expr::Call { args, .. } => args.iter().for_each(|a| a.walk(f)),
            Expr::Unary { rhs, .. } => rhs.walk(f),
            Expr::Binary { lhs, rhs, .. } => {
                lhs.walk(f);
                rhs.walk(f);
            }
            _ => {}
        }
    }
}
