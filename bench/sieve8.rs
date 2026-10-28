// Count primes below 20 million, one byte per flag.
fn main() {
    let n: i64 = 20_000_000;
    let mut composite = vec![0u8; n as usize];
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
