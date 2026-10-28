# lamda

A statically typed, expression-oriented language whose compiler is written in Rust. lamda
compiles to C, which is then built with `cc -O3 -march=native`, so programs run as optimized
native code. On the 14 benchmarks in `bench/` it is faster than equivalent Rust on 11 (by 7% or
more on 9) and ties the other 3.

```
type Point { x: float, y: float }

fn map<T, U>(xs: [T], f: fn(T) -> U) -> [U] {
    be out: [U] = array(len(xs));
    be mut i = 0;
    while i < len(xs) { out[i] = f(xs[i]); i = i + 1; }
    out
}

fn main() {
    be words = "the quick brown fox" | split(" ");
    print(map(words, \w: str -> len(w)));                     // [3, 5, 5, 3]
    print(Point(3.0, 4.0));                                    // Point { x: 3.0, y: 4.0 }
    print("2026-10-28" | replace(`(\d+)-(\d+)-(\d+)`, "$3/$2/$1"));   // 28/10/2026
}
```

```sh
cargo build --release
./target/release/lamda run examples/types.lm
./target/release/lamda build bench/fib.lm -o fib
./target/release/lamda check examples/regex.lm    # type-check only
./target/release/lamda emit-c examples/hello.lm   # inspect the generated C
```

## Language

**Types.** `int` (64-bit, wrapping), `float` (64-bit IEEE), `bool`, `str` (immutable bytes),
`bytes` (mutable bytes read as `int` 0..=255; stores wrap like Rust's `as u8`), arrays `[T]` of
any type (`arr` is `[int]`), structs, function types `fn(int, str) -> bool`, and regexes `re`.
Parameters without a type are `int`. There are no implicit conversions: `1 + 2.5` is an error;
write `float(1) + 2.5`.

**Bindings and control flow.** `be x = 1;`, `be mut x = 1;`, `be xs: [str] = [];` (annotation
optional), `x = 2;`, shadowing allowed. `if c { a } else { b }` and blocks are expressions;
`while c { ... }`; `return expr;`.

**Functions and closures.** `fn name(a: int, b: [str]) -> int { ... }` returns its last
expression. Lambdas `\x, y: str -> body` infer their return type. Closures capture variables
by value when created; arrays inside them stay shared. Named functions are values, and so are
generic ones where the expected function type is known (`map(rows, first)`). See
`examples/closures.lm`.

**Pipe.** `x | f` is `f(x)` and `x | f(a)` is `f(x, a)`; a lambda or parenthesized right-hand
side is called with `x`. It binds loosest of all operators; wrap a lambda in parentheses to keep
piping after it. See `examples/pipes.lm`.

**Structs and generics.** `type Point { x: float, y: float }` and generic
`type Pair<A, B> { first: A, second: B }`. Construct with `Point(1.0, 2.0)` (fields in order),
read with `p.x`, write with `p.x = 3.0` (needs `be mut p`: structs are values, copied when
passed) or `ps[i].x = 3.0` (arrays are shared, so this changes the element in place). A struct
can contain itself through an array (`type Tree { kids: [Tree] }`) but not directly. Generic
functions `fn first<T>(xs: [T]) -> T` get their type parameters from the arguments and are
compiled once per combination of types, like Rust. See `examples/types.lm`.

**Arrays.** Literals `[1, 2, 3]`, `array(n, init)`, or `array(n)` zero-filled when the type is
known (`be xs: [str] = array(n);`, else `[int]`). `len(xs)`, `xs[i]`, `xs[i] = v`, and slices
`xs[a..b]`, `xs[a..]`, `xs[..b]` that share memory with the original (also on `str` and
`bytes`).

**Strings.** Literals `"..."` with `\n \t \r \\ \" \0 \xNN` escapes; `len`, `s[i]` (a byte),
`+`, `==`, `<` (byte order), slices. `find(s, needle)` (index or -1), `split(s, sep) -> [str]`,
`join(xs, sep)`, `str(x)` for any printable value, `parse(s)`/`int(s)`, `float(s)`, and
`read_file(path)`. See `examples/strings.lm`.

**Regexes.** Backtick literals, `` `(?P<y>\d{4})-(\d\d)` ``, with the syntax and matching rules
of Rust's `regex` crate (byte API): Unicode classes, `(?i)` and other flags, lazy quantifiers,
`^ $ \b \B` anywhere, named and numbered groups, leftmost-first matching. Patterns are compiled
when the program is, so a bad pattern is a compile error. Search takes linear time.
`matches(s, re)`, `find(s, re)`, `extract(s, re)`, `count(s, re)`, `all_matches(s, re) -> [str]`,
`captures(s, re) -> [str]` (group 0 is the whole match; `""` for a group that did not take
part; empty without a match), `all_captures(s, re) -> [[str]]`,
`replace(s, re, with)` (with `$1`, `$name`, `${name}`, `$$`), and
`fold_matches(s, re, init, \acc, m: str -> ...)`. Write `` \` `` for a backtick. See
`examples/regex.lm`.

**Numbers.** `float(n)`, `int(x)` (truncates; saturates like Rust's `as`), `sqrt floor ceil
round abs sin cos exp ln pow min max`. Floats print like Rust's `{:?}` (`2.0`, `0.1`, `1e21`)
and compute bit-for-bit like Rust (no fused multiply-add). See `examples/floats.lm`.

**Printing.** `print(x)` and `str(x)` work on every type except functions: structs and arrays
print like Rust's `{:?}`, with strings inside them quoted. `==` compares structs and arrays
by value.

**Safety and evaluation order.** Array indexes, slice bounds and integer division are
checked: an error stops the program with `lamda: line 12: index 3 out of range for length 3`
(exit code 101). `MIN / -1` wraps like other integer arithmetic. Operands and arguments are
evaluated left to right; an assignment `xs[i] = v` evaluates `xs`, `i`, then `v`.

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

`bench/run.sh` builds each benchmark with lamda and with `rustc -C opt-level=3`, checks that
the outputs are identical, and reports the best of 9 runs. Both versions see the same
constants (no `black_box` on the Rust side). Results vary by a few percent between runs. On an
Apple Silicon Mac:

| bench    | lamda    | rust     | rust (`target-cpu=native`) | what it does                                   |
|----------|----------|----------|----------------------------|------------------------------------------------|
| fib(40)  | 2.1 ms   | 153 ms   | 154 ms                     | naive recursive Fibonacci                      |
| sieve    | 125 ms   | 127 ms   | 129 ms                     | primes below 20M, one `int` per flag           |
| sieve8   | 36 ms    | 42 ms    | 41 ms                      | the same with `bytes` / `Vec<u8>`              |
| gather   | 201 ms   | 213 ms   | 207 ms                     | sum values at indexes read from another array  |
| closure  | 102 ms   | 109 ms   | 109 ms                     | fold an array through a capturing lambda       |
| mandel   | 189 ms   | 189 ms   | 189 ms                     | Mandelbrot set, 1600×1200, 200 iterations      |

String and regex benchmarks run on a 110 MB log file (`bench/gen_log.py`). The string ones use
Rust's `std`; the regex ones use the `regex` crate's byte API (`bench/rust-regex`), the fastest
way to write them in Rust:

| bench    | lamda    | rust     | what it does                                                       |
|----------|----------|----------|--------------------------------------------------------------------|
| wc       | 63 ms    | 64 ms    | count lines and words byte by byte                                 |
| strfind  | 25 ms    | 46 ms    | count `status=500` (Rust: `str::matches`)                          |
| strsum   | 33 ms    | 63 ms    | sum the numbers after every `ms=`                                  |
| alloc    | 38 ms    | 94 ms    | format 5M ints with `str(i) + ","`, read each back                 |
| recount  | 20 ms    | 21 ms    | count `` status=5\d\d `` (literal prefix)                          |
| resum    | 47 ms    | 66 ms    | sum every `` ms=\d+ `` with `fold_matches` and a lambda            |
| realt    | 30 ms    | 41 ms    | count `` (ERROR\|WARN) +user=\d+ path=/api/(items\|orders)/\d+ ``  |
| reword   | 135 ms   | 161 ms   | count `` [a-z]+=\d{4} `` (a match can start almost anywhere)       |

`mandel`, `wc` and `recount` are ties: both compilers end in LLVM and produce essentially the
same machine code for the first two, and `recount` spends its time finding a literal, which
both do at memory speed. `sieve` and `gather` are 2-3% wins, within run-to-run noise. Where
lamda wins clearly, it is for one of these reasons:

- **Recursive unrolling.** A self-recursive function is cloned into itself up to two levels
  (bounded by code size), and clang inlines the clones back. For pure functions this exposes
  repeated calls to common-subexpression elimination (unrolled `fib(n)` computes `fib(n-3)`
  twice, and the duplicate is removed), which cuts the exponential call tree. That is why `fib`
  is ~75x faster; it helps tree-recursive pure functions, and ordinary recursion only saves
  call overhead.
- **Per-loop tuning.** A `while` loop that advances by a runtime amount (`j = j + i`) is not
  unrolled, because unrolling needs a trip-count division that clang hoists in front of the
  loop, where it runs even when the loop body never does (`sieve8`). Every other loop gets four
  interleaved copies, so reductions like `acc = f(acc, x)` run four independent accumulators
  instead of clang's default two (`closure`).
- **Strings.** `find` checks 32 positions per step with SIMD, keeping only those where the
  needle's first and last bytes both line up, then visits just those via a lane bitmask.
  `str(n)` formats two digits at a time instead of calling `snprintf`. Rust's `read_to_string`
  also validates UTF-8, which lamda strings (bytes) skip.
- **Allocation.** Allocating from the GC is a free-list pop; `alloc` frees nothing per string.
- **Regexes.** Dense DFAs laid out like regex-automata's (states are table offsets, and dead,
  match and skip states are numbered first, so a step is one load and one comparison), built
  ahead of time instead of lazily. When no match is in progress the search jumps ahead: to
  the next occurrence of the literal every match starts with (`status=5`, via `find`), or, if
  matches can start with only a few bytes (`E` or `W`), to the next of those with SIMD.

Tried and rejected: profile-guided optimization, dropping `-fwrapv`, `-mcpu=native`, a global
`-fno-unroll-loops` (fixes the sieves but costs `closure` 40%), and memory-mapping files in
`read_file` (8 ms faster per file benchmark, but another program truncating the file would
crash lamda).

## Memory

lamda has a garbage collector (`src/runtime.c`): conservative mark-and-sweep, so generated code
needs no bookkeeping at all. All GC memory lives in one reserved address range split into 64 KB
pages; small objects share pages by size class and large ones get pages of their own, so
finding the object behind any pointer, including a slice into the middle of a string, is
arithmetic. Strings and arrays of numbers hold no pointers and are never scanned. A collection
runs after 512 KB of allocation or twice the live data, whichever is larger; sweeping happens
lazily as each size class needs memory, and the address space of freed large objects is
returned to the OS and reused.

- `LAMDA_GC_STATS=1` prints collections, peak heap and address space used at exit.
- `LAMDA_GC_STRESS=1` collects on every allocation, for testing (`examples/gc.lm`).

## Design trade-offs

- **The collector is conservative.** An integer on the stack or in a closure that happens to
  look like a heap address keeps that object alive a little longer. It never causes a wrong
  result, and it is what lets generated code run without any GC bookkeeping.
- **Some regexes take the slower path.** A Unicode `\b`/`\B` on non-ASCII text, a CRLF-mode
  anchor, or a pattern whose DFA would exceed 2,000 states is searched by a Pike VM instead of
  the DFAs: same results and still linear time, just slower.

## Not built yet

Arrays have a fixed length (no `push`), and there are no hash maps, `for` loops, modules,
string formatting beyond `str` and `+`, or error values other than stopping the program.
