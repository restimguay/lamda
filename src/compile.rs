//! Type-checks the AST and lowers it to C in a single pass.
//!
//! Blocks and `if` become GNU statement expressions (`({ ... })`), which both
//! clang and gcc support, so lamda's expression-oriented control flow maps
//! directly onto C without temporaries.
//!
//! Function values are `lm_fn { fn, env }` pairs. Each lambda is lifted to a
//! top-level C function that reads its captured variables from `env`. Captures
//! are copied when the lambda is created, so a closure never sees later
//! assignments, and a lambda without captures allocates nothing.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::ast::*;
use crate::Error;

const RUNTIME: &str = include_str!("runtime.c");

const BUILTINS: [&str; 12] = [
    "print", "array", "bytes", "len", "str", "parse", "read_file", "find", "matches", "extract", "count", "fold_matches",
];

/// Self-recursive functions are cloned into themselves up to this many levels.
const MAX_UNROLL: usize = 2;
/// Upper bound on expression nodes across all clones of one function.
const UNROLL_BUDGET: usize = 400;

#[derive(Clone)]
struct Local {
    cname: String,
    ty: Type,
    mutable: bool,
    captured: bool,
}

struct Capture {
    outer: String,
    inner: String,
    ty: Type,
}

/// The variables visible in one C function: a top-level function or a lifted lambda.
struct Frame {
    scopes: Vec<HashMap<String, Local>>,
    captures: Vec<Capture>,
    /// `None` while a lambda's return type is still being inferred.
    ret: Option<Type>,
}

impl Frame {
    fn new(ret: Option<Type>) -> Self {
        Frame { scopes: vec![HashMap::new()], captures: Vec::new(), ret }
    }
}

struct Gen<'a> {
    sigs: HashMap<&'a str, Rc<FnSig>>,
    frames: Vec<Frame>,
    next_id: usize,
    /// While emitting a function: its name and the C function its self-calls go to.
    recursion: Option<(&'a str, String)>,
    /// Env structs and prototypes of lifted lambdas and function-value wrappers.
    decls: String,
    /// Definitions of lifted lambdas and function-value wrappers.
    lifted: String,
    wrapped: HashSet<String>,
    /// C names of the DFAs already emitted, by pattern.
    regexes: HashMap<String, String>,
}

pub fn to_c(funcs: &[Func]) -> Result<String, Error> {
    let mut sigs = HashMap::new();
    for f in funcs {
        if BUILTINS.contains(&f.name.as_str()) {
            return Err(Error::at(f.line, format!("`{}` is a builtin and cannot be redefined", f.name)));
        }
        let sig = FnSig { params: f.params.iter().map(|p| p.1.clone()).collect(), ret: f.ret.clone() };
        if sigs.insert(f.name.as_str(), Rc::new(sig)).is_some() {
            return Err(Error::at(f.line, format!("function `{}` is defined twice", f.name)));
        }
    }
    match sigs.get("main") {
        Some(sig) if sig.params.is_empty() && sig.ret == Type::Unit => {}
        Some(_) => return Err(Error::msg("`main` must take no parameters and return nothing")),
        None => return Err(Error::msg("no `main` function")),
    }

    let mut g = Gen {
        sigs,
        frames: Vec::new(),
        next_id: 0,
        recursion: None,
        decls: String::new(),
        lifted: String::new(),
        wrapped: HashSet::new(),
        regexes: HashMap::new(),
    };
    let depths: Vec<usize> = funcs.iter().map(unroll_depth).collect();

    let mut prototypes = String::new();
    let mut bodies = String::new();
    for (f, &depth) in funcs.iter().zip(&depths) {
        for level in 0..=depth {
            prototypes.push_str(&prototype(f, level));
            prototypes.push_str(";\n");
        }
    }
    for (f, &depth) in funcs.iter().zip(&depths) {
        // f_name -> u{depth}_name -> ... -> u1_name -> f_name. clang inlines the
        // clones back into f_name, which exposes repeated pure calls (fib(n-3)
        // appears twice once fib is unrolled) to common-subexpression elimination.
        for level in 0..=depth {
            let target = if level == 0 { depth } else { level - 1 };
            g.recursion = Some((f.name.as_str(), c_name(f, target)));
            bodies.push('\n');
            bodies.push_str(&g.func(f, level)?);
        }
    }

    Ok(format!(
        "{RUNTIME}\n{}{prototypes}{bodies}{}\n\
         // Not inlined, so every frame the GC scans lies below main's frame.\n\
         static __attribute__((noinline)) void lm_start(void) {{ f_main(); }}\n\n\
         int main(void) {{\n    lm_gc_init(__builtin_frame_address(0));\n    lm_start();\n    return 0;\n}}\n",
        g.decls, g.lifted
    ))
}

