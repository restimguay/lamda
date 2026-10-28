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
    assert!(String::from_utf8_lossy(&out.stderr).contains("expected [int], found bytes"));
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
        ("fn main() { be p = print; }", "`print` cannot be used as a value"),
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
        "true\n2\n29\nuser=9 status=503\n587\n60\n[\"2026-10-28\", \"2026\", \"10\", \"28\"]\n28/10/2026 and 02/01/1999\n3\n\
         <a>\n2\n[\"INFO\", \"ERROR\", \"WARN\"]\n[\"Straße\", \"STRASSE\"]\n[\"naïve\", \"café\"]\ntrue\n`^(INFO|WARN|ERROR) `\n"
    );
    let out = run_source("re-err", "fn main() {\n    print(\"x\" | count(`a(b`));\n}");
    assert!(String::from_utf8_lossy(&out.stderr).contains(":2: invalid regex `a(b`: unclosed group"));
}

/// Random patterns and haystacks, checked against the `regex` crate's byte API,
/// whose semantics lamda follows: every match, the first match's groups, and
/// `replace` with a group reference must all agree.
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
            let (atom, quantifiable) = match rand(if depth > 0 { 20 } else { 15 }) {
                0 => ("a".to_string(), true),
                1 => ("b".to_string(), true),
                2 => (".".to_string(), true),
                3 => ("[ab]".to_string(), true),
                4 => ("[^a]".to_string(), true),
                5 => ("\\d".to_string(), true),
                6 => ("\\w".to_string(), true),
                7 => ("é".to_string(), true),
                8 => ("\\s".to_string(), true),
                9 => ("\\b".to_string(), false),
                10 => ("\\B".to_string(), false),
                11 => ("^".to_string(), false),
                12 => ("$".to_string(), false),
                13 => ("(?m:^)".to_string(), false),
                14 => ("(?i:A)".to_string(), true),
                15 | 16 => (format!("({})", pattern(rand, depth - 1)), true),
                17 => (format!("(?:{})", pattern(rand, depth - 1)), true),
                _ => (format!("({}|{})", pattern(rand, depth - 1), pattern(rand, depth - 1)), true),
            };
            out.push_str(&atom);
            if quantifiable {
                out.push_str(["", "", "", "*", "+", "?", "{1,2}", "{2}", "*?", "+?", "??"][rand(11) as usize]);
            }
        }
        out
    }

    let mut cases: Vec<(String, String)> = [
        ("a*", "baaab"), ("", "ab"), ("\\b", "ab cd"), ("a|ab", "abab"), ("(a+?)(b*)", "aabb"),
        ("(a|b)*a(a|b){10}", "abababababbbbaaaaab"), ("\\bé\\w*", "x é1 éé"), ("(?m)^\\w+$", "ab\ncd\n\nx"),
        ("(?P<y>\\d{4})-(?P<m>\\d\\d)", "on 2026-10 and 1999-01"), ("x*", "é"),
    ]
    .iter()
    .map(|(p, h)| (p.to_string(), h.to_string()))
    .collect();
    for _ in 0..600 {
        let p = pattern(&mut rand, 2);
        let hay: String = (0..rand(20)).map(|_| ['a', 'b', '1', '\n', ' ', '_', 'é'][rand(7) as usize]).collect();
        cases.push((p, hay));
    }

    let mut src = String::from("fn main() {\n");
    for (p, hay) in &cases {
        let lit = hay.replace('\n', "\\n");
        src.push_str(&format!(
            "    print(\"{lit}\" | all_matches(`{p}`) | join(\"|\"));\n\
             \x20   print(\"{lit}\" | captures(`{p}`) | join(\"|\"));\n\
             \x20   print(\"{lit}\" | replace(`{p}`, \"<$1>\"));\n\
             \x20   print(\"\\x01\");\n"
        ));
    }
    src.push_str("}\n");
    let out = run_source("re-diff", &src);
    if !out.status.success() {
        let kept = std::env::temp_dir().join("lamda-re-diff-failed.lm");
        std::fs::write(&kept, &src).unwrap();
        panic!("lamda failed ({:?}): {}; program kept at {}", out.status, String::from_utf8_lossy(&out.stderr), kept.display());
    }
    // Outputs can contain newlines themselves, so each case ends with a \x01 line.
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let got: Vec<&str> = stdout.split("\x01\n").collect();
    for (k, (p, hay)) in cases.iter().enumerate() {
        let re = regex::bytes::Regex::new(p).unwrap();
        let h = hay.as_bytes();
        let all: Vec<String> = re.find_iter(h).map(|m| String::from_utf8_lossy(m.as_bytes()).into_owned()).collect();
        let caps: Vec<String> = re
            .captures(h)
            .map(|c| c.iter().map(|g| g.map_or(String::new(), |g| String::from_utf8_lossy(g.as_bytes()).into_owned())).collect())
            .unwrap_or_default();
        let replaced = String::from_utf8_lossy(&re.replace_all(h, &b"<$1>"[..])).into_owned();
        let want = format!("{}\n{}\n{}\n", all.join("|"), caps.join("|"), replaced);
        assert_eq!(got[k], want, "pattern `{p}` on {hay:?}");
    }
}

