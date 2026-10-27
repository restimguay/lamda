// Sum the number in every `ms=` match.
fn main() {
    let s = std::fs::read("data/log.txt").unwrap();
    let re = regex::bytes::Regex::new(r"ms=\d+").unwrap();
    let total: i64 = re
        .find_iter(&s)
        .map(|m| m.as_bytes()[3..].iter().fold(0i64, |n, &d| n * 10 + (d - b'0') as i64))
        .sum();
    println!("{total}");
}