fn c_type(t: &Type) -> &'static str {
    match t {
        Type::Int => "int64_t",
        Type::Bool => "bool",
        Type::Arr => "lm_arr",
        Type::Bytes => "lm_bytes",
        Type::Str => "lm_str",
        Type::Regex => "const lm_re *",
        Type::Fn(_) => "lm_fn",
        Type::Unit | Type::Never => "void",
    }
}

/// The C type of a pointer to the lifted function behind a value of type `sig`.
fn c_fn_ptr(sig: &FnSig) -> String {
    let mut params = String::from("void *");
    for p in &sig.params {
        params.push_str(", ");
        params.push_str(c_type(p));
    }
    format!("{} (*)({params})", c_type(&sig.ret))
}

/// The C name of `f` at an unroll level; level 0 is the function itself.
fn c_name(f: &Func, level: usize) -> String {
    if level == 0 {
        format!("f_{}", f.name)
    } else {
        format!("u{level}_{}", f.name)
    }
}

fn c_params(params: &[Type]) -> String {
    let params: Vec<String> = params.iter().enumerate().map(|(i, t)| format!("{} p{i}", c_type(t))).collect();
    if params.is_empty() { "void".to_string() } else { params.join(", ") }
}

fn prototype(f: &Func, level: usize) -> String {
    let params: Vec<Type> = f.params.iter().map(|p| p.1.clone()).collect();
    let inline = if level > 0 { "inline " } else { "" };
    format!("static {inline}{} {}({})", c_type(&f.ret), c_name(f, level), c_params(&params))
}

/// How many levels to unroll `f` into itself: as many as fit the budget, given
/// that each level multiplies the copies by the number of self-call sites.
fn unroll_depth(f: &Func) -> usize {
    let (mut size, mut self_calls) = (0, 0);
    visit_block(&f.body, &mut |e| {
        size += 1;
        if matches!(&e.kind, ExprKind::Call(name, _) if *name == f.name) {
            self_calls += 1;
        }
    });
    let (mut depth, mut copies, mut level) = (0, 1, 1);
    while self_calls > 0 && depth < MAX_UNROLL {
        level *= self_calls;
        if size * (copies + level) > UNROLL_BUDGET {
            break;
        }
        copies += level;
        depth += 1;
    }
    depth
}

fn visit_block(b: &Block, f: &mut impl FnMut(&Expr)) {
    for s in &b.stmts {
        match s {
            Stmt::Let { value, .. } | Stmt::Assign { value, .. } => visit_expr(value, f),
            Stmt::IndexAssign { arr, index, value, .. } => {
                visit_expr(arr, f);
                visit_expr(index, f);
                visit_expr(value, f);
            }
            Stmt::While { cond, body, .. } => {
                visit_expr(cond, f);
                visit_block(body, f);
            }
            Stmt::Return { value, .. } => value.iter().for_each(|e| visit_expr(e, f)),
            Stmt::Expr(e) => visit_expr(e, f),
        }
    }
    if let Some(e) = &b.tail {
        visit_expr(e, f);
    }
}

fn visit_expr(e: &Expr, f: &mut impl FnMut(&Expr)) {
    f(e);
    match &e.kind {
        ExprKind::Int(_) | ExprKind::Str(_) | ExprKind::Regex(_) | ExprKind::Bool(_) | ExprKind::Var(_) => {}
        ExprKind::Slice(xs, lo, hi) => {
            visit_expr(xs, f);
            lo.iter().chain(hi).for_each(|e| visit_expr(e, f));
        }
        ExprKind::Unary(_, x) | ExprKind::Lambda(_, x) => visit_expr(x, f),
        ExprKind::Binary(_, l, r) | ExprKind::Index(l, r) => {
            visit_expr(l, f);
            visit_expr(r, f);
        }
        ExprKind::Call(_, args) => args.iter().for_each(|a| visit_expr(a, f)),
        ExprKind::CallExpr(callee, args) => {
            visit_expr(callee, f);
            args.iter().for_each(|a| visit_expr(a, f));
        }
        ExprKind::If(cond, then, otherwise) => {
            visit_expr(cond, f);
            visit_block(then, f);
            if let Some(b) = otherwise {
                visit_block(b, f);
            }
        }
        ExprKind::Block(b) => visit_block(b, f),
    }
}

