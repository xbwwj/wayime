use std::io;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("CSV error: {0}")]
    Validate(#[from] csv::Error),
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("candidate page index is out of range")]
    PageOutOfRange,
}

pub type Result<T, E = Error> = ::core::result::Result<T, E>;
