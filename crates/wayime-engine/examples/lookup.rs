use std::error::Error;

use clap::Parser;
use wayime_engine::read_dict;

/// Paged query of dictionary.
#[derive(Parser)]
struct Args {
    /// path to the dictionary tsv file
    path: String,
    /// the code to lookup
    code: String,
    /// number of candidates of each path
    #[arg(long, default_value_t = 5)]
    page_size: usize,
}

fn main() -> Result<(), Box<dyn Error>> {
    let args = Args::parse();
    let dict = read_dict(&args.path)?;

    let code = args.code.replace(' ', "");
    let Some(cands) = dict.get(&code) else {
        eprintln!("no candidates for code: {code}");
        return Ok(());
    };

    let ps = args.page_size.max(1);
    for (i, page) in cands.chunks(ps).enumerate() {
        let items: Vec<String> = page
            .iter()
            .enumerate()
            .map(|(j, c)| format!("[{}] {}", j + 1, c.text))
            .collect();
        println!("P{}: {}", i + 1, items.join(" "));
    }

    Ok(())
}
