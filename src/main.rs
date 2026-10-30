//! Google Flow CLI, driven through the UI in Safari.
mod browser;
mod generate;
mod net;
mod page;
mod state;
mod ui;

use clap::{Parser, Subcommand};
use std::{
    path::PathBuf,
    process,
    time::Duration,
};

#[derive(Parser)]
#[command(name = "flowy", about = "Google Flow client using Safari UI automation")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Open Flow in Safari
    Open,
    /// Reload the Flow tab (needed after signing in on an already loaded page)
    Reload,
    /// Show the Flow tab and page state the automation currently sees
    Status,
    /// Bring the Flow tab to the front
    Focus,
    /// Generate images for a prompt in the current project and print their URLs
    Generate {
        prompt: String,
        /// How long to wait for the images, in seconds
        #[arg(long, default_value_t = 240)]
        timeout: u64,
        /// How many times to retry a failed request
        #[arg(long, default_value_t = 2)]
        retries: usize,
    },
    /// Type a prompt without submitting it
    Prompt { text: String },
    /// Submit whatever the prompt box already holds
    Fire,
    /// List the generated images in the current project
    Images,
    /// Download generated images (all of them, or one by its 1-based index)
    Download {
        #[arg(long)]
        index: Option<usize>,
        #[arg(long, default_value = "out")]
        out: PathBuf,
    },
    /// Install the request recorder right after a navigation (direct-RPC work)
    NetInstall,
    /// Print the recorded requests, optionally filtered by URL fragment
    NetDump { filter: Option<String> },
    /// Evaluate a JavaScript expression in the Flow tab and print the JSON result
    Eval { script: String },
    /// Same as `eval`, but read the script from a file (keeps quoting sane for long scripts)
    EvalFile { path: String },
}

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Open => ui::open_flow(),
        Command::Reload => ui::reload_flow(),
        Command::Status => ui::status(),
        Command::Focus => ui::focus_tab(),
        Command::Generate {
            prompt,
            timeout,
            retries,
        } => generate::generate(&prompt, Duration::from_secs(timeout), retries),
        Command::Prompt { text } => generate::prompt(&text),
        Command::Fire => generate::fire(),
        Command::Images => generate::images(),
        Command::Download { index, out } => generate::download(index, &out),
        Command::NetInstall => net::install(),
        Command::NetDump { filter } => net::dump(filter.as_deref()),
        Command::Eval { script } => ui::eval_script(&script),
        Command::EvalFile { path } => ui::eval_file(&path),
    };
    if let Err(error) = result {
        eprintln!("flowy: {error:#}");
        process::exit(1);
    }
}
