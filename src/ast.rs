use std::rc::Rc;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Int,
    Bool,
    Arr,
    /// Array of `int` values stored as unsigned bytes (0..=255).
    Bytes,
    /// Immutable byte string; slices share memory with the original.
    Str,
    /// A regex literal, compiled to a DFA ahead of time.
    Regex,
    Fn(Rc<FnSig>),
    Unit,
    /// The type of a block that always returns early; unifies with anything.
    Never,
}

#[derive(Debug, PartialEq, Eq)]
pub struct FnSig {
    pub params: Vec<Type>,
    pub ret: Type,
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
            Type::Int => "int",
            Type::Bool => "bool",
            Type::Arr => "arr",
            Type::Bytes => "bytes",
            Type::Str => "str",
            Type::Regex => "re",
            Type::Unit => "unit",
            Type::Never => "never",
        };
        f.write_str(name)
    }
}

#[derive(Debug)]
pub struct Func {
    pub name: String,
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
    Let { name: String, mutable: bool, value: Expr, line: usize },
    Assign { name: String, value: Expr, line: usize },
    IndexAssign { arr: Expr, index: Expr, value: Expr, line: usize },
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
    Str(Vec<u8>),
    Regex(String),
    Bool(bool),
    Var(String),
    Unary(UnOp, Box<Expr>),
    Binary(BinOp, Box<Expr>, Box<Expr>),
    /// A call by name: a local holding a function value, or a top-level function.
    Call(String, Vec<Expr>),
    /// A call of any other expression, e.g. `make_adder(1)(2)`.
    CallExpr(Box<Expr>, Vec<Expr>),
    Lambda(Vec<(String, Type)>, Box<Expr>),
    Index(Box<Expr>, Box<Expr>),
    /// `xs[lo..hi]`; a missing bound means the start or the end.
    Slice(Box<Expr>, Option<Box<Expr>>, Option<Box<Expr>>),
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
