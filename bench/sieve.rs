// Count primes below 20 million.
fn main() {
    let n: i64 = std::hint::black_box(20_000_000);
    let mut composite = vec![0i64; n as usize];
    let mut count = 0;
    let mut i = 2;
    while i < n {
        if composite[i as usize] == 0 {
            count += 1;
            let mut j = i * i;
            while j < n {
                composite[j as usize] = 1;
                j += i;
            }
        }
        i += 1;
    }
    println!("{count}");
}
