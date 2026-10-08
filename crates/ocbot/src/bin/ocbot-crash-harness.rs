//! The AC-6 crash-harness binary: a separate process for `tests/chaos_sigkill`.
//! It is a dev tool (`ocbot` is `publish = false`), not part of the bot.

use std::process::ExitCode;

fn main() -> ExitCode {
    let Ok(runtime) = tokio::runtime::Runtime::new() else {
        return ExitCode::FAILURE;
    };
    match runtime.block_on(ocbot::harness::run_from_env()) {
        Ok(()) => ExitCode::SUCCESS,
        // No printing (NFR-9 / print_stdout is denied): the test asserts on
        // process state, not stdout.
        Err(_) => ExitCode::FAILURE,
    }
}
