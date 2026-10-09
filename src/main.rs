fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("--version") => {
            println!("hule {}", env!("CARGO_PKG_VERSION"));
        }
        _ => {
            println!("Hello, world!");
        }
    }
}
