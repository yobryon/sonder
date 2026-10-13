//! Recursive-descent parser for the expression grammar (see crate docs).

use crate::ast::{BinOp, Expr, UnOp};
use crate::error::ExprError;
use crate::lexer::{lex, Span, Tok, Token};

/// Parse a complete expression. Trailing tokens are an error.
pub fn parse(src: &str) -> Result<Expr, ExprError> {
    if src.trim().is_empty() {
        return Err(ExprError::at((0, src.len()), "empty expression"));
    }
    let tokens = lex(src)?;
    let mut p = Parser { tokens, pos: 0 };
    let e = p.expr()?;
    match p.peek() {
        Tok::Eof => Ok(e),
        other => Err(ExprError::at(
            p.span(),
            format!("unexpected {} after expression", describe(other)),
        )),
    }
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

fn describe(t: &Tok) -> String {
    match t {
        Tok::Num(n) => format!("number `{n}`"),
        Tok::Str(s) => format!("string `{s}`"),
        Tok::Ident(s) => format!("identifier `{s}`"),
        Tok::Eof => "end of input".to_string(),
        Tok::LParen => "`(`".into(),
        Tok::RParen => "`)`".into(),
        Tok::Comma => "`,`".into(),
        Tok::Plus => "`+`".into(),
        Tok::Minus => "`-`".into(),
        Tok::Star => "`*`".into(),
        Tok::Slash => "`/`".into(),
        Tok::Percent => "`%`".into(),
        Tok::Caret => "`^`".into(),
        Tok::Bang => "`!`".into(),
        Tok::Lt => "`<`".into(),
        Tok::Le => "`<=`".into(),
        Tok::Gt => "`>`".into(),
        Tok::Ge => "`>=`".into(),
        Tok::EqEq => "`==`".into(),
        Tok::Ne => "`!=`".into(),
        Tok::AndAnd => "`&&`".into(),
        Tok::OrOr => "`||`".into(),
    }
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.tokens[self.pos].tok
    }
    fn span(&self) -> Span {
        self.tokens[self.pos].span
    }
    fn bump(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        t
    }
    fn expect(&mut self, want: Tok, what: &str) -> Result<Span, ExprError> {
        if *self.peek() == want {
            Ok(self.bump().span)
        } else {
            Err(ExprError::at(
                self.span(),
                format!("expected {what}, found {}", describe(self.peek())),
            ))
        }
    }

    fn expr(&mut self) -> Result<Expr, ExprError> {
        self.or()
    }

    fn or(&mut self) -> Result<Expr, ExprError> {
        let mut lhs = self.and()?;
        while *self.peek() == Tok::OrOr {
            self.bump();
            let rhs = self.and()?;
            lhs = binary(BinOp::Or, lhs, rhs);
        }
        Ok(lhs)
    }

    fn and(&mut self) -> Result<Expr, ExprError> {
        let mut lhs = self.cmp()?;
        while *self.peek() == Tok::AndAnd {
            self.bump();
            let rhs = self.cmp()?;
            lhs = binary(BinOp::And, lhs, rhs);
        }
        Ok(lhs)
    }

    fn cmp(&mut self) -> Result<Expr, ExprError> {
        let lhs = self.add()?;
        let op = match self.peek() {
            Tok::Lt => BinOp::Lt,
            Tok::Le => BinOp::Le,
            Tok::Gt => BinOp::Gt,
            Tok::Ge => BinOp::Ge,
            Tok::EqEq => BinOp::Eq,
            Tok::Ne => BinOp::Ne,
            _ => return Ok(lhs),
        };
        self.bump();
        let rhs = self.add()?;
        let e = binary(op, lhs, rhs);
        if matches!(
            self.peek(),
            Tok::Lt | Tok::Le | Tok::Gt | Tok::Ge | Tok::EqEq | Tok::Ne
        ) {
            return Err(ExprError::at(
                self.span(),
                "comparisons cannot be chained; use `&&` (e.g. `a < b && b < c`)",
            ));
        }
        Ok(e)
    }

    fn add(&mut self) -> Result<Expr, ExprError> {
        let mut lhs = self.mul()?;
        loop {
            let op = match self.peek() {
                Tok::Plus => BinOp::Add,
                Tok::Minus => BinOp::Sub,
                _ => return Ok(lhs),
            };
            self.bump();
            let rhs = self.mul()?;
            lhs = binary(op, lhs, rhs);
        }
    }

    fn mul(&mut self) -> Result<Expr, ExprError> {
        let mut lhs = self.unary()?;
        loop {
            let op = match self.peek() {
                Tok::Star => BinOp::Mul,
                Tok::Slash => BinOp::Div,
                Tok::Percent => BinOp::Rem,
                _ => return Ok(lhs),
            };
            self.bump();
            let rhs = self.unary()?;
            lhs = binary(op, lhs, rhs);
        }
    }

    fn unary(&mut self) -> Result<Expr, ExprError> {
        let op = match self.peek() {
            Tok::Minus => UnOp::Neg,
            Tok::Bang => UnOp::Not,
            _ => return self.pow(),
        };
        let start = self.bump().span.0;
        let rhs = self.unary()?;
        let span = (start, rhs.span().1);
        Ok(Expr::Unary {
            op,
            rhs: Box::new(rhs),
            span,
        })
    }

    fn pow(&mut self) -> Result<Expr, ExprError> {
        let base = self.atom()?;
        if *self.peek() == Tok::Caret {
            self.bump();
            let exp = self.unary()?;
            return Ok(binary(BinOp::Pow, base, exp));
        }
        Ok(base)
    }

    fn atom(&mut self) -> Result<Expr, ExprError> {
        let t = self.bump();
        match t.tok {
            Tok::Num(value) => Ok(Expr::Num {
                value,
                span: t.span,
            }),
            Tok::Str(value) => Ok(Expr::Str {
                value,
                span: t.span,
            }),
            Tok::Ident(name) => {
                if *self.peek() == Tok::LParen {
                    self.bump();
                    let mut args = Vec::new();
                    if *self.peek() != Tok::RParen {
                        loop {
                            args.push(self.expr()?);
                            match self.peek() {
                                Tok::Comma => {
                                    self.bump();
                                }
                                Tok::RParen => break,
                                other => {
                                    return Err(ExprError::at(
                                        self.span(),
                                        format!(
                                        "expected `,` or `)` in arguments of `{name}`, found {}",
                                        describe(other)
                                    ),
                                    ))
                                }
                            }
                        }
                    }
                    let end = self.expect(Tok::RParen, "`)`")?.1;
                    Ok(Expr::Call {
                        name,
                        name_span: t.span,
                        args,
                        span: (t.span.0, end),
                    })
                } else {
                    Ok(Expr::Ident { name, span: t.span })
                }
            }
            Tok::LParen => {
                let inner = self.expr()?;
                let end = self.expect(Tok::RParen, "`)`")?.1;
                // Keep the inner node but widen its span to include the parens.
                Ok(widen(inner, (t.span.0, end)))
            }
            Tok::Eof => Err(ExprError::at(
                t.span,
                "unexpected end of input; expected a value",
            )),
            other => Err(ExprError::at(
                t.span,
                format!("expected a value, found {}", describe(&other)),
            )),
        }
    }
}

