use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    ExitCode::from(beacon::cli::run(std::env::args_os()).await)
}
