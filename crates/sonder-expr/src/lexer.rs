//! Tokenizer. Produces a flat token list with byte spans.

use crate::error::ExprError;

/// Byte range `(start, end)` into the source.
pub type Span = (usize, usize);

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// Numeric literal, already converted to seconds if it carried a unit suffix.
    Num(f64),
    /// String literal (quotes stripped).
    Str(String),
    Ident(String),
    LParen,
    RParen,
    Comma,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Caret,
    Bang,
    Lt,
    Le,
    Gt,
    Ge,
    EqEq,
    Ne,
    AndAnd,
    OrOr,
    /// End of input (always the last token).
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
}

/// Tokenize `src`. Whitespace is skipped; `#` starts a comment to end of line.
pub fn lex(src: &str) -> Result<Vec<Token>, ExprError> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let start = i;
        match c {
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
            }
            b'#' => {
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
            }
            b'(' => push(&mut out, Tok::LParen, start, &mut i, 1),
            b')' => push(&mut out, Tok::RParen, start, &mut i, 1),
            b',' => push(&mut out, Tok::Comma, start, &mut i, 1),
            b'+' => push(&mut out, Tok::Plus, start, &mut i, 1),
            b'-' => push(&mut out, Tok::Minus, start, &mut i, 1),
            b'*' => push(&mut out, Tok::Star, start, &mut i, 1),
            b'/' => push(&mut out, Tok::Slash, start, &mut i, 1),
            b'%' => push(&mut out, Tok::Percent, start, &mut i, 1),
            b'^' => push(&mut out, Tok::Caret, start, &mut i, 1),
            b'<' if bytes.get(i + 1) == Some(&b'=') => push(&mut out, Tok::Le, start, &mut i, 2),
            b'<' => push(&mut out, Tok::Lt, start, &mut i, 1),
            b'>' if bytes.get(i + 1) == Some(&b'=') => push(&mut out, Tok::Ge, start, &mut i, 2),
            b'>' => push(&mut out, Tok::Gt, start, &mut i, 1),
            b'=' if bytes.get(i + 1) == Some(&b'=') => push(&mut out, Tok::EqEq, start, &mut i, 2),
            b'=' => return Err(ExprError::at((start, start + 1), "single `=`; use `==` for comparison")),
            b'!' if bytes.get(i + 1) == Some(&b'=') => push(&mut out, Tok::Ne, start, &mut i, 2),
            b'!' => push(&mut out, Tok::Bang, start, &mut i, 1),
            b'&' if bytes.get(i + 1) == Some(&b'&') => push(&mut out, Tok::AndAnd, start, &mut i, 2),
            b'&' => return Err(ExprError::at((start, start + 1), "single `&`; use `&&` for logical and")),
            b'|' if bytes.get(i + 1) == Some(&b'|') => push(&mut out, Tok::OrOr, start, &mut i, 2),
            b'|' => return Err(ExprError::at((start, start + 1), "single `|`; use `||` for logical or")),
            b'\'' | b'"' => {
                let quote = c;
                i += 1;
                let mut s = String::new();
                loop {
                    match bytes.get(i) {
                        None => return Err(ExprError::at((start, bytes.len()), "unterminated string literal")),
                        Some(&b) if b == quote => {
                            i += 1;
                            break;
                        }
                        Some(b'\\') => {
                            match bytes.get(i + 1) {
                                Some(&e @ (b'\\' | b'\'' | b'"')) => s.push(e as char),
                                Some(b'n') => s.push('\n'),
                                Some(b't') => s.push('\t'),
                                _ => return Err(ExprError::at((i, (i + 2).min(bytes.len())), "unknown escape in string")),
                            }
                            i += 2;
                        }
                        Some(_) => {
                            // Copy one whole UTF-8 character.
                            let ch = src[i..].chars().next().unwrap_or('\u{FFFD}');
                            s.push(ch);
                            i += ch.len_utf8();
                        }
                    }
                }
                out.push(Token { tok: Tok::Str(s), span: (start, i) });
            }
            b'0'..=b'9' | b'.' => {
                if c == b'.' && !bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
                    return Err(ExprError::at((start, start + 1), "unexpected `.`"));
                }
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'.' {
                    i += 1;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                let num_end = i;
                let mut value: f64 = src[start..num_end]
                    .parse()
                    .map_err(|_| ExprError::at((start, num_end), format!("bad number `{}`", &src[start..num_end])))?;
                // Unit suffix: letters immediately following the digits.
                while i < bytes.len() && (bytes[i].is_ascii_alphabetic() || bytes[i] == b'_') {
                    i += 1;
                }
                let suffix = &src[num_end..i];
                value *= match suffix {
                    "" => 1.0,
                    "s" => 1.0,
                    "ms" => 0.001,
                    "m" | "min" => 60.0,
                    "h" => 3600.0,
                    other => {
                        return Err(ExprError::at(
                            (num_end, i),
                            format!("unknown unit suffix `{other}` (use `ms`, `s`, `m`, `h`)"),
                        ))
                    }
                };
                out.push(Token { tok: Tok::Num(value), span: (start, i) });
            }
            b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_' || bytes[i] == b'.') {
                    i += 1;
                }
                out.push(Token { tok: Tok::Ident(src[start..i].to_string()), span: (start, i) });
            }
            _ => {
                let ch = src[i..].chars().next().unwrap_or('?');
                return Err(ExprError::at((start, start + ch.len_utf8()), format!("unexpected character `{ch}`")));
            }
        }
    }
    out.push(Token { tok: Tok::Eof, span: (bytes.len(), bytes.len()) });
    Ok(out)
}

