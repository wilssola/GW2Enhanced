//! Checks whether this version of GW2 Translated works with a `Gw2-64.exe`.
//!
//! `cargo run --release --example check_exe -- "D:\...\Guild Wars 2\Gw2-64.exe"`
//!
//! Run it after every game patch. Exits with code 1 if something broke.

fn main() {
    let path = std::env::args().nth(1).expect("path to Gw2-64.exe");
    let exe = std::fs::read(&path).expect("could not read the exe");
    let checks = gw2translated::layout::check_exe(&exe);
    let mut bad = 0;
    for c in &checks {
        match &c.result {
            Ok(v) => println!("OK    {:<22} {v}", c.name),
            Err(e) => {
                bad += 1;
                println!("FAIL  {:<22} {e}", c.name);
            }
        }
    }
    println!("\n{} of {} items OK", checks.len() - bad, checks.len());
    std::process::exit((bad > 0) as i32);
}
