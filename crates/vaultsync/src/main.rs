fn main() {
    if let Err(e) = vaultsync_engine::cli::main() {
        eprintln!("Error: {e:?}");
        std::process::exit(1);
    }
}
