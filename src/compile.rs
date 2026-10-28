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
//!
//! Structs are C structs passed by value; arrays are `{ len, data }` views of
//! GC memory, shared by reference. Generic functions are monomorphized: each
//! combination of type arguments, inferred at the call, gets its own C function.

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::ast::*;
use crate::Error;

const RUNTIME: &str = include_str!("runtime.c");

const BUILTINS: [&str; 32] = [
    "print", "array", "bytes", "len", "str", "parse", "read_file", "find", "matches", "extract", "count", "fold_matches",
    "float", "int", "sqrt", "floor", "ceil", "round", "abs", "sin", "cos", "exp", "ln", "pow", "min", "max", "split",
    "join", "captures", "all_captures", "all_matches", "replace",
];

/// Self-recursive functions are cloned into themselves up to this many levels.
const MAX_UNROLL: usize = 2;
/// Upper bound on expression nodes across all clones of one function.
const UNROLL_BUDGET: usize = 400;
/// Instantiations whose mangled type arguments grow past this are polymorphic recursion.
const MAX_INSTANCE_NAME: usize = 400;

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

/// A generic function instantiated with concrete types, waiting to be emitted.
struct Instance<'a> {
    func: &'a Func,
    bindings: HashMap<String, Type>,
    cname: String,
    sig: Rc<FnSig>,
}

#[derive(Default)]
struct Out {
    /// `typedef struct lm_S... lm_S...;` for every struct type, so arrays can point to them.
    struct_fwd: String,
    /// Array typedefs, in the order their element types were defined.
    array_types: String,
    /// Struct bodies, each after the structs it contains by value.
    struct_bodies: String,
    /// Env structs, regex tables and prototypes of generated functions.
    decls: String,
    /// Prototypes of user functions and generic instances.
    protos: String,
    /// Bodies of user functions and generic instances.
    bodies: String,
    /// Bodies of lambdas, wrappers and generated helpers.
    lifted: String,
}

struct Gen<'a> {
    structs: HashMap<&'a str, &'a StructDef>,
    funcs: HashMap<&'a str, &'a Func>,
    /// Resolved signatures of the non-generic functions.
    sigs: HashMap<&'a str, Rc<FnSig>>,
    /// Type parameter bindings of the function being compiled.
    type_env: HashMap<String, Type>,
    frames: Vec<Frame>,
    next_id: usize,
    /// While emitting a function: its name and the C function its self-calls go to.
    recursion: Option<(&'a str, String)>,
    out: Out,
    /// C names of the array and struct types already defined, by mangled type.
    c_types: HashMap<String, String>,
    /// Generated helpers (`lm_eq_*`, `lm_w_*`) already emitted.
    helpers: HashSet<String>,
    wrapped: HashSet<String>,
    /// C names of the DFAs already emitted, by pattern.
    regexes: HashMap<String, String>,
    instances: HashMap<String, (String, Rc<FnSig>)>,
    worklist: Vec<Instance<'a>>,
}

pub fn to_c(prog: &Program) -> Result<String, Error> {
    let mut g = Gen {
        structs: HashMap::new(),
        funcs: HashMap::new(),
        sigs: HashMap::new(),
        type_env: HashMap::new(),
        frames: Vec::new(),
        next_id: 0,
        recursion: None,
        out: Out::default(),
        // Arrays the runtime itself uses.
        c_types: HashMap::from([
            ("Ai".to_string(), "lm_arr".to_string()),
            ("As".to_string(), "lm_As".to_string()),
            ("AAs".to_string(), "lm_AAs".to_string()),
        ]),
        helpers: HashSet::new(),
        wrapped: HashSet::new(),
        regexes: HashMap::new(),
        instances: HashMap::new(),
        worklist: Vec::new(),
    };

    for s in &prog.structs {
        if BUILTINS.contains(&s.name.as_str()) || g.structs.insert(&s.name, s).is_some() {
            return Err(Error::at(s.line, format!("type `{}` is defined twice or shadows a builtin", s.name)));
        }
    }
    for s in &prog.structs {
        let env: HashMap<String, Type> = s.params.iter().map(|p| (p.clone(), Type::Param(p.as_str().into()))).collect();
        for (_, t) in &s.fields {
            g.resolve_in(t, &env, s.line)?;
        }
    }
    check_struct_cycles(&prog.structs)?;

    for f in &prog.funcs {
        if BUILTINS.contains(&f.name.as_str()) {
            return Err(Error::at(f.line, format!("`{}` is a builtin and cannot be redefined", f.name)));
        }
        if g.structs.contains_key(f.name.as_str()) || g.funcs.insert(&f.name, f).is_some() {
            return Err(Error::at(f.line, format!("`{}` is defined twice", f.name)));
        }
        if f.type_params.is_empty() {
            let sig = g.resolve_sig(f, &HashMap::new())?;
            g.sigs.insert(&f.name, sig);
        }
    }
    match g.sigs.get("main") {
        Some(sig) if sig.params.is_empty() && sig.ret == Type::Unit => {}
        Some(_) => return Err(Error::msg("`main` must take no parameters and return nothing")),
        None if g.funcs.contains_key("main") => return Err(Error::msg("`main` cannot be generic")),
        None => return Err(Error::msg("no `main` function")),
    }

    for f in prog.funcs.iter().filter(|f| f.type_params.is_empty()) {
        let depth = unroll_depth(f);
        for level in 0..=depth {
            let proto = g.prototype(f, level);
            g.out.protos.push_str(&format!("{proto};\n"));
        }
        // f_name -> u{depth}_name -> ... -> u1_name -> f_name. clang inlines the
        // clones back into f_name, which exposes repeated pure calls (fib(n-3)
        // appears twice once fib is unrolled) to common-subexpression elimination.
        for level in 0..=depth {
            let target = if level == 0 { depth } else { level - 1 };
            g.recursion = Some((f.name.as_str(), c_name(f, target)));
            let body = g.func(f, level)?;
            g.out.bodies.push('\n');
            g.out.bodies.push_str(&body);
        }
    }
    g.recursion = None;
    while let Some(inst) = g.worklist.pop() {
        let body = g.instance(&inst)?;
        g.out.bodies.push('\n');
        g.out.bodies.push_str(&body);
    }

    let o = &g.out;
    Ok(format!(
        "{RUNTIME}\n{}{}{}{}{}{}{}\n\
         // Not inlined, so every frame the GC scans lies below main's frame.\n\
         static __attribute__((noinline)) void lm_start(void) {{ f_main(); }}\n\n\
         int main(void) {{\n    lm_gc_init(__builtin_frame_address(0));\n    lm_start();\n    return 0;\n}}\n",
        o.struct_fwd, o.array_types, o.struct_bodies, o.decls, o.protos, o.bodies, o.lifted
    ))
}

/// Rejects structs that contain themselves by value (directly or through other
/// structs), which would have infinite size. Containing themselves in an array is fine.
fn check_struct_cycles(structs: &[StructDef]) -> Result<(), Error> {
    fn by_value(t: &Type, out: &mut Vec<String>) {
        if let Type::Named(name, args) = t {
            out.push(name.to_string());
            args.iter().for_each(|a| by_value(a, out));
        }
    }
    let defs: HashMap<&str, &StructDef> = structs.iter().map(|s| (s.name.as_str(), s)).collect();
    fn visit<'a>(name: &'a str, defs: &HashMap<&'a str, &'a StructDef>, path: &mut Vec<&'a str>) -> Result<(), Error> {
        let Some(def) = defs.get(name) else { return Ok(()) };
        if path.contains(&name) {
            return Err(Error::at(def.line, format!("type `{name}` contains itself and would have infinite size; use an array")));
        }
        path.push(&def.name);
        let mut inner = Vec::new();
        def.fields.iter().for_each(|(_, t)| by_value(t, &mut inner));
        for n in &inner {
            if let Some((k, _)) = defs.get_key_value(n.as_str()) {
                visit(k, defs, path)?;
            }
        }
        path.pop();
        Ok(())
    }
    for s in structs {
        visit(&s.name, &defs, &mut Vec::new())?;
    }
    Ok(())
}

/// A unique, C-identifier-safe encoding of a type.
fn mangle(t: &Type) -> String {
    match t {
        Type::Int => "i".into(),
        Type::Float => "f".into(),
        Type::Bool => "b".into(),
        Type::Bytes => "y".into(),
        Type::Str => "s".into(),
        Type::Regex => "r".into(),
        Type::Unit => "u".into(),
        Type::Never => "n".into(),
        Type::Array(e) => format!("A{}", mangle(e)),
        Type::Struct(s) => {
            let args: String = s.args.iter().map(mangle).collect();
            format!("S{}{}{args}", s.name.len(), s.name)
        }
        Type::Fn(sig) => {
            let params: String = sig.params.iter().map(mangle).collect();
            format!("F{}{params}{}", sig.params.len(), mangle(&sig.ret))
        }
        Type::Param(n) | Type::Named(n, _) => format!("P{}{n}", n.len()),
    }
}

