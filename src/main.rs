use std::process::ExitCode;

use clap::Parser;
use korean_dict_epub::app::run;
use korean_dict_epub::cli::Cli;

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(plan) => {
            println!("{plan}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error[{}]: {error}", error.code());
            ExitCode::from(error.exit_code())
        }
    }
}
