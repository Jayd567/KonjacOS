//! Text -> tokens. Shell-style: a word is everything up to whitespace or
//! one of `| ; ( ) [ ] { } , "`, so `*.txt`, `../docs` and `--reverse` are
//! each one word, and operators need spaces around them (`size > 5MB`).
//! What a word means is the parser's business.

use alloc::string::String;
use alloc::vec::Vec;

use crate::error::{ShellError, Span};

#[derive(Clone, Debug, PartialEq)]
pub enum Tok {
    Word(String),
    /// `"..."`: the text between the quotes, escapes not yet processed
    /// (the parser handles them along with `$var` and `( )`).
    DStr(String),
    /// `'...'`: taken literally.
    SStr(String),
    Pipe,
    Semi,
    Newline,
    LParen,
    RParen,
    LBracket,
    RBracket,
    LBrace,
    RBrace,
    Comma,
    Eof,
}

#[derive(Clone, Debug)]
pub struct Token {
    pub tok: Tok,
    pub span: Span,
    /// Whether whitespace (or the start of the text) came right before.
    pub spaced: bool,
}

fn is_delim(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\r' | b'\n' | b'|' | b';' | b'(' | b')' | b'[' | b']' | b'{' | b'}' | b',' | b'"')
}

/// Splits `src` into tokens. `base` is added to every position: the
/// parser lexes the `( )` inside a string with the string's offset, so
/// spans still point into the whole line.
pub fn lex(src: &str, src_id: u32, base: usize) -> Result<Vec<Token>, ShellError> {
    let b = src.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    let mut spaced = true;
    let span = |s: usize, e: usize| Span::new(src_id, base + s, base + e);
    while i < b.len() {
        let start = i;
        let tok = match b[i] {
            b' ' | b'\t' | b'\r' => {
                i += 1;
                spaced = true;
                continue;
            }
            b'#' if spaced => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'\n' => {
                i += 1;
                Tok::Newline
            }
            b'|' => {
                i += 1;
                Tok::Pipe
            }
            b';' => {
                i += 1;
                Tok::Semi
            }
            b'(' => {
                i += 1;
                Tok::LParen
            }
            b')' => {
                i += 1;
                Tok::RParen
            }
            b'[' => {
                i += 1;
                Tok::LBracket
            }
            b']' => {
                i += 1;
                Tok::RBracket
            }
            b'{' => {
                i += 1;
                Tok::LBrace
            }
            b'}' => {
                i += 1;
                Tok::RBrace
            }
            b',' => {
                i += 1;
                Tok::Comma
            }
            b'"' => {
                let mut j = i + 1;
                while j < b.len() && b[j] != b'"' {
                    j += if b[j] == b'\\' { 2 } else { 1 };
                }
                if j >= b.len() {
                    return Err(ShellError::at("this string has no closing \"", span(i, b.len())));
                }
                let text = String::from(&src[i + 1..j]);
                i = j + 1;
                Tok::DStr(text)
            }
            b'\'' => {
                let close = src[i + 1..].find('\'');
                match close {
                    Some(n) => {
                        let text = String::from(&src[i + 1..i + 1 + n]);
                        i = i + 2 + n;
                        Tok::SStr(text)
                    }
                    None => return Err(ShellError::at("this string has no closing '", span(i, b.len()))),
                }
            }
            _ => {
                while i < b.len() && !is_delim(b[i]) {
                    i += 1;
                }
                Tok::Word(String::from(&src[start..i]))
            }
        };
        toks.push(Token { tok, span: span(start, i), spaced });
        spaced = false;
    }
    toks.push(Token { tok: Tok::Eof, span: span(b.len(), b.len()), spaced: true });
    Ok(toks)
}