fn binary(op: BinOp, lhs: Expr, rhs: Expr) -> Expr {
    let span = (lhs.span().0, rhs.span().1);
    Expr::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
        span,
    }
}

fn widen(e: Expr, span: Span) -> Expr {
    match e {
        Expr::Num { value, .. } => Expr::Num { value, span },
        Expr::Str { value, .. } => Expr::Str { value, span },
        Expr::Ident { name, .. } => Expr::Ident { name, span },
        Expr::Call {
            name,
            name_span,
            args,
            ..
        } => Expr::Call {
            name,
            name_span,
            args,
            span,
        },
        Expr::Unary { op, rhs, .. } => Expr::Unary { op, rhs, span },
        Expr::Binary { op, lhs, rhs, .. } => Expr::Binary { op, lhs, rhs, span },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn show(e: &Expr) -> String {
        match e {
            Expr::Num { value, .. } => format!("{value}"),
            Expr::Str { value, .. } => format!("{value:?}"),
            Expr::Ident { name, .. } => name.clone(),
            Expr::Call { name, args, .. } => {
                format!(
                    "{name}({})",
                    args.iter().map(show).collect::<Vec<_>>().join(", ")
                )
            }
            Expr::Unary { op, rhs, .. } => {
                format!(
                    "({}{})",
                    if *op == UnOp::Neg { "-" } else { "!" },
                    show(rhs)
                )
            }
            Expr::Binary { op, lhs, rhs, .. } => {
                format!("({} {} {})", show(lhs), op.symbol(), show(rhs))
            }
        }
    }

    #[test]
    fn precedence() {
        assert_eq!(show(&parse("1 + 2 * 3").unwrap()), "(1 + (2 * 3))");
        assert_eq!(show(&parse("1 - 2 - 3").unwrap()), "((1 - 2) - 3)");
        assert_eq!(show(&parse("2 ^ 3 ^ 2").unwrap()), "(2 ^ (3 ^ 2))");
        assert_eq!(show(&parse("-2 ^ 2").unwrap()), "(-(2 ^ 2))");
        assert_eq!(
            show(&parse("a < b && c || !d").unwrap()),
            "(((a < b) && c) || (!d))"
        );
        assert_eq!(show(&parse("(1 + 2) * 3").unwrap()), "((1 + 2) * 3)");
        assert_eq!(show(&parse("a % 2 == 0").unwrap()), "((a % 2) == 0)");
    }

    #[test]
    fn calls_and_strings() {
        assert_eq!(
            show(&parse("norm(smooth(rate(event('car'), 10s), 3))").unwrap()),
            "norm(smooth(rate(event(\"car\"), 10), 3))"
        );
        assert_eq!(show(&parse("random()").unwrap()), "random()");
        assert_eq!(
            show(&parse("attr_text(\"k\") == 'x'").unwrap()),
            "(attr_text(\"k\") == \"x\")"
        );
    }

    #[test]
    fn spans() {
        let e = parse("  foo(1, 2) ").unwrap();
        assert_eq!(e.span(), (2, 11));
        let e = parse("(a + b)").unwrap();
        assert_eq!(e.span(), (0, 7));
    }

    #[test]
    fn errors() {
        let e = parse("1 +").unwrap_err();
        assert!(e.message.contains("end of input"), "{e}");
        let e = parse("f(1 2)").unwrap_err();
        assert!(e.message.contains("expected `,` or `)`"), "{e}");
        assert_eq!(e.span, Some((4, 5)));
        let e = parse("1 2").unwrap_err();
        assert!(e.message.contains("after expression"), "{e}");
        let e = parse("a < b < c").unwrap_err();
        assert!(e.message.contains("chained"), "{e}");
        assert!(parse("").unwrap_err().message.contains("empty"));
        assert!(parse("(1").unwrap_err().message.contains("expected `)`"));
        assert!(parse(")").unwrap_err().message.contains("expected a value"));
    }
}
