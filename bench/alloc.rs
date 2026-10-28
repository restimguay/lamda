// Allocation-heavy: format 5 million integers as strings and read them back.
fn main() {
    let n = 5_000_000;
    let mut total = 0i64;
    for i in 0..n {
        let s = i.to_string() + ",";
        total += s.len() as i64 + s.as_bytes()[0] as i64;
    }
    println!("{total}");
}
