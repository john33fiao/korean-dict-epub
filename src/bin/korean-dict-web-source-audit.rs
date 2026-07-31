use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use korean_dict_epub::web_source_audit::{DEFAULT_OUTPUT, DEFAULT_SOURCE, run_audit};

#[derive(Debug, Parser)]
#[command(
    name = "korean-dict-web-source-audit",
    version,
    about = "Audit source identifiers and dictionary relations without creating a database"
)]
struct Arguments {
    /// Read-only root of the tracked NIKL dictionary XML submodule
    #[arg(long, value_name = "PATH", default_value = DEFAULT_SOURCE)]
    source: PathBuf,

    /// Ignored local directory for deterministic JSON and Markdown reports
    #[arg(long, value_name = "PATH", default_value = DEFAULT_OUTPUT)]
    output: PathBuf,

    /// Atomically replace both existing reports
    #[arg(long)]
    overwrite: bool,
}

fn main() -> ExitCode {
    let arguments = Arguments::parse();
    match run_audit(&arguments.source, &arguments.output, arguments.overwrite) {
        Ok(report) => {
            let relations = report
                .relation_status_counts
                .values()
                .map(|counts| counts.total)
                .sum::<u64>();
            println!("schema={}", report.schema);
            println!("source_commit={}", report.source_commit);
            println!("input_files={}", report.input_files.len());
            println!("relations={relations}");
            println!("output={}", arguments.output.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("error[KWEB-E002]: {error}");
            ExitCode::from(1)
        }
    }
}
