//! The process door's guards run this in place of `marv`: `__relay` (the relay in each process's sandbox), `serve` and
//! `parent` (`marv_session::processes::fixture`).

fn main() -> std::process::ExitCode {
    marv_session::processes::fixture::main()
}
