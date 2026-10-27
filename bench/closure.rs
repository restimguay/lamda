// A higher-order fold called with a fresh capturing closure each round.
fn fold(xs: &[i64], init: i64, f: impl Fn(i64, i64) -> i64) -> i64 {
    let mut acc = init;
    for &x in xs {
        acc = f(acc, x);
    }
    acc
}

fn main() {
    let n: i64 = std::hint::black_box(1_000_000);
    let xs: Vec<i64> = (0..n).collect();
    let mut total = 0;
    for round in 0..300 {
        let k = round;
        total += fold(&xs, 0, |acc, x| acc + (x * k) % 7);
    }
    println!("{total}");
}
