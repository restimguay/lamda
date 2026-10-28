use std::rc::Rc;

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Int,
    Float,
    Bool,
    /// Array of `int` values stored as unsigned bytes (0..=255).
    Bytes,
    /// Immutable byte string; slices share memory with the original.
    Str,
    /// A regex literal, compiled to a DFA ahead of time.
    Regex,
    /// `[T]`: a fixed-length array, shared by reference.
    Array(Rc<Type>),
    /// A struct type (with its type arguments, for generic structs). Structs are values.
    Struct(Rc<StructRef>),
    Fn(Rc<FnSig>),
    /// A type parameter of a generic function or struct, such as `T`.
    Param(Rc<str>),
    /// A name the parser could not classify (a struct or a type parameter);
    /// resolved by the compiler.
    Named(Rc<str>, Vec<Type>),
    Unit,
    /// The type of a block that always returns early; unifies with anything.
    Never,
}

#[derive(Debug, PartialEq)]
pub struct StructRef {
    pub name: String,
    pub args: Vec<Type>,
}

#[derive(Debug, PartialEq)]
pub struct FnSig {
    pub params: Vec<Type>,
    pub ret: Type,
}

impl Type {
    pub fn array(elem: Type) -> Type {
        Type::Array(Rc::new(elem))
    }
}

fn write_args(f: &mut std::fmt::Formatter, args: &[Type]) -> std::fmt::Result {
    if !args.is_empty() {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        write!(f, "<{}>", args.join(", "))?;
    }
    Ok(())
}

impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let name = match self {
            Type::Fn(sig) => {
                let params: Vec<String> = sig.params.iter().map(|p| p.to_string()).collect();
                write!(f, "fn({})", params.join(", "))?;
                if sig.ret != Type::Unit {
                    write!(f, " -> {}", sig.ret)?;
                }
                return Ok(());
            }
            Type::Array(elem) => return write!(f, "[{elem}]"),
            Type::Struct(s) => {
                f.write_str(&s.name)?;
                return write_args(f, &s.args);
            }
            Type::Param(name) => return f.write_str(name),
            Type::Named(name, args) => {
                f.write_str(name)?;
                return write_args(f, args);
            }
            Type::Int => "int",
            Type::Float => "float",
            Type::Bool => "bool",
            Type::Bytes => "bytes",
            Type::Str => "str",
            Type::Regex => "re",
            Type::Unit => "unit",
            Type::Never => "never",
        };
        f.write_str(name)
    }
}

#[derive(Debug, Default)]
pub struct Program {
    pub funcs: Vec<Func>,
    pub structs: Vec<StructDef>,
}

#[derive(Debug)]
pub struct StructDef {
    pub name: String,
    pub params: Vec<String>,
    pub fields: Vec<(String, Type)>,
    pub line: usize,
}

#[derive(Debug)]
pub struct Func {
    pub name: String,
    pub type_params: Vec<String>,
    pub params: Vec<(String, Type)>,
    pub ret: Type,
    pub body: Block,
    pub line: usize,
}

#[derive(Debug)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub tail: Option<Box<Expr>>,
}

#[derive(Debug)]
pub enum Stmt {
    Let { name: String, mutable: bool, ty: Option<Type>, value: Expr, line: usize },
    /// `target = value`, where the target is a variable, `xs[i]` or `x.field` (nested).
    Assign { target: Expr, value: Expr, line: usize },
    While { cond: Expr, body: Block, line: usize },
    Return { value: Option<Expr>, line: usize },
    Expr(Expr),
}

#[derive(Debug, Clone, Copy)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug, Clone, Copy)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinOp {
    pub fn c_op(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "&&",
            BinOp::Or => "||",
        }
    }
}

#[derive(Debug)]
pub enum ExprKind {
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    Regex(String),
    Bool(bool),
    Var(String),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// A call by name: a local holding a function value, a top-level function,
    /// a struct constructor, or a builtin.
    Call(String, Vec<Expr>),
    /// A call of any other expression, e.g. `make_adder(1)(2)`.
    CallExpr(Box<Expr>, Vec<Expr>),
    Lambda(Vec<(String, Type)>, Box<Expr>),
    Index(Box<Expr>, Box<Expr>),
    /// `xs[lo..hi]`; a missing bound means the start or the end.
    Slice(Box<Expr>, Option<Box<Expr>>, Option<Box<Expr>>),
    Field(Box<Expr>, String),
    /// `[a, b, c]`
    Array(Vec<Expr>),
    If(Box<Expr>, Block, Option<Block>),
    Block(Block),
}

#[derive(Debug)]
pub struct Expr {
    pub kind: ExprKind,
    pub line: usize,
}

impl Expr {
    /// Block-like expressions can stand as statements without a trailing `;`.
    pub fn is_block_like(&self) -> bool {
        matches!(self.kind, ExprKind::If(..) | ExprKind::Block(_))
    }
}
