// Count occurrences of a substring.
fn main() {
    let s = std::fs::read_to_string("data/log.txt").unwrap();
    println!("{}", s.matches("status=500").count());
}
