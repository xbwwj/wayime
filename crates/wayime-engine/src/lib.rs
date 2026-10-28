use std::{collections::HashMap, fs, io, path::Path};

use csv::ReaderBuilder;
use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("CSV error: {0}")]
    Validate(#[from] csv::Error),
}

pub type Result<T, E = Error> = ::core::result::Result<T, E>;

#[derive(Debug, Deserialize)]
pub struct Candidate {
    pub text: String,
    pub code: String,
    pub weight: Option<f64>,
    pub meta: Option<String>,
}

/// The dict is a map from code to candidates.
pub type Dict = HashMap<String, Vec<Candidate>>;

// PERF: lack streaming, insert order, etc
// XXX: separate io and parsing logic
pub fn read_dict(path: impl AsRef<Path>) -> Result<Dict> {
    let mut dict = Dict::new();

    let tsv = fs::read_to_string(path)?;

    let mut rdr = ReaderBuilder::new()
        .flexible(true)
        .delimiter(b'\t')
        .from_reader(tsv.as_bytes());

    for result in rdr.deserialize::<Candidate>() {
        let candidate = result?;
        dict.entry(
            // code may have space, we do not need space for now
            candidate.code.replace(" ", ""),
        )
        .or_default()
        .push(candidate);
    }

    // sort candidates for each code
    for candidates in dict.values_mut() {
        candidates.sort_unstable_by(|a, b| {
            b.weight
                .unwrap_or(f64::NEG_INFINITY)
                .total_cmp(&a.weight.unwrap_or(f64::NEG_INFINITY))
        });
    }

    Ok(dict)
}
