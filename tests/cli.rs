use std::process::Command;

fn lamda(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_lamda"))
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .unwrap()
}

fn run_source(name: &str, src: &str) -> std::process::Output {
    let path = std::env::temp_dir().join(format!("lamda-test-{name}-{}.lm", std::process::id()));
    std::fs::write(&path, src).unwrap();
    let out = lamda(&["run", path.to_str().unwrap()]);
    std::fs::remove_file(&path).unwrap();
    out
}

#[test]
fn hello_example() {
    let out = lamda(&["run", "examples/hello.lm"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "6765\n-1\n0\n385\n47\ntrue\n");
}

#[test]
fn never_typed_branches() {
    let out = run_source(
        "never",
        "fn pick(b: bool) -> int { if b { return 1; } else { return 2; } }
         fn main() { print(pick(true)); print(pick(false)); }",
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1\n2\n");
}

#[test]
fn type_errors_report_line() {
    let out = run_source("typeerr", "fn main() {\n    be x = 1;\n    x = 2;\n}");
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains(":3: cannot assign twice to immutable `x`"), "{err}");

    let out = run_source("old-let", "fn main() {\n    let x = 1;\n}");
    assert!(String::from_utf8_lossy(&out.stderr).contains(":2: lamda uses `be` instead of `let`"));

    let out = run_source("mismatch", "fn f() -> int { true }\nfn main() {}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("expected int, found bool"));
}

#[test]
fn wrapping_overflow() {
    let out = run_source("wrap", "fn main() { be x = 9223372036854775807; print(x + 1); }");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "-9223372036854775808\n");
}

#[test]
fn unrolled_recursion_keeps_side_effects_in_order() {
    let out = run_source(
        "unroll",
        "fn walk(n: int, log: arr) -> int {
             if n <= 0 { return 0; }
             log[len(log) - n] = n;
             print(n);
             walk(n - 1, log) + 1
         }
         fn tree(n: int) -> int { if n < 2 { 1 } else { tree(n - 1) + tree(n - 2) + tree(n - 1) } }
         fn main() {
             be log = array(5);
             print(walk(5, log));
             print(log[0] * 10000 + log[1] * 1000 + log[2] * 100 + log[3] * 10 + log[4]);
             print(tree(20));
         }",
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "5\n4\n3\n2\n1\n5\n54321\n22619537\n");
}

#[test]
fn bytes_arrays() {
    let out = run_source(
        "bytes",
        "fn fill(b: bytes, v: int) { be mut i = 0; while i < len(b) { b[i] = v + i; i = i + 1; } }
         fn main() {
             be b = bytes(4);
             print(b[3]);
             fill(b, 254);
             print(b[0]); print(b[1]); print(b[2]); print(b[3]); // stores wrap to 0..=255
             b[0] = -1;
             print(b[0]);
             print(len(b));
         }",
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "0\n254\n255\n0\n1\n255\n4\n");

    let out = run_source("bytes-err", "fn main() { be b = bytes(2); be a = array(2); a = b; }");
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot assign twice"));
    let out = run_source("bytes-err2", "fn f(a: arr) {}\nfn main() { f(bytes(2)); }");
    assert!(String::from_utf8_lossy(&out.stderr).contains("expected arr, found bytes"));
}

#[test]
fn closures_example() {
    let out = lamda(&["run", "examples/closures.lm"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "63\n15\n81\n25\n101\n24\n55\n13\ntrue\n");
}

#[test]
fn closure_errors() {
    let cases = [
        ("fn main() { be mut x = 1; be f = \\y -> { x = y; }; }", "closures capture variables by value"),
        ("fn main() { be x = 1; x(2); }", "cannot call a value of type int"),
        ("fn twice(f: fn(int) -> int) -> int { f(1) }\nfn main() { twice(\\x -> x > 1); }",
         "expected fn(int) -> int, found fn(int) -> bool"),
        ("fn main() { be p = print; }", "builtin `print` cannot be used as a value"),
        ("fn main() { be f = \\a, b -> a + b; f(1); }", "takes 2 argument(s), got 1"),
    ];
    for (i, (src, want)) in cases.iter().enumerate() {
        let out = run_source(&format!("closure-err{i}"), src);
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(want), "case {i}: {err}");
    }
}

