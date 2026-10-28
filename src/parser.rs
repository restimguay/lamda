use std::rc::Rc;

use crate::ast::*;
use crate::lexer::{Tok, Token};
use crate::Error;

pub fn parse(tokens: Vec<Token>) -> Result<Program, Error> {
    let mut p = Parser { tokens, pos: 0 };
    let mut program = Program::default();
    while p.peek() != &Tok::Eof {
        if p.peek() == &Tok::Type {
            program.structs.push(p.struct_def()?);
        } else {
            program.funcs.push(p.func()?);
        }
    }
    Ok(program)
}

/// Desugars `value | rhs`: `x | f` is `f(x)`, `x | f(a)` is `f(x, a)`, and any other
/// right-hand side (a lambda, or anything in parentheses) is called with `x`.
fn pipe(value: Expr, rhs: Expr, parenthesized: bool, line: usize) -> Expr {
    let kind = match (parenthesized, rhs.kind) {
        (false, ExprKind::Var(name)) => ExprKind::Call(name, vec![value]),
        (false, ExprKind::Call(name, mut args)) => {
            args.insert(0, value);
            ExprKind::Call(name, args)
        }
        (false, ExprKind::CallExpr(callee, mut args)) => {
            args.insert(0, value);
            ExprKind::CallExpr(callee, args)
        }
        (_, kind) => ExprKind::CallExpr(Box::new(Expr { kind, line: rhs.line }), vec![value]),
    };
    Expr { kind, line }
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> &Tok {
        &self.tokens[self.pos].tok
    }

    fn line(&self) -> usize {
        self.tokens[self.pos].line
    }

    fn advance(&mut self) -> Tok {
        let tok = self.tokens[self.pos].tok.clone();
        if tok != Tok::Eof {
            self.pos += 1;
        }
        tok
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        if self.peek() == tok {
            self.advance();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, tok: Tok, what: &str) -> Result<(), Error> {
        if self.eat(&tok) {
            Ok(())
        } else {
            Err(self.unexpected(what))
        }
    }

    fn unexpected(&self, what: &str) -> Error {
        Error::at(self.line(), format!("expected {what}, found {:?}", self.peek()))
    }

    fn ident(&mut self) -> Result<String, Error> {
        match self.peek().clone() {
            Tok::Ident(name) => {
                self.advance();
                Ok(name)
            }
            _ => Err(self.unexpected("identifier")),
        }
    }

    /// `<A, B>` after a declaration's name, if present.
    fn type_params(&mut self) -> Result<Vec<String>, Error> {
        let mut params = Vec::new();
        if self.eat(&Tok::Lt) {
            while self.peek() != &Tok::Gt {
                params.push(self.ident()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::Gt, "`>`")?;
        }
        Ok(params)
    }

    fn ty(&mut self) -> Result<Type, Error> {
        if self.eat(&Tok::LBracket) {
            let elem = self.ty()?;
            self.expect(Tok::RBracket, "`]`")?;
            return Ok(Type::array(elem));
        }
        if self.eat(&Tok::Fn) {
            self.expect(Tok::LParen, "`(`")?;
            let mut params = Vec::new();
            while self.peek() != &Tok::RParen {
                params.push(self.ty()?);
                if !self.eat(&Tok::Comma) {
                    break;
                }
            }
            self.expect(Tok::RParen, "`)`")?;
            let ret = if self.eat(&Tok::Arrow) { self.ty()? } else { Type::Unit };
            return Ok(Type::Fn(Rc::new(FnSig { params, ret })));
        }
        Ok(match self.ident()?.as_str() {
            "int" => Type::Int,
            "float" => Type::Float,
            "bool" => Type::Bool,
            "arr" => Type::array(Type::Int),
            "bytes" => Type::Bytes,
            "str" => Type::Str,
            "re" => Type::Regex,
            name => {
                let mut args = Vec::new();
                if self.eat(&Tok::Lt) {
                    while self.peek() != &Tok::Gt {
                        args.push(self.ty()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(Tok::Gt, "`>`")?;
                }
                Type::Named(name.into(), args)
            }
        })
    }

    /// `type Name<A> { field: T, ... }`
    fn struct_def(&mut self) -> Result<StructDef, Error> {
        let line = self.line();
        self.expect(Tok::Type, "`type`")?;
        let name = self.ident()?;
        let params = self.type_params()?;
        self.expect(Tok::LBrace, "`{`")?;
        let mut fields = Vec::new();
        while self.peek() != &Tok::RBrace {
            let field = self.ident()?;
            self.expect(Tok::Colon, "`:`")?;
            fields.push((field, self.ty()?));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RBrace, "`}`")?;
        Ok(StructDef { name, params, fields, line })
    }

    fn func(&mut self) -> Result<Func, Error> {
        let line = self.line();
        self.expect(Tok::Fn, "`fn`")?;
        let name = self.ident()?;
        let type_params = self.type_params()?;
        self.expect(Tok::LParen, "`(`")?;
        let mut params = Vec::new();
        while self.peek() != &Tok::RParen {
            let pname = self.ident()?;
            let pty = if self.eat(&Tok::Colon) { self.ty()? } else { Type::Int };
            params.push((pname, pty));
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen, "`)`")?;
        let ret = if self.eat(&Tok::Arrow) { self.ty()? } else { Type::Unit };
        let body = self.block()?;
        Ok(Func { name, type_params, params, ret, body, line })
    }

    fn block(&mut self) -> Result<Block, Error> {
        self.expect(Tok::LBrace, "`{`")?;
        let mut stmts = Vec::new();
        let mut tail = None;

        while self.peek() != &Tok::RBrace {
            let line = self.line();
            match self.peek() {
                Tok::Be => {
                    self.advance();
                    let mutable = self.eat(&Tok::Mut);
                    let name = self.ident()?;
                    let ty = if self.eat(&Tok::Colon) { Some(self.ty()?) } else { None };
                    self.expect(Tok::Assign, "`=`")?;
                    let value = self.expr()?;
                    self.expect(Tok::Semi, "`;`")?;
                    stmts.push(Stmt::Let { name, mutable, ty, value, line });
                }
                Tok::While => {
                    self.advance();
                    let cond = self.expr()?;
                    let body = self.block()?;
                    stmts.push(Stmt::While { cond, body, line });
                }
                Tok::Return => {
                    self.advance();
                    let value = if self.peek() == &Tok::Semi { None } else { Some(self.expr()?) };
                    self.expect(Tok::Semi, "`;`")?;
                    stmts.push(Stmt::Return { value, line });
                }
                Tok::Ident(word) if word == "let" => {
                    return Err(Error::at(line, "lamda uses `be` instead of `let`: `be x = 1;`"));
                }
                _ => {
                    let e = self.expr()?;
                    if self.eat(&Tok::Assign) {
                        let value = self.expr()?;
                        self.expect(Tok::Semi, "`;`")?;
                        stmts.push(Stmt::Assign { target: e, value, line });
                    } else if self.eat(&Tok::Semi) {
                        stmts.push(Stmt::Expr(e));
                    } else if self.peek() == &Tok::RBrace {
                        tail = Some(Box::new(e));
                    } else if e.is_block_like() {
                        stmts.push(Stmt::Expr(e));
                    } else {
                        return Err(self.unexpected("`;` or `}`"));
                    }
                }
            }
        }

        self.expect(Tok::RBrace, "`}`")?;
        Ok(Block { stmts, tail })
    }

    fn expr(&mut self) -> Result<Expr, Error> {
        let mut lhs = self.binary(0)?;
        while self.peek() == &Tok::Pipe {
            let line = self.line();
            self.advance();
            let parenthesized = self.peek() == &Tok::LParen;
            let rhs = self.binary(0)?;
            lhs = pipe(lhs, rhs, parenthesized, line);
        }
        Ok(lhs)
    }

    fn binary(&mut self, min_prec: u8) -> Result<Expr, Error> {
        let mut lhs = self.unary()?;
        loop {
            let (op, prec) = match self.peek() {
                Tok::OrOr => (BinOp::Or, 1),
                Tok::AndAnd => (BinOp::And, 2),
                Tok::EqEq => (BinOp::Eq, 3),
                Tok::NotEq => (BinOp::Ne, 3),
                Tok::Lt => (BinOp::Lt, 3),
                Tok::Le => (BinOp::Le, 3),
                Tok::Gt => (BinOp::Gt, 3),
                Tok::Ge => (BinOp::Ge, 3),
                Tok::Plus => (BinOp::Add, 4),
                Tok::Minus => (BinOp::Sub, 4),
                Tok::Star => (BinOp::Mul, 5),
                Tok::Slash => (BinOp::Div, 5),
                Tok::Percent => (BinOp::Rem, 5),
                _ => break,
            };
            if prec < min_prec {
                break;
            }
            let line = self.line();
            self.advance();
            let rhs = self.binary(prec + 1)?;
            lhs = Expr { kind: ExprKind::Binary(op, Box::new(lhs), Box::new(rhs)), line };
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Result<Expr, Error> {
        let line = self.line();
        let op = match self.peek() {
            Tok::Minus => UnOp::Neg,
            Tok::Bang => UnOp::Not,
            _ => return self.postfix(),
        };
        self.advance();
        let operand = self.unary()?;
        Ok(Expr { kind: ExprKind::Unary(op, Box::new(operand)), line })
    }

    fn postfix(&mut self) -> Result<Expr, Error> {
        let mut e = self.primary()?;
        loop {
            let line = self.line();
            let kind = if self.eat(&Tok::LBracket) {
                let lo = if self.peek() == &Tok::DotDot { None } else { Some(Box::new(self.expr()?)) };
                let kind = if self.eat(&Tok::DotDot) {
                    let hi = if self.peek() == &Tok::RBracket { None } else { Some(Box::new(self.expr()?)) };
                    ExprKind::Slice(Box::new(e), lo, hi)
                } else {
                    match lo {
                        Some(index) => ExprKind::Index(Box::new(e), index),
                        None => return Err(self.unexpected("index or `..`")),
                    }
                };
                self.expect(Tok::RBracket, "`]`")?;
                kind
            } else if self.eat(&Tok::LParen) {
                ExprKind::CallExpr(Box::new(e), self.args()?)
            } else if self.eat(&Tok::Dot) {
                ExprKind::Field(Box::new(e), self.ident()?)
            } else {
                return Ok(e);
            };
            e = Expr { kind, line };
        }
    }

    /// Parses call arguments after the opening `(`.
    fn args(&mut self) -> Result<Vec<Expr>, Error> {
        let mut args = Vec::new();
        while self.peek() != &Tok::RParen {
            args.push(self.expr()?);
            if !self.eat(&Tok::Comma) {
                break;
            }
        }
        self.expect(Tok::RParen, "`)`")?;
        Ok(args)
    }

    fn primary(&mut self) -> Result<Expr, Error> {
        let line = self.line();
        let kind = match self.advance() {
            Tok::Int(n) => ExprKind::Int(n),
            Tok::Float(x) => ExprKind::Float(x),
            Tok::Str(bytes) => ExprKind::Str(bytes),
            Tok::Regex(pattern) => ExprKind::Regex(pattern),
            Tok::True => ExprKind::Bool(true),
            Tok::False => ExprKind::Bool(false),
            Tok::Ident(name) => {
                if self.eat(&Tok::LParen) {
                    ExprKind::Call(name, self.args()?)
                } else {
                    ExprKind::Var(name)
                }
            }
            Tok::LParen => {
                let e = self.expr()?;
                self.expect(Tok::RParen, "`)`")?;
                return Ok(e);
            }
            Tok::Backslash => {
                // `\x, y: arr -> body`; the body extends as far right as possible.
                let mut params = Vec::new();
                while self.peek() != &Tok::Arrow {
                    let pname = self.ident()?;
                    let pty = if self.eat(&Tok::Colon) { self.ty()? } else { Type::Int };
                    params.push((pname, pty));
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::Arrow, "`->`")?;
                ExprKind::Lambda(params, Box::new(self.expr()?))
            }
            Tok::LBracket => {
                let mut items = Vec::new();
                while self.peek() != &Tok::RBracket {
                    items.push(self.expr()?);
                    if !self.eat(&Tok::Comma) {
                        break;
                    }
                }
                self.expect(Tok::RBracket, "`]`")?;
                ExprKind::Array(items)
            }
            Tok::LBrace => {
                self.pos -= 1;
                ExprKind::Block(self.block()?)
            }
            Tok::If => {
                let cond = self.expr()?;
                let then = self.block()?;
                let otherwise = if self.eat(&Tok::Else) {
                    if self.peek() == &Tok::If {
                        // `else if` is sugar for `else { if ... }`.
                        let nested = self.primary()?;
                        Some(Block { stmts: Vec::new(), tail: Some(Box::new(nested)) })
                    } else {
                        Some(self.block()?)
                    }
                } else {
                    None
                };
                ExprKind::If(Box::new(cond), then, otherwise)
            }
            tok => return Err(Error::at(line, format!("expected expression, found {tok:?}"))),
        };
        Ok(Expr { kind, line })
    }
}
