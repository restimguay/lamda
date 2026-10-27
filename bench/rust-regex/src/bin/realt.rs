// Count matches of a pattern that starts with an alternation (no common prefix).
fn main() {
    let s = std::fs::read("data/log.txt").unwrap();
    let re = regex::bytes::Regex::new(r"(ERROR|WARN) +user=\d+ path=/api/(items|orders)/\d+").unwrap();
    println!("{}", re.find_iter(&s).count());
}
