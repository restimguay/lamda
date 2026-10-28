// Count the points of a 1600x1200 grid inside the Mandelbrot set (200 iterations).
fn main() {
    let w: i64 = 1600;
    let h: i64 = 1200;
    let mut inside = 0;
    for y in 0..h {
        let ci = y as f64 * 2.0 / h as f64 - 1.0;
        for x in 0..w {
            let cr = x as f64 * 3.0 / w as f64 - 2.0;
            let (mut zr, mut zi, mut i) = (0.0f64, 0.0f64, 0);
            while i < 200 && zr * zr + zi * zi <= 4.0 {
                let t = zr * zr - zi * zi + cr;
                zi = 2.0 * zr * zi + ci;
                zr = t;
                i += 1;
            }
            if i == 200 { inside += 1; }
        }
    }
    println!("{inside}");
}
