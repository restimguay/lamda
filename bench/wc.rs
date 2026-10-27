// Count lines and words (runs of non-space bytes), one byte at a time.
fn main() {
    let s = std::fs::read("data/log.txt").unwrap();
    let mut lines = 0i64;
    let mut words = 0i64;
    let mut in_word = false;
    for &c in &s {
        if c == b'\n' { lines += 1; }
        let space = c == b' ' || c == b'\n' || c == b'\t';
        if !space && !in_word { words += 1; }
        in_word = !space;
    }
    println!("{lines}");
    println!("{words}");
}