fn unify(a: &Type, b: &Type) -> Option<Type> {
    match (a, b) {
        _ if a == b => Some(a.clone()),
        (Type::Never, t) | (t, Type::Never) => Some(t.clone()),
        _ => None,
    }
}

/// Wraps an expression that never produces a value so it type-checks in C
/// where a value of type `to` is expected.
fn coerce(code: String, from: &Type, to: &Type) -> String {
    let zero = match to {
        Type::Int => "0",
        Type::Bool => "false",
        Type::Arr => "(lm_arr){0}",
        Type::Bytes => "(lm_bytes){0}",
        Type::Str => "(lm_str){0}",
        Type::Regex => "(const lm_re *)0",
        Type::Fn(_) => "(lm_fn){0}",
        Type::Unit | Type::Never => return code,
    };
    if *from == Type::Never {
        format!("({{ {code}; __builtin_unreachable(); {zero}; }})")
    } else {
        code
    }
}

fn expect(line: usize, what: &str, want: &Type, got: &Type) -> Result<(), Error> {
    if unify(want, got).as_ref() == Some(want) {
        Ok(())
    } else {
        Err(Error::at(line, format!("{what}: expected {want}, found {got}")))
    }
}

/// Loop tuning for clang, chosen by how the loop steps.
///
/// A loop that advances a variable by a runtime amount (`j = j + i`) is not
/// unrolled: unrolling needs its trip count, a division that clang hoists in
/// front of the loop, where it runs even when the loop body never does (sieve:
/// 6-13%). Every other loop gets four interleaved copies, so reductions such as
/// `acc = f(acc, x)` keep four independent accumulators instead of clang's
/// default two (closure: 8%).
fn loop_hint(body: &Block) -> &'static str {
    let runtime_step = body.stmts.iter().any(|s| match s {
        Stmt::Assign { name, value, .. } => match &value.kind {
            ExprKind::Binary(BinOp::Add | BinOp::Sub, lhs, step) => {
                matches!(&lhs.kind, ExprKind::Var(v) if v == name) && !matches!(step.kind, ExprKind::Int(_))
            }
            _ => false,
        },
        _ => false,
    });
    if runtime_step {
        "unroll(disable)"
    } else {
        "interleave_count(4)"
    }
}

/// A string literal as an `lm_str`. Bytes outside printable ASCII, and `?` (trigraphs),
/// become three-digit octal escapes, which cannot run into the following character.
fn c_string(bytes: &[u8]) -> String {
    let mut lit = String::new();
    for &b in bytes {
        match b {
            b'"' | b'\\' => {
                lit.push('\\');
                lit.push(b as char);
            }
            b' '..=b'~' if b != b'?' => lit.push(b as char),
            _ => lit.push_str(&format!("\\{b:03o}")),
        }
    }
    format!("((lm_str){{ {}, (const uint8_t *)\"{lit}\" }})", bytes.len())
}

fn expect_indexable(line: usize, got: &Type) -> Result<(), Error> {
    match got {
        Type::Arr | Type::Bytes | Type::Str | Type::Never => Ok(()),
        _ => Err(Error::at(line, format!("expected arr, bytes or str, found {got}"))),
    }
}

impl<'a> Gen<'a> {
    fn fresh(&mut self) -> usize {
        self.next_id += 1;
        self.next_id
    }

    fn frame(&mut self) -> &mut Frame {
        self.frames.last_mut().unwrap()
    }

    fn func(&mut self, f: &Func, level: usize) -> Result<String, Error> {
        self.frames.push(Frame::new(Some(f.ret.clone())));
        let header = self.bind_params(&f.params);
        let (body, ty) = self.block(&f.body)?;
        self.frames.pop();

        let what = format!("body of `{}`", f.name);
        expect(f.line, &what, &f.ret, &ty)?;
        let body = if f.ret == Type::Unit {
            format!("    {body};\n")
        } else {
            format!("    return {};\n", coerce(body, &ty, &f.ret))
        };
        Ok(format!("{} {{\n{header}{body}}}\n", prototype(f, level)))
    }

