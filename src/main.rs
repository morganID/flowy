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
    /// List the Safari tabs the extension can see, and which one flowy drives
    Tabs,
    /// Show the Safari profile flowy drives, or pin a different one
    Profile { name: Option<String> },
    /// Generate images for a prompt in the current project and print their URLs
    Generate {
        prompt: String,
        /// How long to wait for the images, in seconds
        #[arg(long, default_value_t = 240)]
        timeout: u64,
        /// How many times to retry a failed request
        #[arg(long, default_value_t = 2)]
        retries: usize,
        /// Read the results from the DOM only, skipping the generation RPC's reply
        #[arg(long)]
        no_rpc: bool,
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
    // safari-driver reads SAFARI_PROFILE on every command, so the pinned profile has to be in
    // the environment before anything touches the bridge. Without this a run silently drives
    // whichever profile the extension resolves — often the personal one.
    browser::export_profile();
    let result = match cli.command {
        Command::Open => ui::open_flow(),
        Command::Reload => ui::reload_flow(),
        Command::Status => ui::status(),
        Command::Focus => ui::focus_tab(),
        Command::Tabs => page::tabs(),
        Command::Profile { name } => match name.as_deref() {
            Some(name) => browser::BrowserState::pin_profile(Some(name)).map(|pinned| {
                println!("profile: {}", pinned.unwrap_or_default());
            }),
            None => {
                match browser::BrowserState::load().profile {
                    Some(profile) => println!("profile: {profile}"),
                    None => println!("profile: (not pinned — the extension chooses)"),
                }
                Ok(())
            }
        },
        Command::Generate {
            prompt,
            timeout,
            retries,
            no_rpc,
        } => generate::generate(&prompt, Duration::from_secs(timeout), retries, !no_rpc),
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
