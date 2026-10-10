fn main() {
    use clap::Parser;
    let argv: Vec<String> = std::env::args().collect();
    // Bare invocation is a sidecar: that is the argv `gray plugin check`
    // and a profile `sidecar:` line both spawn, and it keeps the daemon
    // contract identical whether or not the caller names the mode.
    if argv.len() == 1 {
        let path = gray_telegram::config::default_path();
        let path = std::path::absolute(&path).unwrap_or(path);
        gray_telegram::sidecar::serve(&path);
    }
    let cli = gray_telegram::cli::Cli::parse();
    // Absolute, so the service, the lock argv and the saved workdir agree
    // no matter where setup was run from.
    let path = cli.config_path();
    let path = std::path::absolute(&path).unwrap_or(path);
    match gray_telegram::cli::run(&cli.command, &path) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
}