#[test]
fn pipes() {
    let out = lamda(&["run", "examples/pipes.lm"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "220\n7\n5\n3\n15\n42\ntrue\n");

    let out = run_source("pipe-err", "fn main() {\n    5 | 3;\n}");
    assert!(String::from_utf8_lossy(&out.stderr).contains(":2: cannot call a value of type int"));
}

#[test]
fn strings() {
    let out = lamda(&["run", "examples/strings.lm"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "hello, world\n12\n104\nworld\nhello!\ntrue\n4\n-1\n2\n43\n-7°C\ntab\there, quote \"q\", hex A\n6\ntrue\n"
    );

    let cases = [
        ("fn main() { be s = \"ab\"; s[0] = 1; }", "strings are immutable"),
        ("fn main() { be s = \"ab\" - \"a\"; }", "`-` is not defined for str"),
        ("fn main() { be b = \"1\" == 1; }", "expected str, found int"),
        ("fn main() { print(find(\"abc\")); }", "`find` takes 2 argument(s), got 1"),
        ("fn main() {\n    print(\"oops);\n}", ":2: unterminated string literal"),
    ];
    for (i, (src, want)) in cases.iter().enumerate() {
        let out = run_source(&format!("str-err{i}"), src);
        assert!(!out.status.success());
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(want), "case {i}: {err}");
    }
}

const GC_OUTPUT: &str = "168890\nhello, 42!?\nabcdef\nhello, 42!.abcdef!\n14999850000\n";

#[test]
fn gc_keeps_live_values() {
    for stress in ["0", "1"] {
        let out = Command::new(env!("CARGO_BIN_EXE_lamda"))
            .args(["run", "examples/gc.lm"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env("LAMDA_GC_STRESS", stress)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout), GC_OUTPUT, "LAMDA_GC_STRESS={stress}");
    }
}

#[test]
fn gc_reclaims_garbage() {
    let path = std::env::temp_dir().join(format!("lamda-test-churn-{}.lm", std::process::id()));
    std::fs::write(
        &path,
        "fn main() { be mut i = 0; be mut t = 0;
           while i < 5000000 { be s = str(i) + \",\"; t = t + s[0]; i = i + 1; }
           print(t); }",
    )
    .unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_lamda"))
        .args(["run", path.to_str().unwrap()])
        .env("LAMDA_GC_STATS", "1")
        .output()
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    let stats = String::from_utf8_lossy(&out.stderr);
    let peak_kb: u64 = stats
        .split("peak heap ")
        .nth(1)
        .and_then(|s| s.split(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no gc stats in: {stats}"));
    // Without collection this program needs ~75 MB; collecting from 512 KB keeps it near 1 MB.
    assert!(peak_kb < 8 * 1024, "peak heap {peak_kb} KB");
}

#[test]
fn regex_example() {
    let out = lamda(&["run", "examples/regex.lm"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "true\n2\n29\nuser=9 status=503\n587\n60\ntrue\nfalse\nc333\n-1\n"
    );
    let out = run_source("re-err", "fn main() {\n    print(\"x\" | count(`a(b`));\n}");
    assert!(String::from_utf8_lossy(&out.stderr).contains(":2: invalid regex `a(b`: unclosed `(`"));
}

/// Random patterns and haystacks, checked against the `regex` crate. lamda matches
/// leftmost-longest and the crate leftmost-first, so they agree on whether there
/// is a match and where it starts, though not always where it ends.
#[test]
fn regex_agrees_with_regex_crate() {
    let mut seed = 0x9E3779B97F4A7C15u64;
    let mut rand = move |n: u64| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed % n
    };
    fn pattern(rand: &mut dyn FnMut(u64) -> u64, depth: u32) -> String {
        let mut out = String::new();
        for _ in 0..1 + rand(3) {
            let atom = match rand(if depth > 0 { 9 } else { 7 }) {
                0 => "a".to_string(),
                1 => "b".to_string(),
                2 => ".".to_string(),
                3 => "[ab]".to_string(),
                4 => "[^a]".to_string(),
                5 => "\\d".to_string(),
                6 => "1".to_string(),
                7 => format!("({})", pattern(rand, depth - 1)),
                _ => format!("({}|{})", pattern(rand, depth - 1), pattern(rand, depth - 1)),
            };
            let quant = ["", "", "", "*", "+", "?", "{1,2}", "{2}"][rand(8) as usize];
            out.push_str(&atom);
            out.push_str(quant);
        }
        out
    }

    let mut cases = Vec::new();
    for _ in 0..400 {
        let mut p = pattern(&mut rand, 2);
        if rand(6) == 0 {
            p.insert(0, '^');
        }
        if rand(6) == 0 {
            p.push('$');
        }
        let hay: String = (0..rand(24)).map(|_| ['a', 'b', '1', '\n', 'c'][rand(5) as usize]).collect();
        cases.push((p, hay));
    }

    let mut src = String::from("fn main() {\n");
    for (p, hay) in &cases {
        let lit = hay.replace('\n', "\\n");
        src.push_str(&format!("    print(\"{lit}\" | find(`{p}`));\n    print(\"{lit}\" | matches(`{p}`));\n"));
    }
    src.push_str("}\n");
    let out = run_source("re-diff", &src);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut lines = stdout.lines();
    for (p, hay) in &cases {
        let re = regex::Regex::new(p).unwrap();
        let want_find = re.find(hay).map_or(-1, |m| m.start() as i64).to_string();
        let want_match = re.is_match(hay).to_string();
        assert_eq!(lines.next(), Some(want_find.as_str()), "find `{p}` in {hay:?}");
        assert_eq!(lines.next(), Some(want_match.as_str()), "matches `{p}` in {hay:?}");
    }
}

#[test]
fn mapped_files_are_collected() {
    // Files of 1 MB or more are memory-mapped; reading one repeatedly must not pile up mappings.
    let dir = std::env::temp_dir();
    let data = dir.join(format!("lamda-test-big-{}.txt", std::process::id()));
    let line = "status=503 ms=12\nstatus=200 ms=7\n";
    std::fs::write(&data, line.repeat(2_000_000 / line.len())).unwrap();
    let src = format!(
        "fn main() {{ be mut t = 0; be mut r = 0;
           while r < 30 {{ be s = read_file(\"{}\"); t = t + count(s, `status=5\\d\\d`) + len(str(r)); r = r + 1; }}
           print(t); }}",
        data.display()
    );
    for stress in ["0", "1"] {
        let path = dir.join(format!("lamda-test-reread-{}.lm", std::process::id()));
        std::fs::write(&path, &src).unwrap();
        let out = Command::new(env!("CARGO_BIN_EXE_lamda"))
            .args(["run", path.to_str().unwrap()])
            .env("LAMDA_GC_STATS", "1")
            .env("LAMDA_GC_STRESS", stress)
            .output()
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let matches = 2_000_000 / line.len();
        assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{}\n", 30 * matches + 10 + 2 * 20));
        let stats = String::from_utf8_lossy(&out.stderr);
        let peak_kb: u64 = stats.split("peak heap ").nth(1).and_then(|s| s.split(' ').next()).unwrap().parse().unwrap();
        assert!(peak_kb < 8 * 1024, "peak heap {peak_kb} KB with LAMDA_GC_STRESS={stress}");
    }
    std::fs::remove_file(&data).unwrap();
}
