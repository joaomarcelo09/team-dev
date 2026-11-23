//! `jg-factory`: the Dark Factory command line.

use clap::Parser;

/// Dark Factory: autonomous lab for building software from specs.
#[derive(Parser)]
#[command(name = "jg-factory", version)]
struct Cli {}

fn main() {
    Cli::parse();
    println!("jg-factory: state dir {}", jgf_core::paths::data_dir().display());
}
