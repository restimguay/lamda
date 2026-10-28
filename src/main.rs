mod ast;
mod compile;
mod lexer;
mod parser;
mod regex;

use std::path::{Path, PathBuf};
use std::process::{exit, Command};

const USAGE: &str = "usage:
  lamda run <file.lm>                 compile and run
  lamda build <file.lm> [-o <out>]    compile to a native executable
  lamda emit-c <file.lm>              print the generated C
  lamda check <file.lm>               type-check only (errors as `file:line: message`)

environment:
  LAMDA_CC      C compiler to use (default: cc)
  LAMDA_CFLAGS  extra flags passed to the C compiler";

#[derive(Debug)]
pub struct Error {
    line: Option<usize>,
    msg: String,
}

impl Error {
    pub fn at(line: usize, msg: impl Into<String>) -> Self {
        Error { line: Some(line), msg: msg.into() }
    }

    pub fn msg(msg: impl Into<String>) -> Self {
        Error { line: None, msg: msg.into() }
    }
}

fn fail(msg: impl std::fmt::Display) -> ! {
    eprintln!("lamda: {msg}");
    exit(1);
}

fn compile_file(path: &Path) -> String {
    let src = std::fs::read_to_string(path)
        .unwrap_or_else(|e| fail(format!("cannot read {}: {e}", path.display())));
    let result = lexer::lex(&src)
        .and_then(parser::parse)
        .and_then(|program| compile::to_c(&program));
    result.unwrap_or_else(|e| match e.line {
        Some(line) => fail(format!("{}:{line}: {}", path.display(), e.msg)),
        None => fail(format!("{}: {}", path.display(), e.msg)),
    })
}

fn build(c_src: &str, out: &Path) {
    let c_path = out.with_extension("lamda.c");
    std::fs::write(&c_path, c_src)
        .unwrap_or_else(|e| fail(format!("cannot write {}: {e}", c_path.display())));

    let cc = std::env::var("LAMDA_CC").unwrap_or_else(|_| "cc".to_string());
    let extra = std::env::var("LAMDA_CFLAGS").unwrap_or_default();
    let status = Command::new(&cc)
        // -fwrapv gives integer overflow the same wrapping semantics as Rust release builds;
        // -ffp-contract=off keeps float results identical to Rust's (no fused multiply-add).
        .args(["-O3", "-march=native", "-fwrapv", "-ffp-contract=off", "-w", "-o"])
        .arg(out)
        .arg(&c_path)
        .arg("-lm")
        .args(extra.split_whitespace())
        .status()
        .unwrap_or_else(|e| fail(format!("cannot run C compiler `{cc}`: {e}")));
    let _ = std::fs::remove_file(&c_path);
    if !status.success() {
        fail("C compiler failed (this is a lamda bug; run `lamda emit-c` to inspect the output)");
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (cmd, file) = match args.as_slice() {
        [cmd, file, ..] => (cmd.as_str(), PathBuf::from(file)),
        _ => fail(USAGE),
    };

    match cmd {
        "emit-c" => print!("{}", compile_file(&file)),
        "check" => {
            compile_file(&file);
        }
        "build" => {
            let out = match &args[2..] {
                [] => PathBuf::from(file.file_stem().unwrap_or_else(|| fail(USAGE))),
                [flag, out] if flag == "-o" => PathBuf::from(out),
                _ => fail(USAGE),
            };
            build(&compile_file(&file), &out);
        }
        "run" => {
            let c_src = compile_file(&file);
            let exe = std::env::temp_dir().join(format!("lamda-run-{}", std::process::id()));
            build(&c_src, &exe);
            let status = Command::new(&exe)
                .status()
                .unwrap_or_else(|e| fail(format!("cannot run {}: {e}", exe.display())));
            let _ = std::fs::remove_file(&exe);
            exit(status.code().unwrap_or(1));
        }
        _ => fail(USAGE),
    }
}
