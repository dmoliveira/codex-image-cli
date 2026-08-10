use std::{
    collections::HashSet,
    fs,
    io::Read,
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{cli::OutputFormat, output::derive_file_names, report::AppError};

pub const MANIFEST_SCHEMA_VERSION: u8 = 1;
pub const MAX_MANIFEST_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_MANIFEST_ASSETS: usize = 50_000;
const MAX_MANIFEST_LINE_BYTES: usize = 512 * 1024;
const MAX_PROMPT_CHARS: usize = 32_000;

#[derive(Debug, Clone)]
pub struct Manifest {
    pub path: PathBuf,
    pub assets: Vec<ManifestAsset>,
    pub source_sha256: String,
    pub bytes: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct ManifestAsset {
    pub id: String,
    pub prompt: String,
    pub stem: String,
}

impl ManifestAsset {
    pub fn output_name(&self, format: OutputFormat) -> String {
        derive_file_names(1, Some(&self.stem), None, format)
            .expect("manifest stems are validated during parsing")
            .remove(0)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifestAsset {
    id: String,
    prompt: String,
    name: Option<String>,
}

pub fn load(path: &Path) -> Result<Manifest, AppError> {
    let path = absolute_path(path)?;
    validate_path_components(
        &path,
        "manifest_unreadable",
        "The manifest path must not contain symlinked components.",
    )?;
    let metadata = fs::symlink_metadata(&path).map_err(|_| {
        AppError::preflight(
            "manifest_unreadable",
            "The manifest must be an existing regular UTF-8 file; no request was sent.",
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(AppError::preflight(
            "manifest_unreadable",
            "The manifest must be an existing regular UTF-8 file; no request was sent.",
        ));
    }
    if metadata.len() > MAX_MANIFEST_BYTES as u64 {
        return Err(AppError::usage(
            "manifest_too_large",
            "The manifest exceeds the 64 MiB safety limit.",
        ));
    }
    let mut file = fs::File::open(&path).map_err(|_| {
        AppError::preflight(
            "manifest_unreadable",
            "The manifest could not be opened safely; no request was sent.",
        )
    })?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take((MAX_MANIFEST_BYTES as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| {
            AppError::preflight(
                "manifest_unreadable",
                "The manifest could not be read safely; no request was sent.",
            )
        })?;
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(AppError::usage(
            "manifest_too_large",
            "The manifest exceeds the 64 MiB safety limit.",
        ));
    }
    let source_sha256 = sha256(&bytes);
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        AppError::usage("manifest_invalid_utf8", "The manifest must be UTF-8 JSONL.")
    })?;
    let mut assets = Vec::new();
    let mut ids = HashSet::new();
    let mut stems = HashSet::new();
    for (line_number, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        if line.len() > MAX_MANIFEST_LINE_BYTES {
            return Err(AppError::usage(
                "manifest_line_too_large",
                format!(
                    "Manifest line {} exceeds the 512 KiB safety limit.",
                    line_number + 1
                ),
            ));
        }
        if assets.len() >= MAX_MANIFEST_ASSETS {
            return Err(AppError::usage(
                "manifest_asset_limit",
                "The manifest exceeds the 50,000 asset safety limit.",
            ));
        }
        let raw: RawManifestAsset = serde_json::from_str(line).map_err(|_| {
            AppError::usage(
                "manifest_invalid_record",
                format!(
                    "Manifest line {} is not a valid asset record.",
                    line_number + 1
                ),
            )
        })?;
        validate_stem(&raw.id, "id", line_number + 1)?;
        if raw.prompt.trim().is_empty() || raw.prompt.chars().count() > MAX_PROMPT_CHARS {
            return Err(AppError::usage(
                "manifest_prompt_invalid",
                format!(
                    "Manifest line {} prompt must contain non-whitespace text and be at most 32,000 Unicode scalar values.",
                    line_number + 1
                ),
            ));
        }
        let stem = raw.name.unwrap_or_else(|| raw.id.clone());
        validate_stem(&stem, "name", line_number + 1)?;
        if !ids.insert(raw.id.clone()) {
            return Err(AppError::usage(
                "manifest_duplicate_id",
                format!("Manifest line {} repeats an asset id.", line_number + 1),
            ));
        }
        if !stems.insert(stem.clone()) {
            return Err(AppError::usage(
                "manifest_duplicate_name",
                format!("Manifest line {} repeats an output stem.", line_number + 1),
            ));
        }
        assets.push(ManifestAsset {
            id: raw.id,
            prompt: raw.prompt,
            stem,
        });
    }
    if assets.is_empty() {
        return Err(AppError::usage(
            "manifest_empty",
            "The manifest must contain at least one non-empty JSONL asset record.",
        ));
    }
    Ok(Manifest {
        path,
        assets,
        source_sha256,
        bytes: bytes.len(),
    })
}

fn validate_stem(value: &str, field: &str, line: usize) -> Result<(), AppError> {
    let mut chars = value.chars();
    let valid = !value.is_empty()
        && value.len() <= 80
        && chars
            .next()
            .is_some_and(|character| character.is_ascii_alphanumeric())
        && chars
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'));
    if valid {
        Ok(())
    } else {
        Err(AppError::usage(
            "manifest_unsafe_name",
            format!(
                "Manifest line {line} {field} must be 1-80 characters: an ASCII letter/digit followed only by letters, digits, '_' or '-'."
            ),
        ))
    }
}

fn absolute_path(path: &Path) -> Result<PathBuf, AppError> {
    if path.is_absolute() {
        return Ok(path.to_owned());
    }
    std::env::current_dir()
        .map(|directory| directory.join(path))
        .map_err(|_| {
            AppError::preflight(
                "working_directory_unavailable",
                "The current directory could not be resolved.",
            )
        })
}

pub fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn validate_output_directory(path: &Path) -> Result<PathBuf, AppError> {
    let absolute = absolute_path(path)?;
    if absolute.to_str().is_none() {
        return Err(AppError::preflight(
            "unsafe_output_directory",
            "The run output directory path must be valid UTF-8.",
        ));
    }
    validate_path_components(
        &absolute,
        "unsafe_output_directory",
        "The run output directory must not contain symlinked components or '..'.",
    )?;
    let metadata = fs::symlink_metadata(&absolute).map_err(|_| {
        AppError::preflight(
            "output_directory_unavailable",
            "The run output directory must already exist as a regular directory.",
        )
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(AppError::preflight(
            "unsafe_output_directory",
            "The run output directory must be an existing directory without a symlinked final component.",
        ));
    }
    Ok(absolute)
}

pub fn validate_path_components(
    path: &Path,
    code: &'static str,
    message: &'static str,
) -> Result<(), AppError> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::Normal(name) => {
                current.push(name);
                let metadata = fs::symlink_metadata(&current)
                    .map_err(|_| AppError::preflight(code, message))?;
                if metadata.file_type().is_symlink() {
                    return Err(AppError::preflight(code, message));
                }
            }
            Component::CurDir => {}
            Component::ParentDir => return Err(AppError::preflight(code, message)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn parses_bounded_assets_and_hashes_source() {
        let directory = tempfile::Builder::new()
            .prefix(".codex-image-manifest-test-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let path = directory.path().join("assets.jsonl");
        let mut file = fs::File::create(&path).unwrap();
        writeln!(file, r#"{{"id":"one","prompt":"first"}}"#).unwrap();
        writeln!(
            file,
            r#"{{"id":"two","prompt":"second","name":"hero-two"}}"#
        )
        .unwrap();
        let manifest = load(&path).unwrap();
        assert_eq!(manifest.assets.len(), 2);
        assert_eq!(manifest.assets[0].stem, "one");
        assert_eq!(
            manifest.assets[1].output_name(OutputFormat::Png),
            "hero-two.png"
        );
        assert_eq!(manifest.source_sha256, sha256(&fs::read(&path).unwrap()));
    }

    #[test]
    fn rejects_duplicate_ids_and_stems() {
        let directory = tempfile::Builder::new()
            .prefix(".codex-image-manifest-test-")
            .tempdir_in(env!("CARGO_MANIFEST_DIR"))
            .unwrap();
        let path = directory.path().join("assets.jsonl");
        fs::write(
            &path,
            r#"{"id":"one","prompt":"first","name":"same"}
{"id":"one","prompt":"second","name":"other"}
"#,
        )
        .unwrap();
        assert_eq!(load(&path).unwrap_err().code, "manifest_duplicate_id");
    }
}
