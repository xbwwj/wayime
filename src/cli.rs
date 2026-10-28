use std::{num::NonZeroUsize, path::PathBuf};

use clap::Parser;

#[derive(Debug, Parser)]
/// A minimalistic Wayland input method
pub struct Args {
    /// TSV dictionary path
    #[arg(long, value_name = "WAYIME_DICT_PATH")]
    pub dict: Option<PathBuf>,
    /// Number of candidates to display per page
    #[arg(long, default_value = "5", value_parser = parse_page_size)]
    pub page_size: NonZeroUsize,
}

fn parse_page_size(value: &str) -> std::result::Result<NonZeroUsize, String> {
    let page_size = value
        .parse::<usize>()
        .map_err(|_| "page size must be an integer between 1 and 10".to_owned())?;

    NonZeroUsize::new(page_size)
        .filter(|page_size| page_size.get() <= 10)
        .ok_or_else(|| "page size must be between 1 and 10".to_owned())
}
