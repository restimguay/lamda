# lamda

A small, statically typed, expression-oriented language whose compiler is written in Rust.
lamda compiles to C, which is then built with `cc -O3 -march=native`, so programs run as
optimized native code.

```
fn fib(n: int) -> int {
    if n < 2 { n } else { fib(n - 1) + fib(n - 2) }
}

fn main() {
    print(fib(40));
}
```

```sh
cargo build --release
./target/release/lamda run examples/hello.lm
./target/release/lamda build bench/fib.lm -o fib
./target/release/lamda emit-c examples/hello.lm   # inspect the generated C
```

## Language

- Types: `int` (64-bit, wrapping), `bool`, `arr` (heap array of `int`), `bytes` (heap array of
  bytes; elements read as `int` in 0..=255, stores wrap like Rust's `as u8`). Parameters default
  to `int`.
- `fn name(a: int, b: arr) -> int { ... }` — the last expression of a block is its value.
- Lambdas `\x, y: arr -> body` (parameters default to `int`, return type inferred) and function
  types `fn(int, int) -> int`. Named functions are values too. Closures capture variables by
  value when created; arrays share their contents. See `examples/closures.lm`.
- `be x = ...;`, `be mut x = ...;`, `x = ...;`, shadowing allowed.
- Pipe `|`: `x | f` is `f(x)` and `x | f(a)` is `f(x, a)`; a lambda or parenthesized right-hand
  side is called with `x`, e.g. `10 | (adder(5))`. It binds loosest of all operators. A lambda's
  body extends to the right, so wrap a lambda in parentheses to keep piping after it. See
  `examples/pipes.lm`.
- `if c { a } else { b }` is an expression; `while c { ... }`; `return expr;`.
- Builtins: `print(x)`, `array(n)` and `bytes(n)` (zero-filled), `len(a)`, `a[i]`, `a[i] = v`.
- Strings `str`: immutable bytes, literals `"..."` with `\n \t \r \\ \" \0 \xNN` escapes. `len(s)`,
  `s[i]` (a byte as `int`), `==`, `!=`, `+` (concatenation), `print(s)`. Slices `s[a..b]`, `s[a..]`,
  `s[..b]` share memory with the original (also on `arr` and `bytes`). `find(s, needle)` returns an
  index or -1, `str(n)` formats an int, `parse(s)` reads the integer at the start of `s`, and
  `read_file(path)` loads a whole file. See `examples/strings.lm`.
- Regexes in backticks, type `re`: `` `status=5\d\d` ``. They are compiled to DFAs when the program
  is compiled, so a malformed pattern is a compile error. `matches(s, re)`, `find(s, re)` (start
  index or -1), `extract(s, re)` (the match, or `""`), `count(s, re)`, and
  `fold_matches(s, re, init, f)` which folds `f(acc, match)` over every match. Syntax: literals,
  `.`, `[a-z]`, `[^...]`, `\d \w \s` (and `\D \W \S`), `* + ? {n} {n,} {n,m}`, `|`, `(...)`,
  `(?:...)`, and `^`/`$` at the start/end of the pattern. Write `` \` `` for a backtick. See
  `examples/regex.lm`.

## Editor support

`editors/vscode` is a VS Code extension for `.lm` files: syntax highlighting, bracket and comment
handling, snippets, errors as you type (via `lamda check`), and a Run button (Cmd+F5).

```sh
cargo build --release                  # the extension uses target/release/lamda
python3 editors/vscode/package.py      # builds editors/vscode/lamda-0.1.0.vsix
code --install-extension editors/vscode/lamda-0.1.0.vsix
```

Set `lamda.compilerPath` if the compiler lives elsewhere.

## Performance vs Rust

`bench/run.sh` builds each benchmark with lamda and with `rustc -C opt-level=3`, checks the
outputs match, and reports the best of several runs. On an Apple Silicon Mac:

| bench    | lamda    | rust     | rust (`target-cpu=native`) |
|----------|----------|----------|----------------------------|
| fib(40)  | 1.8 ms   | 146 ms   | 146 ms                     |
| sieve    | 116 ms   | 118 ms   | 118 ms                     |
| sieve8   | 34 ms    | 34 ms    | 34 ms                      |
| gather   | 190 ms   | 210 ms   | 209 ms                     |
| closure  | 98 ms    | 107 ms   | 107 ms                     |

String and regex benchmarks run on a 110 MB log file (`bench/gen_log.py`). The string ones use
Rust's `std`; the regex ones use the `regex` crate's byte API (`bench/rust-regex`), which skips
UTF-8 validation and is the fastest way to write them in Rust:

| bench    | lamda    | rust     | what it does                                              |
|----------|----------|----------|-----------------------------------------------------------|
| wc       | 54 ms    | 64 ms    | count lines and words byte by byte                        |
| strfind  | 15 ms    | 44 ms    | count `status=500` (Rust: `str::matches`)                 |
| strsum   | 23 ms    | 62 ms    | sum the numbers after every `ms=`                         |
| alloc    | 34 ms    | 92 ms    | format 5M ints with `str(i) + ","`, read each back        |
| recount  | 12 ms    | 20 ms    | count `` status=5\d\d `` (literal prefix)                  |
| resum    | 37 ms    | 65 ms    | sum every `` ms=\d+ `` with `fold_matches` and a lambda     |
| realt    | 23 ms    | 40 ms    | count `` (ERROR\|WARN) +user=\d+ path=/api/(items\|orders)/\d+ `` |
| reword   | 136 ms   | 159 ms   | count `` [a-z]+=\d{4} `` (a match can start almost anywhere) |

- `read_file` memory-maps files of 1 MB or more instead of copying them (recount 20 ms → 12 ms;
  reading the file is most of every benchmark here). **Trade-off:** as with any `mmap`, another
  program truncating the file while it is in use stops lamda with a bus error (lamda prints a
  message and exits), and modifying it in place changes the string. Rust only allows this in
  `unsafe` code. The mapping is a GC object and is released when the string is collected.
- `find` checks 32 positions per step with SIMD, keeping only those where both the needle's
  first and last bytes line up, then visits just the candidate positions via a lane bitmask.
- A regex whose matches all begin with the same literal (`status=5`) jumps between occurrences
  of it with `find`; one that can begin with only a few bytes (`E` or `W`) scans for them with
  SIMD; otherwise it skips bytes that cannot begin a match. The DFA then finds the longest match.
- `str(n)` formats two digits at a time instead of calling `snprintf`.
- `alloc` peaks at 1.9 MB of memory, Rust at 1.5 MB (an empty program is 1.3 MB in lamda,
  1.45 MB in Rust; Rust frees each string immediately, a collector frees in batches).

The numeric benchmarks: lamda is faster on four of five; `sieve8` is a tie (within ±1%, either side wins on a given
run). The sieves are bound by memory bandwidth, so margins there are small.

`sieve8` is the same sieve using `bytes` in lamda and `Vec<u8>` in Rust: 8x less memory traffic
than `sieve`, so 3.4x faster in both languages. `closure` folds an array through a capturing
lambda; clang inlines the call and removes the closure's allocation, matching Rust's
monomorphized closure.

Both compilers end in LLVM, so straight-line code performs about the same (the sieves are bound
by memory bandwidth and vary by several percent between runs). lamda wins in three places:

- **Recursive unrolling.** A self-recursive function is cloned into itself up to two levels
  (bounded by code size), and clang inlines the clones back. For pure functions this exposes
  repeated calls to common-subexpression elimination — unrolled `fib(n)` computes `fib(n-3)`
  twice, and the duplicate is removed — which cuts the exponential call tree. That is why
  `fib` is ~80x faster. It helps tree-recursive pure functions like this one; ordinary
  recursion only saves call overhead.
- **Unchecked array indexing.** `gather` (indices loaded from another array, which Rust cannot
  prove in range) runs about 10% faster. The cost is that an out-of-bounds index or division
  by zero is undefined behaviour instead of a panic.
- **Per-loop tuning.** lamda tells clang how to treat each `while` loop based on how it
  steps. A loop that advances by a runtime amount (`j = j + i`) is not unrolled, because
  unrolling needs a trip-count division that clang hoists in front of the loop, where it runs
  even when the loop body never does (this is what put the sieves behind Rust). Every other
  loop gets four interleaved copies, so reductions like `acc = f(acc, x)` run four
  independent accumulators instead of clang's default two (`closure`: ~9% faster than Rust).

Tried and rejected: profile-guided optimization, dropping `-fwrapv`, `-mcpu=native`, and a global
`-fno-unroll-loops` (fixes the sieves but costs `closure` 40%).

## Memory

lamda has a garbage collector (`src/runtime.c`): conservative mark-and-sweep, so generated code
needs no bookkeeping at all. All GC memory lives in one reserved address range split into 64 KB
pages; small objects share pages by size class and large ones get pages of their own, so
finding the object behind any pointer — including a slice pointing into the middle of a
string — is arithmetic. Strings and arrays hold no pointers and are never scanned; only closure
environments are. A collection runs after 512 KB of allocation or twice the live data, whichever
is larger, and sweeping happens lazily as each size class needs memory.

- `LAMDA_GC_STATS=1` prints collections and peak heap at exit.
- `LAMDA_GC_STRESS=1` collects on every allocation, for testing (`examples/gc.lm`).

## Known limitations

- The collector is conservative: an integer that happens to look like a heap address keeps that
  object alive. Only the stack and closure environments are scanned, never strings or arrays.
- Large objects (over 8 KB) take fresh address space each time; a program that allocates
  more than 64 GB of large objects in total runs out even if little is live.
- Regex matching is leftmost-longest (POSIX), where Rust's `regex` is leftmost-first: both find
  the same start, but `a|ab` on `"ab"` matches `ab` in lamda and `a` in Rust. No capture groups,
  lazy quantifiers or Unicode classes (patterns work on bytes), and `^`/`$` only at the edges.
- Regex search can take time proportional to text length × match length on pathological
  patterns (like `a*b` over a long run of `a`s); Rust's `regex` guarantees linear time.
- No floats, structs or generics yet.
- Arguments and operands are evaluated in C's unspecified order, not left to right.
