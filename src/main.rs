use std::process::ExitCode;

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("beacon: cannot start the async runtime: {e}");
            return ExitCode::from(1);
        }
    };
    let code = runtime.block_on(beacon::cli::run(std::env::args_os()));
    // Do not wait for blocking work that is still going. A read of stdin that
    // never finishes (`attach -` fed by a slow producer) must not keep the
    // process alive after the command is over; the exit ends it.
    runtime.shutdown_background();
    ExitCode::from(code)
}
