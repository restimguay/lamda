// Data-dependent indexing: the index comes from another array, so a
// bounds-checked language cannot prove the access is in range.
fn main() {
    let n: i64 = std::hint::black_box(1_048_576);
    let mut perm = vec![0i64; n as usize];
    let mut vals = vec![0i64; n as usize];
    for i in 0..n {
        perm[i as usize] = (i * 7919 + 13) % n;
        vals[i as usize] = i % 1000;
    }
    let mut sum: i64 = 0;
    for _ in 0..300 {
        for k in 0..n as usize {
            sum = sum.wrapping_add(vals[perm[k] as usize]);
        }
    }
    println!("{sum}");
}
