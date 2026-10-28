use anyhow::{Context, Result};
use hf_hub::HFClientSync;

const REPOSITORY_OWNER: &str = "ggerganov";
const REPOSITORY_NAME: &str = "whisper.cpp";
const REPOSITORY_ID: &str = "ggerganov/whisper.cpp";
const MODEL_FILENAME: &str = "ggml-base-q5_1.bin";

/// Show whether the configured Whisper model is present in the local cache.
pub fn show() -> Result<()> {
    let client = HFClientSync::new().context("failed to initialize Hugging Face Hub client")?;

    let cache = client
        .scan_cache()
        .send()
        .context("failed to scan Hugging Face cache")?;

    let cached_model = cache
        .repos
        .iter()
        .filter(|repo| repo.repo_type == "model" && repo.repo_id == REPOSITORY_ID)
        .flat_map(|repo| repo.revisions.iter())
        .flat_map(|revision| revision.files.iter())
        .find(|file| file.file_name == MODEL_FILENAME && file.file_path.is_file())
        .map(|file| (file.file_path.clone(), file.size_on_disk));

    println!("Repository: {REPOSITORY_ID}");
    println!("Model:      {MODEL_FILENAME}");

    match cached_model {
        Some((path, size)) => {
            println!("Status:     downloaded");
            println!("Size:       {}", format_size(size));
            println!("Path:       {}", path.display());
        }
        None => {
            println!("Status:     not downloaded");
            println!();
            println!("Run `wayime model download` to download the model.");
        }
    }

    for warning in &cache.warnings {
        eprintln!("warning: {warning}");
    }

    Ok(())
}

/// Download the configured Whisper model into the Hugging Face cache.
pub fn download() -> Result<()> {
    let client = HFClientSync::new().context("failed to initialize Hugging Face Hub client")?;

    println!("Repository: {REPOSITORY_ID}");
    println!("Model:      {MODEL_FILENAME}");
    println!("Downloading model...");

    let path = client
        .model(REPOSITORY_OWNER, REPOSITORY_NAME)
        .download_file()
        .filename(MODEL_FILENAME.to_owned())
        .send()
        .with_context(|| format!("failed to download {MODEL_FILENAME} from {REPOSITORY_ID}"))?;

    let size = std::fs::metadata(&path)
        .with_context(|| format!("failed to read model metadata: {}", path.display()))?
        .len();

    println!();
    println!("Model is available in the local cache.");
    println!("Size: {}", format_size(size));
    println!("Path: {}", path.display());

    Ok(())
}

fn format_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;

    if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}
