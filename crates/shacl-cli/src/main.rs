//! Command line entry point. Fleshed out once the validator lands.

fn main() -> anyhow::Result<()> {
    println!("shacl {}", env!("CARGO_PKG_VERSION"));
    Ok(())
}