    /// Declares parameters `p0..pN` as locals of the current frame.
    fn bind_params(&mut self, params: &[(String, Type)]) -> String {
        let mut header = String::new();
        for (i, (name, ty)) in params.iter().enumerate() {
            let cname = self.declare(name, ty.clone(), false);
            header.push_str(&format!("    {} {cname} = p{i};\n", c_type(ty)));
        }
        header
    }

    fn declare(&mut self, name: &str, ty: Type, mutable: bool) -> String {
        // Unique C names make shadowing (`be x = x + 1;`) trivially correct.
        let cname = format!("v_{name}_{}", self.fresh());
        let local = Local { cname: cname.clone(), ty, mutable, captured: false };
        self.frame().scopes.last_mut().unwrap().insert(name.to_string(), local);
        cname
    }

    /// Finds a local, capturing it from enclosing frames into every lambda between.
    fn lookup(&mut self, name: &str) -> Option<Local> {
        self.lookup_in(self.frames.len() - 1, name)
    }

    fn lookup_in(&mut self, depth: usize, name: &str) -> Option<Local> {
        if let Some(local) = self.frames[depth].scopes.iter().rev().find_map(|s| s.get(name)) {
            return Some(local.clone());
        }
        if depth == 0 {
            return None;
        }
        let outer = self.lookup_in(depth - 1, name)?;
        let inner = format!("v_{name}_{}", self.fresh());
        let local = Local { cname: inner.clone(), ty: outer.ty.clone(), mutable: false, captured: true };
        let frame = &mut self.frames[depth];
        frame.captures.push(Capture { outer: outer.cname, inner, ty: outer.ty });
        frame.scopes[0].insert(name.to_string(), local.clone());
        Some(local)
    }

    fn block(&mut self, b: &Block) -> Result<(String, Type), Error> {
        self.frame().scopes.push(HashMap::new());
        let mut code = String::from("({ ");
        for s in &b.stmts {
            code.push_str(&self.stmt(s)?);
            code.push(' ');
        }
        let ty = match &b.tail {
            Some(e) => {
                let (c, t) = self.expr(e)?;
                code.push_str(&c);
                code.push_str("; ");
                t
            }
            None if matches!(b.stmts.last(), Some(Stmt::Return { .. })) => Type::Never,
            None => Type::Unit,
        };
        code.push_str("})");
        self.frame().scopes.pop();
        Ok((code, ty))
    }

    fn stmt(&mut self, s: &Stmt) -> Result<String, Error> {
        match s {
            Stmt::Let { name, mutable, value, line } => {
                let (code, ty) = self.expr(value)?;
                if matches!(ty, Type::Unit | Type::Never) {
                    return Err(Error::at(*line, format!("cannot bind `{name}` to a value of type {ty}")));
                }
                let c_ty = c_type(&ty);
                let cname = self.declare(name, ty, *mutable);
                Ok(format!("{c_ty} {cname} = {code};"))
            }
            Stmt::Assign { name, value, line } => {
                let (code, ty) = self.expr(value)?;
                let local = self
                    .lookup(name)
                    .ok_or_else(|| Error::at(*line, format!("unknown variable `{name}`")))?;
                if local.captured {
                    return Err(Error::at(*line, format!("cannot assign to `{name}`: closures capture variables by value")));
                }
                if !local.mutable {
                    return Err(Error::at(*line, format!("cannot assign twice to immutable `{name}` (use `be mut`)")));
                }
                expect(*line, &format!("assignment to `{name}`"), &local.ty, &ty)?;
                Ok(format!("{} = {code};", local.cname))
            }
            Stmt::IndexAssign { arr, index, value, line } => {
                let (a, at) = self.expr(arr)?;
                let (i, it) = self.expr(index)?;
                let (v, vt) = self.expr(value)?;
                expect_indexable(*line, &at)?;
                if at == Type::Str {
                    return Err(Error::at(*line, "strings are immutable"));
                }
                expect(*line, "array index", &Type::Int, &it)?;
                expect(*line, "array element", &Type::Int, &vt)?;
                // Stores into `bytes` truncate to 0..=255, like Rust's `as u8`.
                Ok(format!("({a}).data[{i}] = {v};"))
            }
            Stmt::While { cond, body, line } => {
                let (c, ct) = self.expr(cond)?;
                expect(*line, "`while` condition", &Type::Bool, &ct)?;
                let (b, bt) = self.block(body)?;
                expect(*line, "`while` body", &Type::Unit, &bt)?;
                Ok(format!("_Pragma(\"clang loop {}\") while ({c}) {{ {b}; }}", loop_hint(body)))
            }
            Stmt::Return { value, line } => {
                let (c, t) = match value {
                    Some(e) => self.expr(e)?,
                    None => (String::new(), Type::Unit),
                };
                let frame = self.frame();
                let ret = match &frame.ret {
                    Some(ret) => {
                        expect(*line, "returned value", ret, &t)?;
                        ret.clone()
                    }
                    // The first `return` in a lambda fixes its return type.
                    None => frame.ret.insert(t.clone()).clone(),
                };
                Ok(match ret {
                    Type::Unit if c.is_empty() => "return;".to_string(),
                    Type::Unit => format!("{{ {c}; return; }}"),
                    _ => format!("return {};", coerce(c, &t, &ret)),
                })
            }
            Stmt::Expr(e) => Ok(format!("{};", self.expr(e)?.0)),
        }
    }

