//! The `ocbot` binary: a thin `#[tokio::main]` over [`ocbot::run::start`].

#[tokio::main]
async fn main() -> std::process::ExitCode {
    ocbot::run::start().await
}
