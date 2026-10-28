use std::{num::NonZeroUsize, path::PathBuf};

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "wayime", version, about = "Wayland input method")]
pub struct Args {
    /// Path to the TSV dictionary.
    #[arg(long, value_name = "PATH")]
    pub dict: Option<PathBuf>,

    /// Number of candidates displayed on each page.
    #[arg(long, default_value = "5", value_name = "N")]
    pub page_size: NonZeroUsize,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Manage the speech-recognition model.
    Model {
        #[command(subcommand)]
        command: ModelCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum ModelCommand {
    /// Show the download status of the Whisper model.
    Show,

    /// Download the Whisper model.
    Download,
}
