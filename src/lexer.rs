use crate::Error;

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    Regex(String),
    Ident(String),
    Fn,
    Type,
    Be,
    Mut,
    If,
    Else,
    While,
    Return,
    True,
    False,
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Colon,
    Semi,
    Arrow,
    DotDot,
    Dot,
    Backslash,
    Assign,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Bang,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
    Pipe,
    Eof,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    pub line: usize,
}

pub fn lex(src: &str) -> Result<Vec<Token>, Error> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    let mut line = 1;

    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            i += 1;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '"' {
            let start_line = line;
            let mut bytes = Vec::new();
            i += 1;
            loop {
                let Some(&c) = chars.get(i) else {
                    return Err(Error::at(start_line, "unterminated string literal"));
                };
                i += 1;
                match c {
                    '"' => break,
                    '\\' => {
                        let esc = chars.get(i).copied();
                        i += 1;
                        let byte = match esc {
                            Some('n') => b'\n',
                            Some('t') => b'\t',
                            Some('r') => b'\r',
                            Some('0') => 0,
                            Some('\\') => b'\\',
                            Some('"') => b'"',
                            Some('x') => {
                                let hex: String = chars.get(i..i + 2).unwrap_or(&[]).iter().collect();
                                i += 2;
                                u8::from_str_radix(&hex, 16)
                                    .map_err(|_| Error::at(line, format!("invalid escape `\\x{hex}`")))?
                            }
                            other => {
                                let shown = other.map(String::from).unwrap_or_default();
                                return Err(Error::at(line, format!("unknown escape `\\{shown}`")));
                            }
                        };
                        bytes.push(byte);
                    }
                    c => {
                        if c == '\n' {
                            line += 1;
                        }
                        let mut buf = [0; 4];
                        bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                    }
                }
            }
            out.push(Token { tok: Tok::Str(bytes), line: start_line });
            continue;
        }
        if c == '`' {
            let start_line = line;
            let mut pattern = String::new();
            i += 1;
            loop {
                match chars.get(i) {
                    None => return Err(Error::at(start_line, "unterminated regex literal")),
                    Some('`') => break,
                    Some('\\') if chars.get(i + 1) == Some(&'`') => {
                        pattern.push('`');
                        i += 1;
                    }
                    Some(&c) => {
                        if c == '\n' {
                            line += 1;
                        }
                        pattern.push(c);
                    }
                }
                i += 1;
            }
            i += 1;
            out.push(Token { tok: Tok::Regex(pattern), line: start_line });
            continue;
        }
        if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '_') {
                i += 1;
            }
            let digit_at = |k: usize| chars.get(k).is_some_and(|c| c.is_ascii_digit());
            let mut is_float = false;
            // `1.5` is a float; `1..5` is a range and `1.` is not a number.
            if chars.get(i) == Some(&'.') && digit_at(i + 1) {
                is_float = true;
                i += 1;
                while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '_') {
                    i += 1;
                }
            }
            if matches!(chars.get(i), Some('e' | 'E')) {
                let sign = matches!(chars.get(i + 1), Some('+' | '-')) as usize;
                if digit_at(i + 1 + sign) {
                    is_float = true;
                    i += 1 + sign;
                    while i < chars.len() && chars[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            let text: String = chars[start..i].iter().filter(|&&c| c != '_').collect();
            let tok = if is_float {
                Tok::Float(text.parse::<f64>().map_err(|_| Error::at(line, format!("invalid float literal `{text}`")))?)
            } else {
                Tok::Int(
                    text.parse::<i64>()
                        .map_err(|_| Error::at(line, format!("integer literal `{text}` is too large")))?,
                )
            };
            out.push(Token { tok, line });
            continue;
        }
        if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let tok = match word.as_str() {
                "fn" => Tok::Fn,
                "type" => Tok::Type,
                "be" => Tok::Be,
                "mut" => Tok::Mut,
                "if" => Tok::If,
                "else" => Tok::Else,
                "while" => Tok::While,
                "return" => Tok::Return,
                "true" => Tok::True,
                "false" => Tok::False,
                _ => Tok::Ident(word),
            };
            out.push(Token { tok, line });
            continue;
        }

        let next = chars.get(i + 1).copied();
        let (tok, width) = match (c, next) {
            ('-', Some('>')) => (Tok::Arrow, 2),
            ('.', Some('.')) => (Tok::DotDot, 2),
            ('=', Some('=')) => (Tok::EqEq, 2),
            ('!', Some('=')) => (Tok::NotEq, 2),
            ('<', Some('=')) => (Tok::Le, 2),
            ('>', Some('=')) => (Tok::Ge, 2),
            ('&', Some('&')) => (Tok::AndAnd, 2),
            ('|', Some('|')) => (Tok::OrOr, 2),
            ('|', _) => (Tok::Pipe, 1),
            ('\\', _) => (Tok::Backslash, 1),
            ('.', _) => (Tok::Dot, 1),
            ('(', _) => (Tok::LParen, 1),
            (')', _) => (Tok::RParen, 1),
            ('{', _) => (Tok::LBrace, 1),
            ('}', _) => (Tok::RBrace, 1),
            ('[', _) => (Tok::LBracket, 1),
            (']', _) => (Tok::RBracket, 1),
            (',', _) => (Tok::Comma, 1),
            (':', _) => (Tok::Colon, 1),
            (';', _) => (Tok::Semi, 1),
            ('=', _) => (Tok::Assign, 1),
            ('+', _) => (Tok::Plus, 1),
            ('-', _) => (Tok::Minus, 1),
            ('*', _) => (Tok::Star, 1),
            ('/', _) => (Tok::Slash, 1),
            ('%', _) => (Tok::Percent, 1),
            ('!', _) => (Tok::Bang, 1),
            ('<', _) => (Tok::Lt, 1),
            ('>', _) => (Tok::Gt, 1),
            _ => return Err(Error::at(line, format!("unexpected character `{c}`"))),
        };
        out.push(Token { tok, line });
        i += width;
    }

    out.push(Token { tok: Tok::Eof, line });
    Ok(out)
}