#[test]
fn large_files_are_collected() {
    // Reading a big file repeatedly must not pile up copies.
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

fn stat_kb(stats: &str, key: &str) -> u64 {
    stats.split(key).nth(1).and_then(|s| s.split(' ').next()).and_then(|n| n.parse().ok())
        .unwrap_or_else(|| panic!("no `{key}` in: {stats}"))
}

#[test]
fn evaluation_is_left_to_right() {
    let out = run_source(
        "order",
        "fn f(x: int) -> int { print(x); x }
         fn pair(a: int, b: int) -> int { a * 10 + b }
         fn main() {
             print(f(1) + f(2));
             print(pair(f(3), f(4)));
             be xs = array(9);
             xs[f(5)] = f(6);
             print(xs[f(7)] + f(8));
             be mut x = 1;
             print(x + { x = 5; x });
         }",
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1\n2\n3\n3\n4\n34\n5\n6\n7\n8\n8\n6\n");
}

#[test]
fn checked_indexing_and_division() {
    let cases = [
        ("fn main() {\n    be xs = array(3);\n    print(xs[3]);\n}", "line 3: index 3 out of range for length 3"),
        ("fn main() {\n    be xs = array(3);\n    xs[-1] = 2;\n}", "line 3: index -1 out of range for length 3"),
        ("fn main() {\n    print(\"abc\"[2..5]);\n}", "line 2: slice 2..5 out of range for length 3"),
        ("fn main() {\n    print(\"abc\"[2..1]);\n}", "line 2: slice 2..1 out of range for length 3"),
        ("fn zero() -> int { 0 }\nfn main() {\n    print(7 % zero());\n}", "line 3: division by zero"),
    ];
    for (i, (src, want)) in cases.iter().enumerate() {
        let out = run_source(&format!("check{i}"), src);
        assert_eq!(out.status.code(), Some(101), "case {i}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(want), "case {i}: {err}");
    }
    let out = run_source("divwrap", "fn main() { be m = -9223372036854775807 - 1; print(m / -1); print(m % -1); print(-7 / 2); }");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "-9223372036854775808\n0\n-3\n");
}

#[test]
fn freed_address_space_is_reused() {
    let path = std::env::temp_dir().join(format!("lamda-test-runs-{}.lm", std::process::id()));
    // 2000 arrays of 1.6 MB: 3.2 GB allocated in total, one alive at a time.
    std::fs::write(&path, "fn main() { be mut r = 0; be mut t = 0;
        while r < 2000 { be xs = array(200000 + r % 7); xs[r] = r; t = t + xs[r]; r = r + 1; }
        print(t); }").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_lamda"))
        .args(["run", path.to_str().unwrap()])
        .env("LAMDA_GC_STATS", "1")
        .output()
        .unwrap();
    std::fs::remove_file(&path).unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "1999000\n");
    let stats = String::from_utf8_lossy(&out.stderr);
    let space = stat_kb(&stats, "address space ");
    assert!(space < 64 * 1024, "address space {space} KB");
}

#[test]
fn floats_example() {
    let out = lamda(&["run", "examples/floats.lm"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "5.0\n0.30000000000000004\n0.3333333333333333\n6.02e23\n-1.5\ninf\n2.75\n0\n0.0\n1.5\n1e-7 and 2500.0\n"
    );
}

/// lamda prints floats exactly like Rust's `{:?}`; these expressions mean the same in both.
#[test]
fn float_formatting_matches_rust() {
    let exprs = [
        "1.0 / 3.0", "2.0 / 3.0", "0.1 + 0.2", "100.0", "1e15", "1e16", "1.5e16", "9999999999999998.0",
        "0.0001", "0.00012345", "0.00001", "1e-7", "1.5e-7", "123456789.125", "1e21", "1e100", "1e-100",
        "5e-324", "1.7976931348623157e308", "-0.0", "0.0", "-2.5", "1.0 / 0.0", "-1.0 / 0.0", "0.5e-4",
        "12345.678e3", "3.0 * 1.1", "1e22 / 3.0", "-123.456e-10", "2.2250738585072014e-308",
    ];
    let mut src = String::from("fn main() {\n");
    for e in exprs {
        src.push_str(&format!("    print({e});\n"));
    }
    src.push_str("    print(0.0 / 0.0);\n}\n");
    let out = run_source("float-fmt", &src);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let rust: Vec<String> = [
        1.0f64 / 3.0, 2.0 / 3.0, 0.1 + 0.2, 100.0, 1e15, 1e16, 1.5e16, 9999999999999998.0,
        0.0001, 0.00012345, 0.00001, 1e-7, 1.5e-7, 123456789.125, 1e21, 1e100, 1e-100,
        5e-324, 1.7976931348623157e308, -0.0, 0.0, -2.5, 1.0 / 0.0, -1.0 / 0.0, 0.5e-4,
        12345.678e3, 3.0 * 1.1, 1e22 / 3.0, -123.456e-10, 2.2250738585072014e-308,
    ]
    .iter()
    .map(|x| format!("{x:?}"))
    .chain(["NaN".to_string()])
    .collect();
    let got: Vec<&str> = std::str::from_utf8(&out.stdout).unwrap().lines().collect();
    for (i, want) in rust.iter().enumerate() {
        assert_eq!(got[i], want, "{}", exprs.get(i).unwrap_or(&"0.0 / 0.0"));
    }
}

const TYPES_OUTPUT: &str = "5.0\nPoint { x: 6.0, y: 4.0 }\n[\"the\", \"quick\", \"brown\", \"fox\"]\n[3, 5, 5, 3]\n\
the! quick! brown! fox!\n16\n[Point { x: 1.0, y: 1.0 }, Point { x: 2.0, y: 9.5 }]\ntrue\nPair { first: 42, second: \"age\" }\n\
Team { name: \"blue\", members: [\"ana\", \"bo\"], scores: [8, 9] }\n[[1, 2], [3]] [true, false]\n";

#[test]
fn types_example() {
    for stress in ["0", "1"] {
        let out = Command::new(env!("CARGO_BIN_EXE_lamda"))
            .args(["run", "examples/types.lm"])
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .env("LAMDA_GC_STRESS", stress)
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout), TYPES_OUTPUT, "LAMDA_GC_STRESS={stress}");
    }
}

