use clap::Parser;

#[derive(Parser)]
#[command(
    name = "workloop",
    about = "GitHub development workloop (slice 0 harness)"
)]
struct Cli {}

fn main() {
    Cli::parse();
}
