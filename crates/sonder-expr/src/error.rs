//! Error type shared by the lexer, parser, compiler and helpers.

use std::fmt;

/// A compile-time error in an expression.
///
/// `span` is a byte range `(start, end)` into the source string when the error can be
/// attributed to a location. Use [`ExprError::render`] to produce a multi-line message
/// with the offending source underlined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExprError {
    /// Human-readable description of the problem.
    pub message: String,
    /// Byte range `(start, end)` of the offending source, if known.
    pub span: Option<(usize, usize)>,
}

impl ExprError {
    /// Error with a span.
    pub fn at(span: (usize, usize), message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            span: Some(span),
        }
    }

    /// Error without a location.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            span: None,
        }
    }

    /// Return the same error with a prefix like `signals.density: ` prepended.
    pub fn with_context(mut self, context: &str) -> Self {
        self.message = format!("{context}: {}", self.message);
        self
    }

    /// Render the error together with the source it refers to and a caret underline.
    ///
    /// ```text
    /// unknown function `smoothe`
    ///   norm(smoothe(x, 2))
    ///        ^^^^^^^
    /// ```
    pub fn render(&self, src: &str) -> String {
        match self.span {
            Some((start, end)) if start <= end && end <= src.len() => {
                let width = src[start..end].chars().count().max(1);
                let lead = src[..start].chars().count();
                format!(
                    "{}\n  {}\n  {}{}",
                    self.message,
                    src,
                    " ".repeat(lead),
                    "^".repeat(width)
                )
            }
            _ => self.message.clone(),
        }
    }
}

impl fmt::Display for ExprError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.span {
            Some((s, e)) => write!(f, "{} (at {}..{})", self.message, s, e),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for ExprError {}
