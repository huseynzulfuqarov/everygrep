use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--version" | "-V") => {
            println!("everygrep {}", everygrep_core::VERSION);
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage: everygrep --version");
            ExitCode::from(2)
        }
    }
}