/// The C name of `f` at an unroll level; level 0 is the function itself.
fn c_name(f: &Func, level: usize) -> String {
    if level == 0 {
        format!("f_{}", f.name)
    } else {
        format!("u{level}_{}", f.name)
    }
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
            Stmt::Let { value, .. } => visit_expr(value, f),
            Stmt::Assign { target, value, .. } => {
                visit_expr(target, f);
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
        ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Regex(_) | ExprKind::Bool(_) | ExprKind::Var(_) => {}
        ExprKind::Slice(xs, lo, hi) => {
            visit_expr(xs, f);
            lo.iter().chain(hi).for_each(|e| visit_expr(e, f));
        }
        ExprKind::Unary(_, x) | ExprKind::Lambda(_, x) | ExprKind::Field(x, _) => visit_expr(x, f),
        ExprKind::Binary(_, l, r) | ExprKind::Index(l, r) => {
            visit_expr(l, f);
            visit_expr(r, f);
        }
        ExprKind::Call(_, args) | ExprKind::Array(args) => args.iter().for_each(|a| visit_expr(a, f)),
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

fn expect(line: usize, what: &str, want: &Type, got: &Type) -> Result<(), Error> {
    if unify(want, got).as_ref() == Some(want) {
        Ok(())
    } else {
        Err(Error::at(line, format!("{what}: expected {want}, found {got}")))
    }
}

/// Binds the type parameters in `param` so that it equals `arg`; false on a mismatch.
fn bind(param: &Type, arg: &Type, b: &mut HashMap<String, Type>) -> bool {
    match (param, arg) {
        (_, Type::Never) => true,
        (Type::Param(n), _) => match b.get(&**n) {
            Some(t) => t == arg,
            None => {
                b.insert(n.to_string(), arg.clone());
                true
            }
        },
        (Type::Array(p), Type::Array(a)) => bind(p, a, b),
        (Type::Fn(p), Type::Fn(a)) => {
            p.params.len() == a.params.len()
                && p.params.iter().zip(&a.params).all(|(p, a)| bind(p, a, b))
                && bind(&p.ret, &a.ret, b)
        }
        (Type::Struct(p), Type::Struct(a)) => {
            p.name == a.name && p.args.iter().zip(&a.args).all(|(p, a)| bind(p, a, b))
        }
        _ => param == arg,
    }
}

/// `t` with its type parameters replaced by their bindings.
fn subst(t: &Type, b: &HashMap<String, Type>) -> Type {
    match t {
        Type::Param(n) => b.get(&**n).cloned().unwrap_or_else(|| t.clone()),
        Type::Array(e) => Type::array(subst(e, b)),
        Type::Fn(sig) => Type::Fn(Rc::new(FnSig {
            params: sig.params.iter().map(|p| subst(p, b)).collect(),
            ret: subst(&sig.ret, b),
        })),
        Type::Struct(s) => Type::Struct(Rc::new(StructRef {
            name: s.name.clone(),
            args: s.args.iter().map(|a| subst(a, b)).collect(),
        })),
        _ => t.clone(),
    }
}

fn has_params(t: &Type) -> bool {
    match t {
        Type::Param(_) => true,
        Type::Array(e) => has_params(e),
        Type::Fn(sig) => sig.params.iter().any(has_params) || has_params(&sig.ret),
        Type::Struct(s) => s.args.iter().any(has_params),
        _ => false,
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
        Stmt::Assign { target: Expr { kind: ExprKind::Var(name), .. }, value, .. } => match &value.kind {
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
    format!("((lm_str){{ {}, (const uint8_t *)\"{}\" }})", bytes.len(), c_chars(bytes))
}

fn c_chars(bytes: &[u8]) -> String {
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
    lit
}

/// Whether evaluating `e` has no side effects, so its order relative to other
/// operands cannot be observed. Division and indexing count as pure: they can
/// only stop the program, which an earlier operand cannot observe either.
fn is_pure(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Str(_) | ExprKind::Regex(_) | ExprKind::Bool(_) | ExprKind::Var(_) => true,
        ExprKind::Lambda(..) => true,
        ExprKind::Unary(_, x) | ExprKind::Field(x, _) => is_pure(x),
        ExprKind::Binary(_, l, r) | ExprKind::Index(l, r) => is_pure(l) && is_pure(r),
        ExprKind::Slice(x, lo, hi) => is_pure(x) && lo.iter().chain(hi).all(|e| is_pure(e)),
        ExprKind::Array(items) => items.iter().all(is_pure),
        ExprKind::Call(..) | ExprKind::CallExpr(..) | ExprKind::If(..) | ExprKind::Block(_) => false,
    }
}

/// `({ prelude code; })`, or just `code` without a prelude.
fn wrap(prelude: &str, code: String) -> String {
    if prelude.is_empty() { code } else { format!("({{ {prelude}{code}; }})") }
}

/// Checks index `i{id}` against the length of `a{id}`.
fn bounds_check(id: usize, line: usize) -> String {
    format!("if (__builtin_expect((uint64_t)i{id} >= (uint64_t)a{id}.len, 0)) lm_panic_index(i{id}, a{id}.len, {line});")
}

/// The element type of an indexable value: arrays hold their element type,
/// `bytes` and `str` hold bytes, read as `int`.
fn elem_type(line: usize, t: &Type) -> Result<Type, Error> {
    match t {
        Type::Array(e) => Ok((**e).clone()),
        Type::Bytes | Type::Str => Ok(Type::Int),
        _ => Err(Error::at(line, format!("expected an array, bytes or str, found {t}"))),
    }
}

/// The runtime's number for a look-around assertion (`LM_LOOK_*` in runtime.c).
fn look_code(look: regex_syntax::hir::Look) -> u8 {
    use regex_syntax::hir::Look::*;
    match look {
        Start => 0,
        End => 1,
        StartLF => 2,
        EndLF => 3,
        StartCRLF => 4,
        EndCRLF => 5,
        WordAscii => 6,
        WordAsciiNegate => 7,
        WordUnicode => 8,
        WordUnicodeNegate => 9,
        WordStartAscii => 10,
        WordEndAscii => 11,
        WordStartUnicode => 12,
        WordEndUnicode => 13,
        WordStartHalfAscii => 14,
        WordEndHalfAscii => 15,
        WordStartHalfUnicode => 16,
        WordEndHalfUnicode => 17,
    }
}

fn fn_type(params: Vec<Type>, ret: Type) -> Type {
    Type::Fn(Rc::new(FnSig { params, ret }))
}

impl<'a> Gen<'a> {
    fn fresh(&mut self) -> usize {
        self.next_id += 1;
        self.next_id
    }

    fn frame(&mut self) -> &mut Frame {
        self.frames.last_mut().unwrap()
    }

    // ---- types ----------------------------------------------------------------

    fn resolve(&self, t: &Type, line: usize) -> Result<Type, Error> {
        self.resolve_in(t, &self.type_env, line)
    }

    /// Replaces `Named` types with the struct or type parameter they name.
    fn resolve_in(&self, t: &Type, env: &HashMap<String, Type>, line: usize) -> Result<Type, Error> {
        Ok(match t {
            Type::Named(name, args) => {
                if let Some(bound) = env.get(&**name) {
                    if !args.is_empty() {
                        return Err(Error::at(line, format!("type parameter `{name}` takes no type arguments")));
                    }
                    return Ok(bound.clone());
                }
                let Some(def) = self.structs.get(&**name) else {
                    return Err(Error::at(line, format!("unknown type `{name}`")));
                };
                if args.len() != def.params.len() {
                    return Err(Error::at(
                        line,
                        format!("type `{name}` takes {} type argument(s), got {}", def.params.len(), args.len()),
                    ));
                }
                let args = args.iter().map(|a| self.resolve_in(a, env, line)).collect::<Result<_, _>>()?;
                Type::Struct(Rc::new(StructRef { name: name.to_string(), args }))
            }
            Type::Array(e) => Type::array(self.resolve_in(e, env, line)?),
            Type::Fn(sig) => fn_type(
                sig.params.iter().map(|p| self.resolve_in(p, env, line)).collect::<Result<_, _>>()?,
                self.resolve_in(&sig.ret, env, line)?,
            ),
            _ => t.clone(),
        })
    }

    fn resolve_sig(&self, f: &Func, env: &HashMap<String, Type>) -> Result<Rc<FnSig>, Error> {
        let params = f.params.iter().map(|(_, t)| self.resolve_in(t, env, f.line)).collect::<Result<_, _>>()?;
        Ok(Rc::new(FnSig { params, ret: self.resolve_in(&f.ret, env, f.line)? }))
    }

    /// The fields of a struct type, with its type arguments substituted.
    fn fields(&self, s: &StructRef) -> Vec<(String, Type)> {
        let def = self.structs[s.name.as_str()];
        let env: HashMap<String, Type> = def.params.iter().cloned().zip(s.args.iter().cloned()).collect();
        def.fields
            .iter()
            .map(|(n, t)| (n.clone(), self.resolve_in(t, &env, def.line).expect("checked when the struct was declared")))
            .collect()
    }

    /// The C type for `t`, defining array and struct types on first use.
    fn c_type(&mut self, t: &Type) -> String {
        match t {
            Type::Int => "int64_t".into(),
            Type::Float => "double".into(),
            Type::Bool => "bool".into(),
            Type::Bytes => "lm_bytes".into(),
            Type::Str => "lm_str".into(),
            Type::Regex => "const lm_re *".into(),
            Type::Fn(_) => "lm_fn".into(),
            Type::Unit | Type::Never => "void".into(),
            Type::Array(e) => {
                let m = mangle(t);
                if let Some(name) = self.c_types.get(&m) {
                    return name.clone();
                }
                let name = format!("lm_{m}");
                let elem = self.c_type(e);
                self.out.array_types.push_str(&format!("typedef struct {{ int64_t len; {elem} *data; }} {name};\n"));
                self.c_types.insert(m, name.clone());
                name
            }
            Type::Struct(s) => {
                let m = mangle(t);
                if let Some(name) = self.c_types.get(&m) {
                    return name.clone();
                }
                let name = format!("lm_{m}");
                // Registered before its fields, so an array of this struct inside it resolves.
                self.c_types.insert(m, name.clone());
                self.out.struct_fwd.push_str(&format!("typedef struct {name} {name};\n"));
                let mut body = format!("struct {name} {{");
                let fields = self.fields(s);
                for (f, ft) in &fields {
                    let ct = self.c_type(ft);
                    body.push_str(&format!(" {ct} m_{f};"));
                }
                if fields.is_empty() {
                    body.push_str(" char unused;");
                }
                self.out.struct_bodies.push_str(&format!("{body} }};\n"));
                name
            }
            Type::Param(_) | Type::Named(..) => unreachable!("unresolved type {t}"),
        }
    }

    /// A zero value of type `t` in C, for code that is never reached.
    fn zero(&mut self, t: &Type) -> String {
        match t {
            Type::Int => "0".into(),
            Type::Float => "0.0".into(),
            Type::Bool => "false".into(),
            Type::Regex => "(const lm_re *)0".into(),
            Type::Unit | Type::Never => String::new(),
            _ => format!("({}){{0}}", self.c_type(t)),
        }
    }

    /// Wraps an expression that never produces a value so it type-checks in C
    /// where a value of type `to` is expected.
    fn coerce(&mut self, code: String, from: &Type, to: &Type) -> String {
        if *from == Type::Never && !matches!(to, Type::Unit | Type::Never) {
            let zero = self.zero(to);
            format!("({{ {code}; __builtin_unreachable(); {zero}; }})")
        } else {
            code
        }
    }

    /// Whether values of `t` can point into the GC heap, so arrays of them must be scanned.
    fn has_pointers(&self, t: &Type) -> bool {
        match t {
            Type::Str | Type::Bytes | Type::Array(_) | Type::Fn(_) => true,
            Type::Struct(s) => self.fields(s).iter().any(|(_, t)| self.has_pointers(t)),
            _ => false,
        }
    }

    /// The C type of a pointer to the lifted function behind a value of type `sig`.
    fn c_fn_ptr(&mut self, sig: &FnSig) -> String {
        let mut params = String::from("void *");
        for p in &sig.params {
            params.push_str(", ");
            params.push_str(&self.c_type(p));
        }
        format!("{} (*)({params})", self.c_type(&sig.ret))
    }

    fn c_params(&mut self, params: &[Type]) -> String {
        let params: Vec<String> = params.iter().enumerate().map(|(i, t)| format!("{} p{i}", self.c_type(t))).collect();
        if params.is_empty() { "void".to_string() } else { params.join(", ") }
    }

    fn prototype(&mut self, f: &Func, level: usize) -> String {
        let sig = self.sigs[f.name.as_str()].clone();
        let inline = if level > 0 { "inline " } else { "" };
        let params = self.c_params(&sig.params);
        format!("static {inline}{} {}({params})", self.c_type(&sig.ret), c_name(f, level))
    }

    // ---- functions ------------------------------------------------------------

    fn func(&mut self, f: &Func, level: usize) -> Result<String, Error> {
        let sig = self.sigs[f.name.as_str()].clone();
        let body = self.function_body(f, &sig)?;
        Ok(format!("{} {{\n{body}}}\n", self.prototype(f, level)))
    }

    fn instance(&mut self, inst: &Instance<'a>) -> Result<String, Error> {
        self.type_env = inst.bindings.clone();
        let body = self.function_body(inst.func, &inst.sig);
        self.type_env.clear();
        let params = self.c_params(&inst.sig.params);
        Ok(format!("static {} {}({params}) {{\n{}}}\n", self.c_type(&inst.sig.ret), inst.cname, body?))
    }

    fn function_body(&mut self, f: &Func, sig: &FnSig) -> Result<String, Error> {
        self.frames.push(Frame::new(Some(sig.ret.clone())));
        let params: Vec<(String, Type)> = f.params.iter().map(|(n, _)| n.clone()).zip(sig.params.iter().cloned()).collect();
        let header = self.bind_params(&params);
        let result = self.block(&f.body, Some(&sig.ret));
        self.frames.pop();
        let (body, ty) = result?;
        expect(f.line, &format!("body of `{}`", f.name), &sig.ret, &ty)?;
        Ok(if sig.ret == Type::Unit {
            format!("{header}    {body};\n")
        } else {
            format!("{header}    return {};\n", self.coerce(body, &ty, &sig.ret))
        })
    }

    /// Declares parameters `p0..pN` as locals of the current frame.
    fn bind_params(&mut self, params: &[(String, Type)]) -> String {
        let mut header = String::new();
        for (i, (name, ty)) in params.iter().enumerate() {
            let cname = self.declare(name, ty.clone(), false);
            let ct = self.c_type(ty);
            header.push_str(&format!("    {ct} {cname} = p{i};\n"));
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

    // ---- statements -----------------------------------------------------------

    fn block(&mut self, b: &Block, hint: Option<&Type>) -> Result<(String, Type), Error> {
        self.frame().scopes.push(HashMap::new());
        let result = self.block_inner(b, hint);
        self.frame().scopes.pop();
        result
    }

    fn block_inner(&mut self, b: &Block, hint: Option<&Type>) -> Result<(String, Type), Error> {
        let mut code = String::from("({ ");
        for s in &b.stmts {
            code.push_str(&self.stmt(s)?);
            code.push(' ');
        }
        let ty = match &b.tail {
            Some(e) => {
                let (c, t) = self.expr_hint(e, hint)?;
                code.push_str(&c);
                code.push_str("; ");
                t
            }
            None if matches!(b.stmts.last(), Some(Stmt::Return { .. })) => Type::Never,
            None => Type::Unit,
        };
        code.push_str("})");
        Ok((code, ty))
    }

    fn stmt(&mut self, s: &Stmt) -> Result<String, Error> {
        match s {
            Stmt::Let { name, mutable, ty, value, line } => {
                let annotated = ty.as_ref().map(|t| self.resolve(t, *line)).transpose()?;
                let (code, vt) = self.expr_hint(value, annotated.as_ref())?;
                let ty = match annotated {
                    Some(t) => {
                        expect(*line, &format!("value of `{name}`"), &t, &vt)?;
                        t
                    }
                    None => vt.clone(),
                };
                if matches!(ty, Type::Unit | Type::Never) {
                    return Err(Error::at(*line, format!("cannot bind `{name}` to a value of type {ty}")));
                }
                let code = self.coerce(code, &vt, &ty);
                let ct = self.c_type(&ty);
                let cname = self.declare(name, ty, *mutable);
                Ok(format!("{ct} {cname} = {code};"))
            }
            Stmt::Assign { target, value, line } => {
                // Evaluated left to right: the target's array and index, then the value.
                let (prelude, lvalue, tt) = self.place(target, *line)?;
                let (v, vt) = self.expr_hint(value, Some(&tt))?;
                expect(*line, "assigned value", &tt, &vt)?;
                let v = self.coerce(v, &vt, &tt);
                let id = self.fresh();
                let ct = self.c_type(&tt);
                Ok(format!("{{ {prelude} {ct} v{id} = {v}; {lvalue} = v{id}; }}"))
            }
            Stmt::While { cond, body, line } => {
                let (c, ct) = self.expr(cond)?;
                expect(*line, "`while` condition", &Type::Bool, &ct)?;
                let (b, bt) = self.block(body, None)?;
                expect(*line, "`while` body", &Type::Unit, &bt)?;
                Ok(format!("_Pragma(\"clang loop {}\") while ({c}) {{ {b}; }}", loop_hint(body)))
            }
            Stmt::Return { value, line } => {
                let hint = self.frame().ret.clone();
                let (c, t) = match value {
                    Some(e) => self.expr_hint(e, hint.as_ref())?,
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
                    _ => format!("return {};", self.coerce(c, &t, &ret)),
                })
            }
            Stmt::Expr(e) => Ok(format!("{};", self.expr(e)?.0)),
        }
    }

    /// An assignable place: returns a prelude that evaluates its parts, the C
    /// lvalue, and its type. Variables (and fields of them) must be `be mut`;
    /// array elements can always be assigned, since arrays are shared.
    fn place(&mut self, e: &Expr, line: usize) -> Result<(String, String, Type), Error> {
        match &e.kind {
            ExprKind::Var(name) => {
                let local = self.lookup(name).ok_or_else(|| Error::at(line, format!("unknown variable `{name}`")))?;
                if local.captured {
                    return Err(Error::at(line, format!("cannot assign to `{name}`: closures capture variables by value")));
                }
                if !local.mutable {
                    return Err(Error::at(line, format!("cannot assign twice to immutable `{name}` (use `be mut`)")));
                }
                Ok((String::new(), local.cname, local.ty))
            }
            ExprKind::Field(base, field) => {
                if !matches!(base.kind, ExprKind::Var(_) | ExprKind::Field(..) | ExprKind::Index(..)) {
                    return Err(Error::at(line, "cannot assign to a field of a temporary value"));
                }
                let (prelude, lvalue, bt) = self.place(base, line)?;
                let ft = self.field_type(&bt, field, line)?;
                Ok((prelude, format!("{lvalue}.m_{field}"), ft))
            }
            ExprKind::Index(arr, index) => {
                let (a, at) = self.expr(arr)?;
                let (i, it) = self.expr(index)?;
                let et = elem_type(line, &at)?;
                if at == Type::Str {
                    return Err(Error::at(line, "strings are immutable"));
                }
                expect(line, "array index", &Type::Int, &it)?;
                // Stores into `bytes` truncate to 0..=255, like Rust's `as u8`.
                let id = self.fresh();
                let ct = self.c_type(&at);
                let prelude = format!("{ct} a{id} = {a}; int64_t i{id} = {i}; {}", bounds_check(id, line));
                Ok((prelude, format!("a{id}.data[i{id}]"), et))
            }
            _ => Err(Error::at(line, "invalid assignment target")),
        }
    }

    fn field_type(&self, t: &Type, field: &str, line: usize) -> Result<Type, Error> {
        let Type::Struct(s) = t else {
            return Err(Error::at(line, format!("type {t} has no field `{field}`")));
        };
        let fields = self.fields(s);
        fields.iter().find(|(f, _)| f == field).map(|(_, t)| t.clone()).ok_or_else(|| {
            let names: Vec<&str> = fields.iter().map(|(f, _)| f.as_str()).collect();
            Error::at(line, format!("type {t} has no field `{field}` (its fields: {})", names.join(", ")))
        })
    }

    // ---- expressions ----------------------------------------------------------

    fn expr(&mut self, e: &Expr) -> Result<(String, Type), Error> {
        self.expr_hint(e, None)
    }

    /// Compiles `e`; `hint` is the type the context expects, which lets `[]` and
    /// `array(n)` know their element type.
    fn expr_hint(&mut self, e: &Expr, hint: Option<&Type>) -> Result<(String, Type), Error> {
        let line = e.line;
        Ok(match &e.kind {
            ExprKind::Int(n) => (format!("INT64_C({n})"), Type::Int),
            // `{:?}` is the shortest form that round-trips, and always has a `.` or exponent.
            ExprKind::Float(x) => (format!("{x:?}"), Type::Float),
            ExprKind::Str(bytes) => (c_string(bytes), Type::Str),
            ExprKind::Regex(pattern) => (format!("(&{})", self.regex(pattern, line)?), Type::Regex),
            ExprKind::Bool(b) => (b.to_string(), Type::Bool),
            ExprKind::Var(name) => {
                if let Some(local) = self.lookup(name) {
                    (local.cname, local.ty)
                } else if self.sigs.contains_key(name.as_str()) {
                    self.function_value(name)
                } else if let Some(f) = self.funcs.get(name.as_str()).copied() {
                    match hint {
                        Some(Type::Fn(expected)) => self.generic_value(f, expected, line)?,
                        _ => {
                            return Err(Error::at(
                                line,
                                format!(
                                    "cannot infer the type parameters of `{name}` here; give the value a function type \
                                     (`be f: fn(int) -> int = {name};`) or wrap it in a lambda"
                                ),
                            ))
                        }
                    }
                } else if BUILTINS.contains(&name.as_str()) || self.structs.contains_key(name.as_str()) {
                    return Err(Error::at(line, format!("`{name}` cannot be used as a value; wrap it in a lambda")));
                } else {
                    return Err(Error::at(line, format!("unknown variable `{name}`")));
                }
            }
            ExprKind::Unary(op, operand) => {
                let (c, t) = self.expr(operand)?;
                match op {
                    UnOp::Neg => {
                        if t != Type::Float {
                            expect(line, "operand of `-`", &Type::Int, &t)?;
                        }
                        (format!("(-{c})"), t)
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
                // `&&` and `||` are sequenced in C already; other operators are not.
                let (prelude, l, r) = if matches!(op, BinOp::And | BinOp::Or) || (is_pure(lhs) && is_pure(rhs)) {
                    (String::new(), l, r)
                } else {
                    let id = self.fresh();
                    let (lc, rc) = (self.c_type(&lt), self.c_type(&rt));
                    (format!("{lc} l{id} = {l}; {rc} r{id} = {r};"), format!("l{id}"), format!("r{id}"))
                };
                let (code, ty) = self.binary(*op, &l, &lt, &r, &rt, line)?;
                (wrap(&prelude, code), ty)
            }
            ExprKind::Call(name, args) => {
                if let Some(local) = self.lookup(name) {
                    self.indirect_call(&local.cname, &local.ty, args, line)?
                } else if self.structs.contains_key(name.as_str()) {
                    self.construct(name, args, line)?
                } else if let Some(f) = self.funcs.get(name.as_str()).copied() {
                    if f.type_params.is_empty() {
                        self.call(f, args, line)?
                    } else {
                        self.generic_call(f, args, line)?
                    }
                } else if BUILTINS.contains(&name.as_str()) {
                    self.builtin_call(name, args, hint, line)?
                } else {
                    return Err(Error::at(line, format!("unknown function `{name}`")));
                }
            }
            ExprKind::CallExpr(callee, args) => {
                let (c, t) = self.expr(callee)?;
                self.indirect_call(&c, &t, args, line)?
            }
            ExprKind::Lambda(params, body) => self.lambda(params, body, line)?,
            ExprKind::Slice(xs, lo, hi) => {
                let (x, xt) = self.expr(xs)?;
                elem_type(line, &xt)?;
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
                let ct = self.c_type(&xt);
                let code = format!(
                    "({{ {ct} s{id} = {x}; int64_t lo{id} = {lo}; int64_t hi{id} = {hi}; \
                     if (__builtin_expect((uint64_t)hi{id} > (uint64_t)s{id}.len || (uint64_t)lo{id} > (uint64_t)hi{id}, 0)) \
                     lm_panic_slice(lo{id}, hi{id}, s{id}.len, {line}); \
                     ({ct}){{ hi{id} - lo{id}, s{id}.data + lo{id} }}; }})"
                );
                (code, xt)
            }
            ExprKind::Index(arr, index) => {
                let (a, at) = self.expr(arr)?;
                let (i, it) = self.expr(index)?;
                let et = elem_type(line, &at)?;
                expect(line, "array index", &Type::Int, &it)?;
                let id = self.fresh();
                let cast = if matches!(at, Type::Array(_)) { "" } else { "(int64_t)" };
                let ct = self.c_type(&at);
                let code = format!(
                    "({{ {ct} a{id} = {a}; int64_t i{id} = {i}; {} {cast}a{id}.data[i{id}]; }})",
                    bounds_check(id, line)
                );
                (code, et)
            }
            ExprKind::Field(base, field) => {
                let (b, bt) = self.expr(base)?;
                let ft = self.field_type(&bt, field, line)?;
                (format!("({b}).m_{field}"), ft)
            }
            ExprKind::Array(items) => self.array_literal(items, hint, line)?,
            ExprKind::If(cond, then, otherwise) => {
                let (c, ct) = self.expr(cond)?;
                expect(line, "`if` condition", &Type::Bool, &ct)?;
                let (t, tt) = self.block(then, hint)?;
                match otherwise {
                    Some(otherwise) => {
                        let (o, ot) = self.block(otherwise, hint.or(Some(&tt)))?;
                        let ty = unify(&tt, &ot).ok_or_else(|| {
                            Error::at(line, format!("`if` and `else` have different types: {tt} and {ot}"))
                        })?;
                        let (t, o) = (self.coerce(t, &tt, &ty), self.coerce(o, &ot, &ty));
                        (format!("({c} ? {t} : {o})"), ty)
                    }
                    None => {
                        expect(line, "`if` without `else`", &Type::Unit, &tt)?;
                        (format!("({{ if ({c}) {t}; }})"), Type::Unit)
                    }
                }
            }
            ExprKind::Block(b) => self.block(b, hint)?,
        })
    }

    fn array_literal(&mut self, items: &[Expr], hint: Option<&Type>, line: usize) -> Result<(String, Type), Error> {
        let elem_hint = match hint {
            Some(Type::Array(e)) => Some((**e).clone()),
            _ => None,
        };
        let mut compiled = Vec::with_capacity(items.len());
        for item in items {
            compiled.push(self.expr_hint(item, elem_hint.as_ref())?);
        }
        let elem = match (&elem_hint, compiled.first()) {
            (Some(e), _) => e.clone(),
            (None, Some((_, t))) => t.clone(),
            (None, None) => {
                return Err(Error::at(line, "cannot infer the element type of `[]`; annotate it, as in `be xs: [int] = [];`"))
            }
        };
        let id = self.fresh();
        let (ec, ac) = (self.c_type(&elem), self.c_type(&Type::array(elem.clone())));
        let scan = self.has_pointers(&elem) as u8;
        let mut code = format!("({{ {ec} *d{id} = lm_alloc({}, sizeof({ec}), {scan}); ", items.len());
        for (k, (c, t)) in compiled.into_iter().enumerate() {
            expect(line, &format!("array element {}", k + 1), &elem, &t)?;
            let c = self.coerce(c, &t, &elem);
            code.push_str(&format!("d{id}[{k}] = {c}; "));
        }
        code.push_str(&format!("({ac}){{ {}, d{id} }}; }})", items.len()));
        Ok((code, Type::array(elem)))
    }

    fn binary(&mut self, op: BinOp, l: &str, lt: &Type, r: &str, rt: &Type, line: usize) -> Result<(String, Type), Error> {
        let what = format!("operand of `{}`", op.c_op());
        if matches!(op, BinOp::Eq | BinOp::Ne) {
            expect(line, &what, lt, rt)?;
            let eq = self.eq_code(lt, l, r, line)?;
            let code = if matches!(op, BinOp::Eq) { eq } else { format!("(!{eq})") };
            return Ok((code, Type::Bool));
        }
        if *lt == Type::Str {
            expect(line, &what, &Type::Str, rt)?;
            return Ok(match op {
                BinOp::Add => (format!("lm_concat({l}, {r})"), Type::Str),
                BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => (format!("(lm_str_cmp({l}, {r}) {} 0)", op.c_op()), Type::Bool),
                _ => return Err(Error::at(line, format!("`{}` is not defined for str", op.c_op()))),
            });
        }
        if *lt == Type::Float {
            // IEEE semantics, as in Rust: division by zero gives inf or NaN.
            expect(line, &what, &Type::Float, rt)?;
            return Ok(match op {
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => (format!("({l} {} {r})", op.c_op()), Type::Float),
                BinOp::Rem => (format!("fmod({l}, {r})"), Type::Float),
                BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => (format!("({l} {} {r})", op.c_op()), Type::Bool),
                _ => return Err(Error::at(line, format!("{what}: expected bool, found float"))),
            });
        }
        if matches!(op, BinOp::Div | BinOp::Rem) {
            expect(line, &what, &Type::Int, lt)?;
            expect(line, &what, &Type::Int, rt)?;
            // Division by zero stops the program; MIN / -1 wraps like the other operators.
            let id = self.fresh();
            let wrapped = if matches!(op, BinOp::Div) { format!("-n{id}") } else { "0".to_string() };
            return Ok((
                format!(
                    "({{ int64_t n{id} = {l}; int64_t d{id} = {r}; if (__builtin_expect(d{id} == 0, 0)) lm_panic_div({line}); \
                     d{id} == -1 ? {wrapped} : n{id} {} d{id}; }})",
                    op.c_op()
                ),
                Type::Int,
            ));
        }
        let (want, ty) = match op {
            BinOp::Add | BinOp::Sub | BinOp::Mul => (Type::Int, Type::Int),
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => (Type::Int, Type::Bool),
            _ => (Type::Bool, Type::Bool),
        };
        expect(line, &what, &want, lt)?;
        expect(line, &what, &want, rt)?;
        Ok((format!("({l} {} {r})", op.c_op()), ty))
    }

    /// C code comparing `a` and `b` of type `t` for equality.
    fn eq_code(&mut self, t: &Type, a: &str, b: &str, line: usize) -> Result<String, Error> {
        Ok(match t {
            Type::Int | Type::Float | Type::Bool => format!("({a} == {b})"),
            Type::Str => format!("lm_str_eq({a}, {b})"),
            Type::Array(_) | Type::Bytes | Type::Struct(_) => {
                let name = format!("lm_eq_{}", mangle(t));
                if self.helpers.insert(name.clone()) {
                    let ct = self.c_type(t);
                    let body = match t {
                        Type::Struct(s) => {
                            let mut parts = Vec::new();
                            for (f, ft) in self.fields(s) {
                                parts.push(self.eq_code(&ft, &format!("a.m_{f}"), &format!("b.m_{f}"), line)?);
                            }
                            format!("return {};", if parts.is_empty() { "true".to_string() } else { parts.join(" && ") })
                        }
                        _ => {
                            let et = elem_type(line, t)?;
                            let item = self.eq_code(&et, "a.data[k]", "b.data[k]", line)?;
                            format!(
                                "if (a.len != b.len) return false; \
                                 for (int64_t k = 0; k < a.len; k++) if (!{item}) return false; return true;"
                            )
                        }
                    };
                    self.out.decls.push_str(&format!("static bool {name}({ct} a, {ct} b);\n"));
                    self.out.lifted.push_str(&format!("\nstatic bool {name}({ct} a, {ct} b) {{ {body} }}\n"));
                }
                format!("{name}({a}, {b})")
            }
            _ => return Err(Error::at(line, format!("values of type {t} cannot be compared"))),
        })
    }

    /// The name of a `void (lm_buf *, T)` function that writes a value of `t`
    /// the way Rust's `{:?}` would (strings quoted).
    fn writer(&mut self, t: &Type, line: usize) -> Result<String, Error> {
        Ok(match t {
            Type::Int => "lm_w_int".into(),
            Type::Float => "lm_w_float".into(),
            Type::Bool => "lm_w_bool".into(),
            Type::Str => "lm_w_strq".into(),
            Type::Fn(_) => "lm_w_fn".into(),
            Type::Regex => "lm_w_re".into(),
            Type::Array(_) | Type::Bytes | Type::Struct(_) => {
                let name = format!("lm_w_{}", mangle(t));
                if self.helpers.insert(name.clone()) {
                    let ct = self.c_type(t);
                    let body = match t {
                        Type::Struct(s) => {
                            let fields = self.fields(s);
                            let mut body = format!("lm_buf_puts(b, \"{}", s.name);
                            if fields.is_empty() {
                                body.push_str("\");");
                            } else {
                                body.push_str(" { \");");
                                for (k, (f, ft)) in fields.iter().enumerate() {
                                    let w = self.writer(ft, line)?;
                                    let sep = if k == 0 { "" } else { ", " };
                                    body.push_str(&format!(" lm_buf_puts(b, \"{sep}{f}: \"); {w}(b, v.m_{f});"));
                                }
                                body.push_str(" lm_buf_puts(b, \" }\");");
                            }
                            body
                        }
                        _ => {
                            let w = self.writer(&elem_type(line, t)?, line)?;
                            format!(
                                "lm_buf_puts(b, \"[\"); for (int64_t k = 0; k < v.len; k++) {{ \
                                 if (k) lm_buf_puts(b, \", \"); {w}(b, v.data[k]); }} lm_buf_puts(b, \"]\");"
                            )
                        }
                    };
                    self.out.decls.push_str(&format!("static void {name}(lm_buf *b, {ct} v);\n"));
                    self.out.lifted.push_str(&format!("\nstatic void {name}(lm_buf *b, {ct} v) {{ {body} }}\n"));
                }
                name
            }
            _ => return Err(Error::at(line, format!("cannot print a value of type {t}"))),
        })
    }

    /// Compiles call arguments against `params`. Returns a prelude that evaluates them
    /// left to right (empty when that cannot matter) and the expressions to pass.
    fn args(&mut self, name: &str, params: &[Type], args: &[Expr], line: usize) -> Result<(String, Vec<String>), Error> {
        if args.len() != params.len() {
            return Err(Error::at(line, format!("`{name}` takes {} argument(s), got {}", params.len(), args.len())));
        }
        let mut compiled = Vec::with_capacity(args.len());
        for (i, (a, want)) in args.iter().zip(params).enumerate() {
            let (c, got) = self.expr_hint(a, Some(want))?;
            expect(line, &format!("argument {} of `{name}`", i + 1), want, &got)?;
            compiled.push((self.coerce(c, &got, want), want.clone()));
        }
        Ok(self.in_order(args, compiled))
    }

    /// Binds `compiled` (the code and type of each of `exprs`) to temporaries in order
    /// when any of them could have side effects, so C evaluates them left to right.
    fn in_order(&mut self, exprs: &[Expr], compiled: Vec<(String, Type)>) -> (String, Vec<String>) {
        if exprs.len() < 2 || exprs.iter().all(is_pure) {
            return (String::new(), compiled.into_iter().map(|(c, _)| c).collect());
        }
        let mut prelude = String::new();
        let mut names = Vec::new();
        for (c, t) in compiled {
            let name = format!("t{}", self.fresh());
            let ct = self.c_type(&t);
            prelude.push_str(&format!("{ct} {name} = {c}; "));
            names.push(name);
        }
        (prelude, names)
    }

    fn call(&mut self, f: &'a Func, args: &[Expr], line: usize) -> Result<(String, Type), Error> {
        let sig = self.sigs[f.name.as_str()].clone();
        let (prelude, args) = self.args(&f.name, &sig.params, args, line)?;
        let target = match &self.recursion {
            Some((current, target)) if *current == f.name => target.clone(),
            _ => format!("f_{}", f.name),
        };
        Ok((wrap(&prelude, format!("{target}({})", args.join(", "))), sig.ret.clone()))
    }

    /// Calls a generic function: infers its type parameters from the arguments and
    /// emits (once) the C function for that combination of types.
    fn generic_call(&mut self, f: &'a Func, args: &[Expr], line: usize) -> Result<(String, Type), Error> {
        let env: HashMap<String, Type> = f.type_params.iter().map(|p| (p.clone(), Type::Param(p.as_str().into()))).collect();
        let generic = self.resolve_sig(f, &env)?;
        if args.len() != generic.params.len() {
            return Err(Error::at(line, format!("`{}` takes {} argument(s), got {}", f.name, generic.params.len(), args.len())));
        }
        let mut bindings = HashMap::new();
        let mut compiled = Vec::with_capacity(args.len());
        for (i, (a, param)) in args.iter().zip(&generic.params).enumerate() {
            let partial = subst(param, &bindings);
            let names_generic_fn = matches!(&a.kind, ExprKind::Var(n) if self.funcs.get(n.as_str()).is_some_and(|f| !f.type_params.is_empty()));
            let hint = if !has_params(param) || names_generic_fn { Some(&partial) } else { None };
            let (c, got) = self.expr_hint(a, hint)?;
            if !bind(param, &got, &mut bindings) {
                let want = subst(param, &bindings);
                return Err(Error::at(line, format!("argument {} of `{}`: expected {want}, found {got}", i + 1, f.name)));
            }
            compiled.push((c, got));
        }
        if let Some(p) = f.type_params.iter().find(|p| !bindings.contains_key(*p)) {
            return Err(Error::at(line, format!("cannot infer type parameter `{p}` of `{}` from the arguments", f.name)));
        }
        let (cname, sig) = self.instantiate(f, &generic, bindings, line)?;
        let compiled: Vec<(String, Type)> = compiled
            .into_iter()
            .zip(&sig.params)
            .map(|((c, got), want)| (self.coerce(c, &got, want), want.clone()))
            .collect();
        let (prelude, codes) = self.in_order(args, compiled);
        Ok((wrap(&prelude, format!("{cname}({})", codes.join(", "))), sig.ret.clone()))
    }

    /// The C function for `f` with these type arguments, emitted once.
    fn instantiate(
        &mut self,
        f: &'a Func,
        generic: &FnSig,
        bindings: HashMap<String, Type>,
        line: usize,
    ) -> Result<(String, Rc<FnSig>), Error> {
        let sig = Rc::new(FnSig {
            params: generic.params.iter().map(|p| subst(p, &bindings)).collect(),
            ret: subst(&generic.ret, &bindings),
        });
        let mangled: String = f.type_params.iter().map(|p| mangle(&bindings[p])).collect();
        let key = format!("{}_{mangled}", f.name);
        if key.len() > MAX_INSTANCE_NAME {
            return Err(Error::at(line, format!("`{}` instantiates itself with ever larger types", f.name)));
        }
        if let Some((cname, sig)) = self.instances.get(&key) {
            return Ok((cname.clone(), sig.clone()));
        }
        let cname = format!("g_{key}");
        let params = self.c_params(&sig.params);
        let ret = self.c_type(&sig.ret);
        self.out.protos.push_str(&format!("static {ret} {cname}({params});\n"));
        self.instances.insert(key, (cname.clone(), sig.clone()));
        self.worklist.push(Instance { func: f, bindings, cname: cname.clone(), sig: sig.clone() });
        Ok((cname, sig))
    }

    /// A generic function used as a value: its type parameters come from the
    /// function type the context expects (the parts of it that are known).
    fn generic_value(&mut self, f: &'a Func, expected: &FnSig, line: usize) -> Result<(String, Type), Error> {
        let env: HashMap<String, Type> = f.type_params.iter().map(|p| (p.clone(), Type::Param(p.as_str().into()))).collect();
        let generic = self.resolve_sig(f, &env)?;
        let mismatch = || Error::at(line, format!("`{}` does not fit the expected type {}", f.name, Type::Fn(Rc::new(FnSig {
            params: expected.params.clone(),
            ret: expected.ret.clone(),
        }))));
        if generic.params.len() != expected.params.len() {
            return Err(mismatch());
        }
        let mut bindings = HashMap::new();
        for (g, e) in generic.params.iter().zip(&expected.params).chain([(&generic.ret, &expected.ret)]) {
            if !has_params(e) && !bind(g, e, &mut bindings) {
                return Err(mismatch());
            }
        }
        if let Some(p) = f.type_params.iter().find(|p| !bindings.contains_key(*p)) {
            return Err(Error::at(line, format!("cannot infer type parameter `{p}` of `{}` from the expected type", f.name)));
        }
        let (cname, sig) = self.instantiate(f, &generic, bindings, line)?;
        let wrapper = format!("w_{cname}");
        if self.wrapped.insert(wrapper.clone()) {
            let mut params = String::from("void *env");
            let mut args = Vec::new();
            for (i, p) in sig.params.iter().enumerate() {
                params.push_str(&format!(", {} p{i}", self.c_type(p)));
                args.push(format!("p{i}"));
            }
            let head = format!("static {} {wrapper}({params})", self.c_type(&sig.ret));
            let ret = if sig.ret == Type::Unit { "" } else { "return " };
            self.out.decls.push_str(&format!("{head};\n"));
            self.out.lifted.push_str(&format!("\n{head} {{ {ret}{cname}({}); }}\n", args.join(", ")));
        }
        Ok((format!("((lm_fn){{ (void *){wrapper}, NULL }})"), Type::Fn(sig)))
    }

    /// `Name(a, b)`: builds a struct from its fields in declaration order.
    fn construct(&mut self, name: &str, args: &[Expr], line: usize) -> Result<(String, Type), Error> {
        let def = self.structs[name];
        let env: HashMap<String, Type> = def.params.iter().map(|p| (p.clone(), Type::Param(p.as_str().into()))).collect();
        let generic: Vec<Type> =
            def.fields.iter().map(|(_, t)| self.resolve_in(t, &env, line)).collect::<Result<_, _>>()?;
        if args.len() != generic.len() {
            let names: Vec<&str> = def.fields.iter().map(|(f, _)| f.as_str()).collect();
            return Err(Error::at(
                line,
                format!("`{name}` takes {} field(s) ({}), got {}", generic.len(), names.join(", "), args.len()),
            ));
        }
        let mut bindings = HashMap::new();
        let mut compiled = Vec::with_capacity(args.len());
        for (a, (field, ft)) in args.iter().zip(def.fields.iter().map(|(f, _)| f).zip(&generic)) {
            let hint = (!has_params(ft)).then_some(ft);
            let (c, got) = self.expr_hint(a, hint)?;
            if !bind(ft, &got, &mut bindings) {
                let want = subst(ft, &bindings);
                return Err(Error::at(line, format!("field `{field}` of `{name}`: expected {want}, found {got}")));
            }
            compiled.push((c, got));
        }
        if let Some(p) = def.params.iter().find(|p| !bindings.contains_key(*p)) {
            return Err(Error::at(line, format!("cannot infer type parameter `{p}` of `{name}` from its fields")));
        }
        let ty = Type::Struct(Rc::new(StructRef {
            name: name.to_string(),
            args: def.params.iter().map(|p| bindings[p].clone()).collect(),
        }));
        let compiled: Vec<(String, Type)> = compiled
            .into_iter()
            .zip(&generic)
            .map(|((c, got), ft)| {
                let want = subst(ft, &bindings);
                (self.coerce(c, &got, &want), want)
            })
            .collect();
        let (prelude, codes) = self.in_order(args, compiled);
        let ct = self.c_type(&ty);
        let init = if codes.is_empty() { "0".to_string() } else { codes.join(", ") };
        Ok((wrap(&prelude, format!("(({ct}){{ {init} }})")), ty))
    }

    fn builtin_call(&mut self, name: &str, args: &[Expr], hint: Option<&Type>, line: usize) -> Result<(String, Type), Error> {
        let arity: &[usize] = match name {
            "array" => &[1, 2],
            "find" | "matches" | "extract" | "count" | "pow" | "min" | "max" | "split" | "join" | "captures"
            | "all_captures" | "all_matches" => &[2],
            "replace" => &[3],
            "fold_matches" => &[4],
            _ => &[1],
        };
        if !arity.contains(&args.len()) {
            let want: Vec<String> = arity.iter().map(|n| n.to_string()).collect();
            return Err(Error::at(line, format!("`{name}` takes {} argument(s), got {}", want.join(" or "), args.len())));
        }
        let mut compiled = Vec::with_capacity(args.len());
        for a in args {
            compiled.push(self.expr(a)?);
        }
        let types: Vec<Type> = compiled.iter().map(|(_, t)| t.clone()).collect();
        let (prelude, codes) = self.in_order(args, compiled);
        let compiled: Vec<(String, Type)> = codes.into_iter().zip(types).collect();
        let (code, ty) = self.builtin(name, &compiled, hint, line)?;
        Ok((wrap(&prelude, code), ty))
    }

    fn builtin(&mut self, name: &str, compiled: &[(String, Type)], hint: Option<&Type>, line: usize) -> Result<(String, Type), Error> {
        let (c, t) = &compiled[0];
        match name {
            "print" => match t {
                Type::Int => Ok((format!("lm_print_int({c})"), Type::Unit)),
                Type::Float => Ok((format!("lm_print_float({c})"), Type::Unit)),
                Type::Bool => Ok((format!("lm_print_bool({c})"), Type::Unit)),
                Type::Str => Ok((format!("lm_print_str({c})"), Type::Unit)),
                _ => {
                    let w = self.writer(t, line)?;
                    Ok((format!("({{ lm_buf b = {{0}}; {w}(&b, {c}); lm_buf_print(&b); }})"), Type::Unit))
                }
            },
            "str" => match t {
                Type::Int => Ok((format!("lm_str_of({c})"), Type::Str)),
                Type::Float => Ok((format!("lm_str_of_float({c})"), Type::Str)),
                Type::Str => Ok((c.clone(), Type::Str)),
                _ => {
                    let w = self.writer(t, line)?;
                    Ok((format!("({{ lm_buf b = {{0}}; {w}(&b, {c}); lm_buf_to_str(&b); }})"), Type::Str))
                }
            },
            "array" => {
                expect(line, "array length", &Type::Int, t)?;
                let elem = match (compiled.get(1), hint) {
                    (Some((_, init)), _) => init.clone(),
                    (None, Some(Type::Array(e))) => (**e).clone(),
                    (None, _) => Type::Int,
                };
                let (ec, ac) = (self.c_type(&elem), self.c_type(&Type::array(elem.clone())));
                let scan = self.has_pointers(&elem) as u8;
                let id = self.fresh();
                let fill = match compiled.get(1) {
                    Some((init, _)) => format!(
                        "{ec} x{id} = {init}; for (int64_t k = 0; k < n{id}; k++) d{id}[k] = x{id}; "
                    ),
                    None => String::new(),
                };
                Ok((
                    format!(
                        "({{ int64_t n{id} = {c}; {ec} *d{id} = lm_alloc(n{id}, sizeof({ec}), {scan}); {fill}({ac}){{ n{id}, d{id} }}; }})"
                    ),
                    Type::array(elem),
                ))
            }
            "bytes" => {
                expect(line, "array length", &Type::Int, t)?;
                Ok((format!("lm_bytes_new({c})"), Type::Bytes))
            }
            "len" => {
                elem_type(line, t)?;
                Ok((format!("({c}).len"), Type::Int))
            }
            "float" => match t {
                Type::Int => Ok((format!("((double)({c}))"), Type::Float)),
                Type::Float => Ok((c.clone(), Type::Float)),
                Type::Str => Ok((format!("lm_parse_float({c})"), Type::Float)),
                _ => Err(Error::at(line, format!("cannot convert {t} to float"))),
            },
            "int" => match t {
                Type::Float => Ok((format!("lm_float_to_int({c})"), Type::Int)),
                Type::Int => Ok((c.clone(), Type::Int)),
                Type::Str => Ok((format!("lm_parse({c})"), Type::Int)),
                _ => Err(Error::at(line, format!("cannot convert {t} to int"))),
            },
            "sqrt" | "floor" | "ceil" | "round" | "sin" | "cos" | "exp" | "ln" => {
                expect(line, &format!("argument of `{name}`"), &Type::Float, t)?;
                let f = if name == "ln" { "log" } else { name };
                Ok((format!("{f}({c})"), Type::Float))
            }
            "abs" => match t {
                Type::Float => Ok((format!("fabs({c})"), Type::Float)),
                Type::Int => Ok((format!("lm_abs({c})"), Type::Int)),
                _ => Err(Error::at(line, format!("argument of `abs`: expected int or float, found {t}"))),
            },
            "pow" => {
                expect(line, "argument 1 of `pow`", &Type::Float, t)?;
                expect(line, "argument 2 of `pow`", &Type::Float, &compiled[1].1)?;
                Ok((format!("pow({c}, {})", compiled[1].0), Type::Float))
            }
            "min" | "max" => {
                if !matches!(t, Type::Int | Type::Float) {
                    return Err(Error::at(line, format!("argument 1 of `{name}`: expected int or float, found {t}")));
                }
                expect(line, &format!("argument 2 of `{name}`"), t, &compiled[1].1)?;
                let f = if *t == Type::Float { format!("f{name}") } else { format!("lm_{name}") };
                Ok((format!("{f}({c}, {})", compiled[1].0), t.clone()))
            }
            "parse" => {
                expect(line, "argument of `parse`", &Type::Str, t)?;
                Ok((format!("lm_parse({c})"), Type::Int))
            }
            "read_file" => {
                expect(line, "file path", &Type::Str, t)?;
                Ok((format!("lm_read_file({c})"), Type::Str))
            }
            "split" => {
                expect(line, "argument 1 of `split`", &Type::Str, t)?;
                expect(line, "argument 2 of `split`", &Type::Str, &compiled[1].1)?;
                Ok((format!("lm_split({c}, {})", compiled[1].0), Type::array(Type::Str)))
            }
            "join" => {
                expect(line, "argument 1 of `join`", &Type::array(Type::Str), t)?;
                expect(line, "argument 2 of `join`", &Type::Str, &compiled[1].1)?;
                Ok((format!("lm_join({c}, {})", compiled[1].0), Type::Str))
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
            "matches" | "extract" | "count" | "fold_matches" | "captures" | "all_captures" | "all_matches" | "replace" => {
                let (re, rt) = &compiled[1];
                expect(line, &format!("argument 1 of `{name}`"), &Type::Str, t)?;
                expect(line, &format!("argument 2 of `{name}`"), &Type::Regex, rt)?;
                Ok(match name {
                    "matches" => (format!("lm_re_matches({c}, {re})"), Type::Bool),
                    "extract" => (format!("lm_re_extract({c}, {re})"), Type::Str),
                    "count" => (format!("lm_re_count({c}, {re})"), Type::Int),
                    "captures" => (format!("lm_re_captures({c}, {re})"), Type::array(Type::Str)),
                    "all_captures" => (format!("lm_re_all_captures({c}, {re})"), Type::array(Type::array(Type::Str))),
                    "all_matches" => (format!("lm_re_all_matches({c}, {re})"), Type::array(Type::Str)),
                    "replace" => {
                        expect(line, "argument 3 of `replace`", &Type::Str, &compiled[2].1)?;
                        (format!("lm_re_replace({c}, {re}, {})", compiled[2].0), Type::Str)
                    }
                    _ => {
                        // fold_matches(s, re, init: A, f: fn(A, str) -> A) -> A, inlined so
                        // clang can see through the closure.
                        let (init, acc) = &compiled[2];
                        let (f, ft) = &compiled[3];
                        let folder = fn_type(vec![acc.clone(), Type::Str], acc.clone());
                        expect(line, "argument 4 of `fold_matches`", &folder, ft)?;
                        let Type::Fn(sig) = &folder else { unreachable!() };
                        let (ptr, ac) = (self.c_fn_ptr(sig), self.c_type(acc));
                        let id = self.fresh();
                        (
                            format!(
                                "({{ lm_str s{id} = {c}; const lm_re *r{id} = {re}; {ac} acc{id} = {init}; lm_fn f{id} = {f}; \
                                 int64_t pos{id} = 0, last{id} = -1, st{id}, en{id}; \
                                 while (lm_re_next(s{id}, r{id}, &pos{id}, &last{id}, &st{id}, &en{id})) \
                                 acc{id} = (({ptr})f{id}.fn)(f{id}.env, acc{id}, (lm_str){{ en{id} - st{id}, s{id}.data + st{id} }}); \
                                 acc{id}; }})"
                            ),
                            acc.clone(),
                        )
                    }
                })
            }
            _ => unreachable!("builtin `{name}` has no implementation"),
        }
    }

    /// Compiles a regex literal and emits its tables once; returns the C name.
    fn regex(&mut self, pattern: &str, line: usize) -> Result<String, Error> {
        use crate::regex::Inst;
        if let Some(name) = self.regexes.get(pattern) {
            return Ok(name.clone());
        }
        let re = crate::regex::compile(pattern).map_err(|e| Error::at(line, format!("invalid regex `{pattern}`: {e}")))?;
        let name = format!("lm_re{}", self.fresh());
        let join = |items: Vec<String>| items.join(",");
        let mut decls = String::new();

        let insts: Vec<String> = re
            .prog
            .iter()
            .map(|inst| match *inst {
                Inst::Range(lo, hi, next) => format!("{{0,{lo},{hi},0,{next},0}}"),
                Inst::Split(a, b) => format!("{{1,0,0,0,{a},{b}}}"),
                Inst::Save(slot, next) => format!("{{2,0,0,0,{next},{slot}}}"),
                Inst::Look(look, next) => format!("{{3,0,0,{},{next},0}}", look_code(look)),
                Inst::Match => "{4,0,0,0,0,0}".to_string(),
                Inst::Fail => "{5,0,0,0,0,0}".to_string(),
            })
            .collect();
        decls.push_str(&format!("static const lm_rinst {name}_prog[] = {{{}}};\n", join(insts)));
        decls.push_str(&format!(
            "static const uint8_t {name}_classes[256] = {{{}}};\n",
            join(re.classes.iter().map(|c| c.to_string()).collect())
        ));
        let names: Vec<String> = re.names.iter().map(|n| n.as_ref().map_or("NULL".to_string(), |n| format!("\"{n}\""))).collect();
        decls.push_str(&format!("static const char *const {name}_names[] = {{{}}};\n", join(names)));
        let dfas = match (&re.fwd, &re.rev) {
            (Some(f), Some(r)) => {
                let table = |t: &[u32]| join(t.iter().map(|v| v.to_string()).collect());
                decls.push_str(&format!("static const uint32_t {name}_fwd[] = {{{}}};\n", table(&f.trans)));
                decls.push_str(&format!("static const uint32_t {name}_rev[] = {{{}}};\n", table(&r.trans)));
                let starts = |s: &[u32; 4]| join(s.iter().map(|v| v.to_string()).collect());
                format!(
                    ".fwd = {name}_fwd, .rev = {name}_rev, .fwd_start = {{{}}}, .rev_start = {{{}}}, \
                     .fwd_match = {{{}, {}, {}}}, .rev_match = {{{}, {}, {}}}, .quit_non_ascii = {},",
                    starts(&f.start),
                    starts(&r.start),
                    f.match_lo,
                    f.match_hi,
                    f.special,
                    r.match_lo,
                    r.match_hi,
                    r.special,
                    re.unicode_word
                )
            }
            _ => ".fwd = NULL, .rev = NULL,".to_string(),
        };
        let uword = if re.unicode_word {
            if self.helpers.insert("lm_uword".to_string()) {
                let ranges = crate::regex::unicode_word_ranges();
                let flat: Vec<String> = ranges.iter().flat_map(|(lo, hi)| [lo.to_string(), hi.to_string()]).collect();
                self.out.decls.push_str(&format!("static const uint32_t lm_uword[] = {{{}}};\n", join(flat)));
                self.out.decls.push_str(&format!("enum {{ lm_nuword = {} }};\n", ranges.len()));
            }
            ".uword = lm_uword, .nuword = lm_nuword,"
        } else {
            ".uword = NULL, .nuword = 0,"
        };
        // When one to four bytes can begin a match, they are scanned for with SIMD; pad by repetition.
        let mut firsts: Vec<u8> = (0..=255u8).filter(|&b| re.first[b as usize]).collect();
        if firsts.len() > 4 || re.can_be_empty {
            firsts.clear();
        }
        let set: Vec<String> = (0..4).map(|k| firsts.get(k).or(firsts.first()).copied().unwrap_or(0).to_string()).collect();
        decls.push_str(&format!(
            "static const lm_re {name} = {{ .classes = {name}_classes, .stride = {}, {dfas} \
             .prog = {name}_prog, .prog_len = {}, .start = {}, .ngroups = {}, .names = {name}_names, {uword} \
             .prefix = {}, .nfirst = {}, .firsts = {{{}}}, .first = {{{}}}, .can_be_empty = {}, .anchored_start = {}, \
             .pattern = {} }};\n",
            re.stride,
            re.prog.len(),
            re.start,
            re.ngroups,
            c_string(&re.prefix),
            firsts.len(),
            join(set),
            join(re.first.iter().map(|&f| (f as u8).to_string()).collect()),
            re.can_be_empty,
            re.anchored_start,
            c_string(pattern.as_bytes()),
        ));
        self.out.decls.push_str(&decls);
        self.regexes.insert(pattern.to_string(), name.clone());
        Ok(name)
    }

    fn indirect_call(&mut self, callee: &str, ty: &Type, args: &[Expr], line: usize) -> Result<(String, Type), Error> {
        let Type::Fn(sig) = ty else {
            return Err(Error::at(line, format!("cannot call a value of type {ty}")));
        };
        let (prelude, mut args) = self.args("function value", &sig.params, args, line)?;
        let k = format!("k{}", self.fresh());
        args.insert(0, format!("{k}.env"));
        let ptr = self.c_fn_ptr(sig);
        // The callee is evaluated before the arguments. A zero-filled array of
        // functions holds empty function values, which stop the program if called.
        let code = format!(
            "({{ lm_fn {k} = {callee}; {prelude}if (__builtin_expect(!{k}.fn, 0)) lm_panic({line}, \"call of an empty function value\"); \
             (({ptr}){k}.fn)({}); }})",
            args.join(", ")
        );
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
                params.push_str(&format!(", {} p{i}", self.c_type(p)));
                args.push(format!("p{i}"));
            }
            let head = format!("static {} {wrapper}({params})", self.c_type(&sig.ret));
            let ret = if sig.ret == Type::Unit { "" } else { "return " };
            self.out.decls.push_str(&format!("{head};\n"));
            self.out.lifted.push_str(&format!("\n{head} {{ {ret}f_{name}({}); }}\n", args.join(", ")));
        }
        (format!("((lm_fn){{ (void *){wrapper}, NULL }})"), Type::Fn(sig))
    }

    fn lambda(&mut self, params: &[(String, Type)], body: &Expr, line: usize) -> Result<(String, Type), Error> {
        let params: Vec<(String, Type)> =
            params.iter().map(|(n, t)| Ok((n.clone(), self.resolve(t, line)?))).collect::<Result<_, Error>>()?;
        let name = format!("lam{}", self.fresh());
        self.frames.push(Frame::new(None));
        let header = self.bind_params(&params);
        let result = self.expr(body);
        let frame = self.frames.pop().unwrap();
        let (code, ty) = result?;

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
            format!("    return {};\n", self.coerce(code, &ty, &sig.ret))
        };

        let mut c_params = String::from("void *env_");
        for (i, p) in sig.params.iter().enumerate() {
            c_params.push_str(&format!(", {} p{i}", self.c_type(p)));
        }
        let head = format!("static {} {name}({c_params})", self.c_type(&sig.ret));
        self.out.decls.push_str(&format!("{head};\n"));

        let value = if frame.captures.is_empty() {
            self.out.lifted.push_str(&format!("\n{head} {{\n{header}{body}}}\n"));
            format!("((lm_fn){{ (void *){name}, NULL }})")
        } else {
            let env = format!("struct {name}_env");
            let mut fields = String::new();
            let mut loads = format!("    {env} *env = env_;\n");
            let mut stores = String::new();
            for (i, cap) in frame.captures.iter().enumerate() {
                let ct = self.c_type(&cap.ty);
                fields.push_str(&format!(" {ct} c{i};"));
                loads.push_str(&format!("    {ct} {} = env->c{i};\n", cap.inner));
                stores.push_str(&format!(" e->c{i} = {};", cap.outer));
            }
            self.out.decls.push_str(&format!("{env} {{{fields} }};\n"));
            self.out.lifted.push_str(&format!("\n{head} {{\n{loads}{header}{body}}}\n"));
            format!("({{ {env} *e = lm_gc_alloc(sizeof({env}), 1);{stores} (lm_fn){{ (void *){name}, e }}; }})")
        };
        Ok((value, Type::Fn(Rc::new(sig))))
    }
}
