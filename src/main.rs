use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    let result = match symbol_server::Config::from_env() {
        Ok(config) => symbol_server::run(config).await,
        Err(e) => Err(e),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