fn push(out: &mut Vec<Token>, tok: Tok, start: usize, i: &mut usize, len: usize) {
    out.push(Token { tok, span: (start, start + len) });
    *i += len;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(src: &str) -> Vec<Tok> {
        lex(src).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn lexes_operators_and_numbers() {
        assert_eq!(
            toks("a <= 1.5 && b != 'x' || !c ^ 2"),
            vec![
                Tok::Ident("a".into()),
                Tok::Le,
                Tok::Num(1.5),
                Tok::AndAnd,
                Tok::Ident("b".into()),
                Tok::Ne,
                Tok::Str("x".into()),
                Tok::OrOr,
                Tok::Bang,
                Tok::Ident("c".into()),
                Tok::Caret,
                Tok::Num(2.0),
                Tok::Eof
            ]
        );
    }

    #[test]
    fn unit_suffixes() {
        assert_eq!(toks("10s")[0], Tok::Num(10.0));
        assert_eq!(toks("250ms")[0], Tok::Num(0.25));
        assert_eq!(toks("2m")[0], Tok::Num(120.0));
        assert_eq!(toks("1.5h")[0], Tok::Num(5400.0));
        assert_eq!(toks(".5")[0], Tok::Num(0.5));
        let e = lex("3x").unwrap_err();
        assert!(e.message.contains("unit suffix"));
        assert_eq!(e.span, Some((1, 2)));
    }

    #[test]
    fn strings_and_comments() {
        assert_eq!(toks("\"a b\" # comment\n 'c'"), vec![Tok::Str("a b".into()), Tok::Str("c".into()), Tok::Eof]);
        assert_eq!(toks(r#"'it\'s'"#)[0], Tok::Str("it's".into()));
        assert!(lex("'open").unwrap_err().message.contains("unterminated"));
    }

    #[test]
    fn dotted_identifiers_and_spans() {
        let t = lex("  foo.bar(1)").unwrap();
        assert_eq!(t[0].tok, Tok::Ident("foo.bar".into()));
        assert_eq!(t[0].span, (2, 9));
    }

    #[test]
    fn rejects_single_equals_and_junk() {
        assert!(lex("a = 1").unwrap_err().message.contains("`==`"));
        assert!(lex("a & b").unwrap_err().message.contains("`&&`"));
        assert!(lex("a | b").unwrap_err().message.contains("`||`"));
        assert!(lex("a $ b").unwrap_err().message.contains("unexpected character"));
    }
}