#[test]
fn recursive_types_through_arrays() {
    let out = run_source(
        "tree",
        "type Tree { value: int, kids: [Tree] }
         fn total(t: Tree) -> int {
             be mut sum = t.value;
             be mut i = 0;
             while i < len(t.kids) { sum = sum + total(t.kids[i]); i = i + 1; }
             sum
         }
         fn main() {
             be leaf = \\v -> Tree(v, []);
             be t = Tree(1, [leaf(2), Tree(3, [leaf(4)])]);
             print(total(t));
             print(t);
         }",
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "10\nTree { value: 1, kids: [Tree { value: 2, kids: [] }, Tree { value: 3, kids: [Tree { value: 4, kids: [] }] }] }\n"
    );
}

#[test]
fn type_errors() {
    let cases = [
        ("type Node { next: Node }\nfn main() {}", "contains itself and would have infinite size"),
        ("type P { x: int }\nfn main() { be p = P(1); p.x = 2; }", "cannot assign twice to immutable `p`"),
        ("type P { x: int, y: int }\nfn main() { print(P(1, 2).z); }", "no field `z` (its fields: x, y)"),
        ("type P { x: int, y: int }\nfn main() { print(P(1)); }", "`P` takes 2 field(s) (x, y), got 1"),
        ("fn f<T>() -> int { 1 }\nfn main() { print(f()); }", "cannot infer type parameter `T` of `f`"),
        ("fn same<T>(a: T, b: T) {}\nfn main() { same(1, \"x\"); }", "argument 2 of `same`: expected int, found str"),
        ("fn main() { be xs = []; }", "cannot infer the element type of `[]`"),
        ("fn grow<T>(x: T, n: int) { if n > 0 { grow([x], n - 1); } }\nfn main() { grow(1, 3); }", "ever larger types"),
        ("fn main() { print((\\x -> x) == (\\x -> x)); }", "cannot be compared"),
        ("fn main() { be xs = [1, \"a\"]; }", "array element 2: expected int, found str"),
        ("fn main() { be x: [str] = [1]; }", "array element 1: expected str, found int"),
    ];
    for (i, (src, want)) in cases.iter().enumerate() {
        let out = run_source(&format!("type-err{i}"), src);
        assert!(!out.status.success(), "case {i} compiled");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(want), "case {i}: {err}");
    }
    let out = run_source("empty-fn", "fn main() {\n    be fs: [fn(int) -> int] = array(2);\n    print(fs[0](1));\n}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("line 3: call of an empty function value"));
}

#[test]
fn generic_functions_as_values() {
    let out = run_source(
        "generic-values",
        "fn first<T>(xs: [T]) -> T { xs[0] }
         fn twice<T>(x: T) -> [T] { [x, x] }
         fn map<T, U>(xs: [T], f: fn(T) -> U) -> [U] {
             be out: [U] = array(len(xs));
             be mut i = 0;
             while i < len(xs) { out[i] = f(xs[i]); i = i + 1; }
             out
         }
         fn main() {
             print(map([[1, 2], [3, 4]], first));
             print(map([\"a\", \"b\"], twice));
             be pick: fn([str]) -> str = first;
             print(pick([\"x\", \"y\"]));
         }",
    );
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "[1, 3]\n[[\"a\", \"a\"], [\"b\", \"b\"]]\nx\n");
    let out = run_source("generic-value-err", "fn id<T>(x: T) -> T { x }\nfn main() { be f = id; }");
    assert!(String::from_utf8_lossy(&out.stderr).contains("cannot infer the type parameters of `id`"));
}
