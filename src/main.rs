#[cfg(unix)]
fn main() -> std::process::ExitCode {
    use clap::Parser;
    match ssh_tunnel_manager::cli::run(ssh_tunnel_manager::cli::Cli::parse()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("boring-rs: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(unix))]
fn main() {
    eprintln!("boring-rs currently supports macOS and Linux. On Windows, run it inside WSL.");
    std::process::exit(1);
}