    fn expr(&mut self, e: &Expr) -> Result<(String, Type), Error> {
        let line = e.line;
        Ok(match &e.kind {
            ExprKind::Int(n) => (format!("INT64_C({n})"), Type::Int),
            ExprKind::Str(bytes) => (c_string(bytes), Type::Str),
            ExprKind::Regex(pattern) => (format!("(&{})", self.regex(pattern, line)?), Type::Regex),
            ExprKind::Bool(b) => (b.to_string(), Type::Bool),
            ExprKind::Var(name) => {
                if let Some(local) = self.lookup(name) {
                    (local.cname, local.ty)
                } else if self.sigs.contains_key(name.as_str()) {
                    self.function_value(name)
                } else if BUILTINS.contains(&name.as_str()) {
                    return Err(Error::at(line, format!("builtin `{name}` cannot be used as a value")));
                } else {
                    return Err(Error::at(line, format!("unknown variable `{name}`")));
                }
            }
            ExprKind::Unary(op, operand) => {
                let (c, t) = self.expr(operand)?;
                match op {
                    UnOp::Neg => {
                        expect(line, "operand of `-`", &Type::Int, &t)?;
                        (format!("(-{c})"), Type::Int)
                    }
                    UnOp::Not => {
                        expect(line, "operand of `!`", &Type::Bool, &t)?;
                        (format!("(!{c})"), Type::Bool)
                    }
                }
            }
            ExprKind::Binary(op, lhs, rhs) => {
                let (l, lt) = self.expr(lhs)?;
                let (r, rt) = self.expr(rhs)?;
                let what = format!("operand of `{}`", op.c_op());
                if lt == Type::Str {
                    expect(line, &what, &Type::Str, &rt)?;
                    return Ok(match op {
                        BinOp::Add => (format!("lm_concat({l}, {r})"), Type::Str),
                        BinOp::Eq => (format!("lm_str_eq({l}, {r})"), Type::Bool),
                        BinOp::Ne => (format!("(!lm_str_eq({l}, {r}))"), Type::Bool),
                        _ => return Err(Error::at(line, format!("`{}` is not defined for str", op.c_op()))),
                    });
                }
                let ty = match op {
                    BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                        expect(line, &what, &Type::Int, &lt)?;
                        expect(line, &what, &Type::Int, &rt)?;
                        Type::Int
                    }
                    BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                        expect(line, &what, &Type::Int, &lt)?;
                        expect(line, &what, &Type::Int, &rt)?;
                        Type::Bool
                    }
                    BinOp::Eq | BinOp::Ne => {
                        if !matches!(lt, Type::Int | Type::Bool) {
                            return Err(Error::at(line, format!("cannot compare values of type {lt}")));
                        }
                        expect(line, &what, &lt, &rt)?;
                        Type::Bool
                    }
                    BinOp::And | BinOp::Or => {
                        expect(line, &what, &Type::Bool, &lt)?;
                        expect(line, &what, &Type::Bool, &rt)?;
                        Type::Bool
                    }
                };
                (format!("({l} {} {r})", op.c_op()), ty)
            }
            ExprKind::Call(name, args) => match self.lookup(name) {
                Some(local) => self.indirect_call(&local.cname, &local.ty, args, line)?,
                None => self.call(name, args, line)?,
            },
            ExprKind::CallExpr(callee, args) => {
                let (c, t) = self.expr(callee)?;
                self.indirect_call(&c, &t, args, line)?
            }
            ExprKind::Lambda(params, body) => self.lambda(params, body, line)?,
            ExprKind::Slice(xs, lo, hi) => {
                let (x, xt) = self.expr(xs)?;
                expect_indexable(line, &xt)?;
                let id = self.fresh();
                let mut bound = |e: &Option<Box<Expr>>, default: String| -> Result<String, Error> {
                    match e {
                        Some(e) => {
                            let (c, t) = self.expr(e)?;
                            expect(line, "slice bound", &Type::Int, &t)?;
                            Ok(c)
                        }
                        None => Ok(default),
                    }
                };
                let lo = bound(lo, "0".to_string())?;
                let hi = bound(hi, format!("s{id}.len"))?;
                let ct = c_type(&xt);
                let code = format!(
                    "({{ {ct} s{id} = {x}; int64_t lo{id} = {lo}; int64_t hi{id} = {hi}; ({ct}){{ hi{id} - lo{id}, s{id}.data + lo{id} }}; }})"
                );
                (code, xt)
            }
            ExprKind::Index(arr, index) => {
                let (a, at) = self.expr(arr)?;
                let (i, it) = self.expr(index)?;
                expect_indexable(line, &at)?;
                expect(line, "array index", &Type::Int, &it)?;
                (format!("((int64_t)({a}).data[{i}])"), Type::Int)
            }
            ExprKind::If(cond, then, otherwise) => {
                let (c, ct) = self.expr(cond)?;
                expect(line, "`if` condition", &Type::Bool, &ct)?;
                let (t, tt) = self.block(then)?;
                match otherwise {
                    Some(otherwise) => {
                        let (o, ot) = self.block(otherwise)?;
                        let ty = unify(&tt, &ot).ok_or_else(|| {
                            Error::at(line, format!("`if` and `else` have different types: {tt} and {ot}"))
                        })?;
                        (format!("({c} ? {} : {})", coerce(t, &tt, &ty), coerce(o, &ot, &ty)), ty)
                    }
                    None => {
                        expect(line, "`if` without `else`", &Type::Unit, &tt)?;
                        (format!("({{ if ({c}) {t}; }})"), Type::Unit)
                    }
                }
            }
            ExprKind::Block(b) => self.block(b)?,
        })
    }

    fn args(&mut self, name: &str, params: &[Type], args: &[Expr], line: usize) -> Result<Vec<String>, Error> {
        if args.len() != params.len() {
            return Err(Error::at(line, format!("`{name}` takes {} argument(s), got {}", params.len(), args.len())));
        }
        let mut compiled = Vec::with_capacity(args.len());
        for (i, (a, want)) in args.iter().zip(params).enumerate() {
            let (c, got) = self.expr(a)?;
            expect(line, &format!("argument {} of `{name}`", i + 1), want, &got)?;
            compiled.push(coerce(c, &got, want));
        }
        Ok(compiled)
    }

    fn call(&mut self, name: &str, args: &[Expr], line: usize) -> Result<(String, Type), Error> {
        if let Some(sig) = self.sigs.get(name).cloned() {
            let args = self.args(name, &sig.params, args, line)?.join(", ");
            let target = match &self.recursion {
                Some((current, target)) if *current == name => target.clone(),
                _ => format!("f_{name}"),
            };
            return Ok((format!("{target}({args})"), sig.ret.clone()));
        }

        let arity = match name {
            "find" | "matches" | "extract" | "count" => 2,
            "fold_matches" => 4,
            _ if BUILTINS.contains(&name) => 1,
            _ => return Err(Error::at(line, format!("unknown function `{name}`"))),
        };
        if args.len() != arity {
            return Err(Error::at(line, format!("`{name}` takes {arity} argument(s), got {}", args.len())));
        }
        let mut compiled = Vec::with_capacity(arity);
        for a in args {
            compiled.push(self.expr(a)?);
        }
        let (c, t) = &compiled[0];
        match name {
            "print" => match t {
                Type::Int => Ok((format!("lm_print_int({c})"), Type::Unit)),
                Type::Bool => Ok((format!("lm_print_bool({c})"), Type::Unit)),
                Type::Str => Ok((format!("lm_print_str({c})"), Type::Unit)),
                _ => Err(Error::at(line, format!("cannot print a value of type {t}"))),
            },
            "array" => {
                expect(line, "array length", &Type::Int, t)?;
                Ok((format!("lm_array({c})"), Type::Arr))
            }
            "bytes" => {
                expect(line, "array length", &Type::Int, t)?;
                Ok((format!("lm_bytes_new({c})"), Type::Bytes))
            }
            "len" => {
                expect_indexable(line, t)?;
                Ok((format!("({c}).len"), Type::Int))
            }
            "str" => {
                expect(line, "argument of `str`", &Type::Int, t)?;
                Ok((format!("lm_str_of({c})"), Type::Str))
            }
            "parse" => {
                expect(line, "argument of `parse`", &Type::Str, t)?;
                Ok((format!("lm_parse({c})"), Type::Int))
            }
            "read_file" => {
                expect(line, "file path", &Type::Str, t)?;
                Ok((format!("lm_read_file({c})"), Type::Str))
            }
            "find" if compiled[1].1 == Type::Regex => {
                expect(line, "argument 1 of `find`", &Type::Str, t)?;
                Ok((format!("lm_re_find({c}, {})", compiled[1].0), Type::Int))
            }
            "find" => {
                let (needle, nt) = &compiled[1];
                expect(line, "argument 1 of `find`", &Type::Str, t)?;
                expect(line, "argument 2 of `find`", &Type::Str, nt)?;
                Ok((format!("lm_find({c}, {needle})"), Type::Int))
            }
            "matches" | "extract" | "count" | "fold_matches" => {
                let (re, rt) = &compiled[1];
                expect(line, &format!("argument 1 of `{name}`"), &Type::Str, t)?;
                expect(line, &format!("argument 2 of `{name}`"), &Type::Regex, rt)?;
                Ok(match name {
                    "matches" => (format!("lm_re_matches({c}, {re})"), Type::Bool),
                    "extract" => (format!("lm_re_extract({c}, {re})"), Type::Str),
                    "count" => (format!("lm_re_count({c}, {re})"), Type::Int),
                    _ => {
                        let (init, it) = &compiled[2];
                        let (f, ft) = &compiled[3];
                        let folder = Type::Fn(Rc::new(FnSig { params: vec![Type::Int, Type::Str], ret: Type::Int }));
                        expect(line, "argument 3 of `fold_matches`", &Type::Int, it)?;
                        expect(line, "argument 4 of `fold_matches`", &folder, ft)?;
                        (format!("lm_re_fold({c}, {re}, {init}, {f})"), Type::Int)
                    }
                })
            }
            _ => unreachable!("builtin `{name}` has no implementation"),
        }
    }

    /// Compiles a regex literal to a DFA and emits its tables once; returns the C name.
    fn regex(&mut self, pattern: &str, line: usize) -> Result<String, Error> {
        if let Some(name) = self.regexes.get(pattern) {
            return Ok(name.clone());
        }
        let dfa = crate::regex::compile(pattern).map_err(|e| Error::at(line, format!("invalid regex `{pattern}`: {e}")))?;
        let name = format!("lm_re{}", self.fresh());
        // A start-byte set of up to four bytes is scanned for with SIMD; pad it by repetition.
        let mut firsts: Vec<u8> = (0..=255u8).filter(|&b| dfa.first[b as usize]).collect();
        if firsts.len() > 4 || dfa.accept[1] {
            firsts.clear();
        }
        let set: Vec<u8> = (0..4).map(|k| firsts.get(k).or(firsts.first()).copied().unwrap_or(0)).collect();
        let join = |items: Vec<String>| items.join(",");
        self.decls.push_str(&format!(
            "static const uint16_t {name}_next[] = {{{}}};\n\
             static const bool {name}_accept[] = {{{}}};\n\
             static const lm_re {name} = {{ {name}_next, {name}_accept, {}, {{{}}}, {}, {}, {}, {{{}}} }};\n",
            join(dfa.next.iter().map(|n| n.to_string()).collect()),
            join(dfa.accept.iter().map(|&a| (a as u8).to_string()).collect()),
            c_string(&dfa.prefix),
            join(dfa.first.iter().map(|&f| (f as u8).to_string()).collect()),
            dfa.anchor_start,
            dfa.anchor_end,
            firsts.len(),
            join(set.iter().map(|b| b.to_string()).collect()),
        ));
        self.regexes.insert(pattern.to_string(), name.clone());
        Ok(name)
    }

    fn indirect_call(&mut self, callee: &str, ty: &Type, args: &[Expr], line: usize) -> Result<(String, Type), Error> {
        let Type::Fn(sig) = ty else {
            return Err(Error::at(line, format!("cannot call a value of type {ty}")));
        };
        let mut args = self.args("function value", &sig.params, args, line)?;
        let k = format!("k{}", self.fresh());
        args.insert(0, format!("{k}.env"));
        let code = format!("({{ lm_fn {k} = {callee}; (({}){k}.fn)({}); }})", c_fn_ptr(sig), args.join(", "));
        Ok((code, sig.ret.clone()))
    }

    /// A top-level function used as a value, through a wrapper that ignores `env`.
    fn function_value(&mut self, name: &str) -> (String, Type) {
        let sig = self.sigs[name].clone();
        let wrapper = format!("w_{name}");
        if self.wrapped.insert(name.to_string()) {
            let mut params = String::from("void *env");
            let mut args = Vec::new();
            for (i, p) in sig.params.iter().enumerate() {
                params.push_str(&format!(", {} p{i}", c_type(p)));
                args.push(format!("p{i}"));
            }
            let head = format!("static {} {wrapper}({params})", c_type(&sig.ret));
            let ret = if sig.ret == Type::Unit { "" } else { "return " };
            self.decls.push_str(&format!("{head};\n"));
            self.lifted.push_str(&format!("\n{head} {{ {ret}f_{name}({}); }}\n", args.join(", ")));
        }
        (format!("((lm_fn){{ (void *){wrapper}, NULL }})"), Type::Fn(sig))
    }

    fn lambda(&mut self, params: &[(String, Type)], body: &Expr, line: usize) -> Result<(String, Type), Error> {
        let name = format!("lam{}", self.fresh());
        self.frames.push(Frame::new(None));
        let header = self.bind_params(params);
        let (code, ty) = self.expr(body)?;
        let frame = self.frames.pop().unwrap();

        let ret = match &frame.ret {
            Some(ret) => {
                expect(line, "lambda body", ret, &ty)?;
                ret.clone()
            }
            None => ty.clone(),
        };
        let sig = FnSig { params: params.iter().map(|p| p.1.clone()).collect(), ret };
        let body = if sig.ret == Type::Unit {
            format!("    {code};\n")
        } else {
            format!("    return {};\n", coerce(code, &ty, &sig.ret))
        };

        let mut c_params = String::from("void *env_");
        for (i, p) in sig.params.iter().enumerate() {
            c_params.push_str(&format!(", {} p{i}", c_type(p)));
        }
        let head = format!("static {} {name}({c_params})", c_type(&sig.ret));
        self.decls.push_str(&format!("{head};\n"));

        let value = if frame.captures.is_empty() {
            self.lifted.push_str(&format!("\n{head} {{\n{header}{body}}}\n"));
            format!("((lm_fn){{ (void *){name}, NULL }})")
        } else {
            let env = format!("struct {name}_env");
            let mut fields = String::new();
            let mut loads = format!("    {env} *env = env_;\n");
            let mut stores = String::new();
            for (i, cap) in frame.captures.iter().enumerate() {
                fields.push_str(&format!(" {} c{i};", c_type(&cap.ty)));
                loads.push_str(&format!("    {} {} = env->c{i};\n", c_type(&cap.ty), cap.inner));
                stores.push_str(&format!(" e->c{i} = {};", cap.outer));
            }
            self.decls.push_str(&format!("{env} {{{fields} }};\n"));
            self.lifted.push_str(&format!("\n{head} {{\n{loads}{header}{body}}}\n"));
            format!("({{ {env} *e = lm_gc_alloc(sizeof({env}), 1);{stores} (lm_fn){{ (void *){name}, e }}; }})")
        };
        Ok((value, Type::Fn(Rc::new(sig))))
    }
}
