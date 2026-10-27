// Count matches of a pattern that can start at almost any byte.
fn main() {
    let s = std::fs::read("data/log.txt").unwrap();
    let re = regex::bytes::Regex::new(r"[a-z]+=\d{4}").unwrap();
    println!("{}", re.find_iter(&s).count());
}
