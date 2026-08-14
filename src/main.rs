mod cli;
mod config;
mod doctor;
mod evalcache;
mod initzsh;
mod lock;
mod paths;
mod pool;
mod remove;
mod sources;
mod update;
mod version;

fn main() -> std::process::ExitCode {
    cli::run()
}
