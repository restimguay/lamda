// Sum every number that follows `ms=`.
fn main() {
    let s = std::fs::read_to_string("data/log.txt").unwrap();
    let mut rest = s.as_str();
    let mut total = 0i64;
    while let Some(at) = rest.find("ms=") {
        rest = &rest[at + 3..];
        let end = rest.bytes().position(|b| !b.is_ascii_digit()).unwrap_or(rest.len());
        total += rest[..end].parse::<i64>().unwrap();
        rest = &rest[end..];
    }
    println!("{total}");
}
