// Count matches of a pattern with a literal prefix.
fn main() {
    let s = std::fs::read("data/log.txt").unwrap();
    let re = regex::bytes::Regex::new(r"status=5\d\d").unwrap();
    println!("{}", re.find_iter(&s).count());
}
