use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{cli::OutputFormat, report::AppError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OutputIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Debug, Clone)]
pub struct RecoveryArtifact {
    pub final_name: String,
    pub stage_name: String,
    pub expected_stage_id: OutputIdentity,
    pub expected_id: Option<OutputIdentity>,
    pub expected_sha256: String,
}

#[derive(Debug, Clone)]
pub struct OutputVerificationArtifact {
    pub output_name: String,
    pub expected_output_id: OutputIdentity,
    pub expected_sha256: String,
}

#[derive(Debug, Clone)]
pub struct RetainedVerificationArtifact {
    pub name: String,
    pub expected_id: OutputIdentity,
}

#[derive(Debug, Clone)]
pub struct RecoveryVerificationArtifact {
    pub output_name: String,
    pub expected_output_id: OutputIdentity,
    pub expected_sha256: String,
    pub stage_name: String,
    pub expected_stage_id: OutputIdentity,
}

#[derive(Debug, Clone, Copy)]
pub struct RecoveryObservation {
    pub final_matches: bool,
    pub stage_matches: bool,
}

pub fn derive_file_names(
    count: u8,
    name: Option<&str>,
    prefix: Option<&str>,
    format: OutputFormat,
) -> Result<Vec<String>, AppError> {
    let stem = name.or(prefix).unwrap_or("codex-image");
    validate_stem(stem)?;

    if count == 1 {
        return Ok(vec![format!("{stem}.{}", format.extension())]);
    }
    let width = count.to_string().len().max(2);
    Ok((1..=count)
        .map(|index| format!("{stem}-{index:0width$}.{}", format.extension()))
        .collect())
}

/// Derive a display-only plan. Real generation pins the directory with
/// descriptor-relative operations before it makes a request.
pub fn derive_output_paths(directory: &Path, file_names: &[String]) -> Vec<PathBuf> {
    file_names.iter().map(|name| directory.join(name)).collect()
}

fn validate_stem(stem: &str) -> Result<(), AppError> {
    let valid_length = !stem.is_empty() && stem.len() <= 80;
    let mut characters = stem.chars();
    let starts_safely = characters
        .next()
        .is_some_and(|character| character.is_ascii_alphanumeric());
    let contains_only_safe_characters = characters
        .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'));
    if valid_length && starts_safely && contains_only_safe_characters {
        Ok(())
    } else {
        Err(AppError::usage(
            "unsafe_output_stem",
            "--name and --prefix must be 1-80 characters: an ASCII letter/digit followed only by letters, digits, '_' or '-'. Paths and extensions are not allowed.",
        ))
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod secure {
    use std::{
        fs::File,
        io::{Read, Seek, SeekFrom, Write},
        os::unix::fs::MetadataExt,
        path::{Component, Path, PathBuf},
    };

    use getrandom::fill as random_fill;
    use rustix::{
        fs::{openat, renameat_with, statat, AtFlags, FileType, Mode, OFlags, RenameFlags, CWD},
        io::Errno,
    };

    use crate::report::AppError;

    use sha2::{Digest, Sha256};

    use super::{
        HashSet, OutputIdentity, OutputVerificationArtifact, RecoveryArtifact, RecoveryObservation,
        RecoveryVerificationArtifact, RetainedVerificationArtifact,
    };

    type FileId = OutputIdentity;

    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[derive(Debug, Clone, Copy)]
    struct EntryInfo {
        id: FileId,
        file_type: FileType,
    }

    struct ChainNode {
        name: std::ffi::OsString,
        file: File,
        id: FileId,
    }

    /// Every filesystem operation is relative to this retained directory
    /// descriptor. Replacing the visible `--output-dir` path cannot redirect
    /// this process to a different directory object.
    struct PinnedDirectory {
        root: File,
        root_id: FileId,
        nodes: Vec<ChainNode>,
        display: PathBuf,
    }

    impl PinnedDirectory {
        fn open(input: &Path) -> Result<Self, AppError> {
            let display = absolute_clean_path(input)?;
            let root = open_root().map_err(|_| {
                AppError::preflight(
                    "secure_output_directory_unavailable",
                    "The filesystem root could not be opened securely; no image request was sent.",
                )
            })?;
            let root_id = file_id(&root).map_err(|_| {
                AppError::preflight(
                    "secure_output_directory_unavailable",
                    "The filesystem root could not be inspected securely; no image request was sent.",
                )
            })?;
            let mut nodes = Vec::new();
            for component in display.components() {
                let Component::Normal(name) = component else {
                    continue;
                };
                let parent = nodes
                    .last()
                    .map(|node: &ChainNode| &node.file)
                    .unwrap_or(&root);
                let file = open_directory_component(parent, name).map_err(|_| {
                    AppError::preflight(
                        "unsafe_output_directory",
                        "--output-dir must be an existing directory with no symlinked path components.",
                    )
                })?;
                let id = file_id(&file).map_err(|_| {
                    AppError::preflight(
                        "secure_output_directory_unavailable",
                        "--output-dir could not be inspected securely; no image request was sent.",
                    )
                })?;
                nodes.push(ChainNode {
                    name: name.to_os_string(),
                    file,
                    id,
                });
            }
            Ok(Self {
                root,
                root_id,
                nodes,
                display,
            })
        }

        fn directory(&self) -> &File {
            self.nodes
                .last()
                .map(|node| &node.file)
                .unwrap_or(&self.root)
        }

        fn display_path(&self, name: &str) -> PathBuf {
            self.display.join(name)
        }

        /// Revalidate the original path chain just before success. It cannot
        /// freeze a directory after return, but it prevents this process from
        /// claiming a stale/replaced visible path as its successful output.
        fn verify_visible_chain(&self) -> bool {
            let Ok(root) = open_root() else {
                return false;
            };
            let Ok(root_id) = file_id(&root) else {
                return false;
            };
            if root_id != self.root_id {
                return false;
            }
            let mut current = root;
            for node in &self.nodes {
                let Ok(next) = open_directory_component(&current, &node.name) else {
                    return false;
                };
                let Ok(id) = file_id(&next) else {
                    return false;
                };
                if id != node.id {
                    return false;
                }
                current = next;
            }
            true
        }
    }

    pub fn entry_exists(output_dir: &Path, name: &str) -> Result<bool, AppError> {
        if !is_safe_output_name(name) {
            return Err(AppError::preflight(
                "unsafe_output_name",
                "The output entry name is not a safe single filename.",
            ));
        }
        let directory = PinnedDirectory::open(output_dir).map_err(|_| {
            AppError::preflight(
                "output_directory_unavailable",
                "The output directory could not be pinned safely before the run.",
            )
        })?;
        raw_entry_info(directory.directory(), name)
            .map(|entry| entry.is_some())
            .map_err(|_| {
                AppError::preflight(
                    "output_directory_unavailable",
                    "The output directory could not be inspected safely before the run.",
                )
            })
    }

    enum Reservation {
        NoOverwrite,
        Overwrite { expected_id: Option<FileId> },
    }

    struct Stage {
        name: String,
        id: FileId,
        file: Option<File>,
    }

    struct Entry {
        final_name: String,
        destination: PathBuf,
        reservation: Reservation,
        stage: Stage,
        expected_sha256: Option<String>,
        publication_attempted: bool,
        committed: bool,
    }

    #[derive(Debug)]
    pub struct CommitResult {
        pub outputs: Vec<PathBuf>,
        pub retained_artifacts: Vec<PathBuf>,
    }

    /// A no-delete transaction. Generated private stage/backup entries are
    /// deliberately retained when an error or overwrite occurs: POSIX offers
    /// no identity-bound unlink, so automatic pathname cleanup could delete a
    /// competing file in a hostile concurrent directory.
    pub struct OutputTransaction {
        directory: PinnedDirectory,
        entries: Vec<Entry>,
        overwrite: bool,
        recovering: bool,
        transaction_id: String,
        finalized: bool,
    }

    impl OutputTransaction {
        pub fn reserve(
            output_dir: &Path,
            file_names: Vec<String>,
            overwrite: bool,
        ) -> Result<Self, AppError> {
            let unique: HashSet<_> = file_names.iter().collect();
            if unique.len() != file_names.len() {
                return Err(AppError::preflight(
                    "duplicate_output_path",
                    "The requested output names are not unique; no image request was sent.",
                ));
            }
            let directory = PinnedDirectory::open(output_dir)?;
            let mut transaction_id = String::new();
            transaction_id.push_str(&random_token()?);
            let mut transaction = Self {
                directory,
                entries: Vec::with_capacity(file_names.len()),
                overwrite,
                recovering: false,
                transaction_id,
                finalized: false,
            };

            for (index, final_name) in file_names.into_iter().enumerate() {
                let reservation = transaction.inspect_reservation(&final_name)?;
                let stage_name = transaction.private_name("stage", index);
                let (file, stage_id) =
                    create_private_file(transaction.directory.directory(), &stage_name).map_err(
                        |error| preflight_create_error(error, "output_reservation_failed"),
                    )?;
                transaction.entries.push(Entry {
                    destination: transaction.directory.display_path(&final_name),
                    final_name,
                    reservation,
                    stage: Stage {
                        name: stage_name,
                        id: stage_id,
                        file: Some(file),
                    },
                    expected_sha256: None,
                    publication_attempted: false,
                    committed: false,
                });
            }
            Ok(transaction)
        }

        pub fn reserve_with_expected_targets(
            output_dir: &Path,
            file_names: Vec<String>,
            expected_ids: Vec<Option<OutputIdentity>>,
        ) -> Result<Self, AppError> {
            if file_names.len() != expected_ids.len() {
                return Err(AppError::output_commit(
                    "internal_reservation_count_mismatch",
                    "The persisted output reservation count did not match the requested outputs.",
                ));
            }
            let directory = PinnedDirectory::open(output_dir)?;
            let transaction_id = random_token()?;
            let mut transaction = Self {
                directory,
                entries: Vec::with_capacity(file_names.len()),
                overwrite: false,
                recovering: true,
                transaction_id,
                finalized: false,
            };
            let unique: HashSet<_> = file_names.iter().collect();
            if unique.len() != file_names.len() {
                return Err(AppError::preflight(
                    "duplicate_output_path",
                    "The requested output names are not unique; no image request was sent.",
                ));
            }
            for ((index, final_name), expected_id) in
                file_names.into_iter().enumerate().zip(expected_ids)
            {
                let reservation =
                    transaction.inspect_expected_reservation(&final_name, expected_id)?;
                let stage_name = transaction.private_name("stage", index);
                let (file, stage_id) =
                    create_private_file(transaction.directory.directory(), &stage_name).map_err(
                        |error| preflight_create_error(error, "output_reservation_failed"),
                    )?;
                transaction.entries.push(Entry {
                    destination: transaction.directory.display_path(&final_name),
                    final_name,
                    reservation,
                    stage: Stage {
                        name: stage_name,
                        id: stage_id,
                        file: Some(file),
                    },
                    expected_sha256: None,
                    publication_attempted: false,
                    committed: false,
                });
            }
            Ok(transaction)
        }

        pub fn recover(
            output_dir: &Path,
            artifacts: Vec<RecoveryArtifact>,
        ) -> Result<Self, AppError> {
            let directory = PinnedDirectory::open(output_dir)?;
            let mut transaction = Self {
                directory,
                entries: Vec::with_capacity(artifacts.len()),
                overwrite: false,
                recovering: true,
                transaction_id: "recovery".to_owned(),
                finalized: false,
            };
            let mut names = HashSet::new();
            for artifact in artifacts {
                if !names.insert(artifact.final_name.clone())
                    || !is_safe_output_name(&artifact.final_name)
                    || !is_private_stage_name(&artifact.stage_name)
                {
                    return Err(AppError::preflight(
                        "publishing_recovery_invalid",
                        "The persisted publication recovery names are unsafe or duplicated.",
                    ));
                }
                let Some(stage_info) =
                    raw_entry_info(transaction.directory.directory(), &artifact.stage_name)
                        .map_err(|_| {
                            AppError::preflight(
                        "publishing_output_missing",
                        "A persisted staged output is missing or could not be inspected safely.",
                    )
                        })?
                else {
                    return Err(AppError::preflight(
                        "publishing_output_missing",
                        "A persisted staged output is missing; retrieve the remote Batch output to repair it.",
                    ));
                };
                if stage_info.file_type != FileType::RegularFile {
                    return Err(AppError::preflight(
                        "publishing_output_unsafe",
                        "A persisted staged output is not a regular non-symlink file.",
                    ));
                }
                if stage_info.id != artifact.expected_stage_id {
                    return Err(AppError::output_commit(
                        "publishing_output_changed",
                        "A persisted staged output changed before recovery; no replacement was attempted.",
                    ));
                }
                transaction.entries.push(Entry {
                    destination: transaction.directory.display_path(&artifact.final_name),
                    final_name: artifact.final_name,
                    reservation: match artifact.expected_id {
                        Some(expected_id) => Reservation::Overwrite {
                            expected_id: Some(expected_id),
                        },
                        None => Reservation::NoOverwrite,
                    },
                    stage: Stage {
                        name: artifact.stage_name,
                        id: stage_info.id,
                        file: None,
                    },
                    expected_sha256: Some(artifact.expected_sha256),
                    publication_attempted: false,
                    committed: false,
                });
            }
            Ok(transaction)
        }

        fn inspect_reservation(&self, final_name: &str) -> Result<Reservation, AppError> {
            match raw_entry_info(self.directory.directory(), final_name) {
                Ok(None) => Ok(if self.overwrite {
                    Reservation::Overwrite { expected_id: None }
                } else {
                    Reservation::NoOverwrite
                }),
                Ok(Some(_)) if !self.overwrite => Err(AppError::preflight(
                    "output_path_exists",
                    "An output path already exists. Choose another --name/--prefix or pass --overwrite deliberately.",
                )),
                Ok(Some(info)) if info.file_type == FileType::RegularFile => {
                    Ok(Reservation::Overwrite {
                        expected_id: Some(info.id),
                    })
                }
                Ok(Some(_)) => Err(AppError::preflight(
                    "non_regular_output_target",
                    "Output targets must be regular files when they already exist.",
                )),
                Err(_) => Err(AppError::preflight(
                    "output_target_unavailable",
                    "An output target could not be inspected safely; no image request was sent.",
                )),
            }
        }

        fn inspect_expected_reservation(
            &self,
            final_name: &str,
            expected_id: Option<OutputIdentity>,
        ) -> Result<Reservation, AppError> {
            match (raw_entry_info(self.directory.directory(), final_name), expected_id) {
                (Ok(Some(info)), Some(expected_id))
                    if info.id == expected_id && info.file_type == FileType::RegularFile =>
                {
                    Ok(Reservation::Overwrite {
                        expected_id: Some(expected_id),
                    })
                }
                (Ok(None), _) => Ok(Reservation::NoOverwrite),
                (Ok(Some(_)), _) => Err(AppError::output_commit(
                    "output_target_changed",
                    "An output target changed since the publication reservation was persisted; no replacement was attempted.",
                )),
                _ => Err(AppError::output_commit(
                    "output_target_unavailable",
                    "An output target could not be inspected safely; no replacement was attempted.",
                )),
            }
        }

        pub fn stage_all(&mut self, images: &[Vec<u8>]) -> Result<(), AppError> {
            if images.len() != self.entries.len() {
                return Err(AppError::output_commit(
                    "internal_image_count_mismatch",
                    "The validated image count did not match the reserved output count; no success was reported.",
                ));
            }
            for (index, image) in images.iter().enumerate() {
                self.stage_one(index, image)?;
            }
            Ok(())
        }

        pub fn stage_selected(
            &mut self,
            selected_indices: &[usize],
            images: &[Vec<u8>],
        ) -> Result<(), AppError> {
            if selected_indices.len() != self.entries.len() {
                return Err(AppError::output_commit(
                    "internal_image_count_mismatch",
                    "The selected image count did not match the reserved output count; no success was reported.",
                ));
            }
            for (position, image_index) in selected_indices.iter().copied().enumerate() {
                let Some(image) = images.get(image_index) else {
                    return Err(AppError::output_commit(
                        "internal_image_index_mismatch",
                        "A validated image index did not match the reserved output count; no success was reported.",
                    ));
                };
                self.stage_one(position, image)?;
            }
            Ok(())
        }

        pub fn stage_one(&mut self, index: usize, image: &[u8]) -> Result<(), AppError> {
            let Some(entry) = self.entries.get_mut(index) else {
                return Err(AppError::output_commit(
                    "internal_image_index_mismatch",
                    "The validated image index did not match the reserved output count; no success was reported.",
                ));
            };
            let Some(file) = entry.stage.file.as_mut() else {
                return Err(AppError::output_commit(
                    "missing_staged_output",
                    "A staged output was unexpectedly unavailable. The image request may have been billed; no success was reported.",
                ));
            };
            let expected_sha256 = sha256_hex(image);
            if file.set_len(0).is_err()
                || file.seek(SeekFrom::Start(0)).is_err()
                || file.write_all(image).is_err()
                || file.sync_all().is_err()
            {
                return Err(AppError::output_commit(
                    "output_staging_failed",
                    "The returned image could not be staged securely. The image request may have been billed; no success was reported.",
                ));
            }
            entry.stage.file.take();
            entry.expected_sha256 = Some(expected_sha256);
            Ok(())
        }

        pub fn commit_all(&mut self) -> Result<CommitResult, AppError> {
            for index in 0..self.entries.len() {
                if self.entries[index].committed {
                    continue;
                }
                let result = if self.recovering {
                    match self.entries[index].reservation {
                        Reservation::Overwrite { .. } => self.publish_overwrite(index),
                        Reservation::NoOverwrite => self.publish_no_overwrite(index),
                    }
                } else if self.overwrite {
                    self.publish_overwrite(index)
                } else {
                    self.publish_no_overwrite(index)
                };
                if let Err(mut error) = result {
                    error.add_possibly_modified_paths(self.preserve_artifacts());
                    self.finalized = true;
                    return Err(error);
                }
            }

            if self.directory.directory().sync_all().is_err()
                || !self.directory.verify_visible_chain()
                || !self.verify_final_entries()
                || !self.verify_retained_backups()
            {
                let mut error = AppError::output_commit(
                    "output_path_changed",
                    "The output directory or a published target changed during the transaction. No success was reported; inspect the listed paths before retrying.",
                );
                error.add_possibly_modified_paths(self.preserve_artifacts());
                self.finalized = true;
                return Err(error);
            }

            let retained_artifacts = self.retained_backups();
            let outputs = self.destinations();
            self.finalized = true;
            Ok(CommitResult {
                outputs,
                retained_artifacts,
            })
        }

        fn publish_no_overwrite(&mut self, index: usize) -> Result<(), AppError> {
            let (final_name, stage_name, stage_id) = {
                let entry = &self.entries[index];
                (
                    entry.final_name.clone(),
                    entry.stage.name.clone(),
                    entry.stage.id,
                )
            };
            rename_no_replace(self.directory.directory(), &stage_name, &final_name)
            .map_err(|_| {
                AppError::output_commit(
                    "output_target_changed",
                    "An output target appeared before atomic no-clobber publication. No success was reported; the image request may have been billed.",
                )
            })?;
            self.entries[index].publication_attempted = true;
            if raw_entry_info(self.directory.directory(), &final_name)
                .ok()
                .flatten()
                .is_some_and(|info| info.id == stage_id && info.file_type == FileType::RegularFile)
            {
                self.entries[index].committed = true;
                Ok(())
            } else {
                Err(AppError::output_commit(
                    "output_target_changed",
                    "An output target changed during publication. No success was reported; the image request may have been billed.",
                ))
            }
        }

        fn publish_overwrite(&mut self, index: usize) -> Result<(), AppError> {
            let (final_name, stage_name, stage_id, expected_id) = {
                let entry = &self.entries[index];
                let Reservation::Overwrite { expected_id } = entry.reservation else {
                    unreachable!("overwrite transaction has the wrong reservation")
                };
                (
                    entry.final_name.clone(),
                    entry.stage.name.clone(),
                    entry.stage.id,
                    expected_id,
                )
            };
            if let Some(expected_id) = expected_id {
                let Some(final_info) = raw_entry_info(self.directory.directory(), &final_name)
                    .ok()
                    .flatten()
                else {
                    self.entries[index].publication_attempted = true;
                    return Err(AppError::output_commit(
                        "output_target_changed",
                        "An overwrite target disappeared after the publication reservation was persisted; no replacement was attempted.",
                    ));
                };
                if final_info.id != expected_id || final_info.file_type != FileType::RegularFile {
                    self.entries[index].publication_attempted = true;
                    return Err(AppError::output_commit(
                        "output_target_changed",
                        "An overwrite target changed after the publication reservation was persisted; no replacement was attempted.",
                    ));
                }
                exchange_names(self.directory.directory(), &stage_name, &final_name).map_err(|_| {
                    AppError::output_commit(
                        "output_publish_failed",
                        "A staged output could not be published atomically. The image request may have been billed; no success was reported.",
                    )
                })?;
                self.entries[index].publication_attempted = true;
                let final_info = raw_entry_info(self.directory.directory(), &final_name)
                    .ok()
                    .flatten();
                let backup_info = raw_entry_info(self.directory.directory(), &stage_name)
                    .ok()
                    .flatten();
                if final_info.is_some_and(|info| {
                    info.id == stage_id && info.file_type == FileType::RegularFile
                }) && backup_info.is_some_and(|info| {
                    info.id == expected_id && info.file_type == FileType::RegularFile
                }) {
                    self.entries[index].committed = true;
                    return Ok(());
                }
                // Do not try a second automatic exchange. An attacker could
                // replace either name between checks; the displaced object is
                // preserved under the private stage name for manual recovery.
                return Err(AppError::output_commit(
                    "output_target_changed",
                    "An overwrite target changed during atomic publication. The displaced object was preserved in a private artifact; no success was reported.",
                ));
            }

            rename_no_replace(self.directory.directory(), &stage_name, &final_name).map_err(|_| {
                AppError::output_commit(
                    "output_target_changed",
                    "A previously absent output target appeared before publication. No success was reported; the image request may have been billed.",
                )
            })?;
            self.entries[index].publication_attempted = true;
            if raw_entry_info(self.directory.directory(), &final_name)
                .ok()
                .flatten()
                .is_some_and(|info| info.id == stage_id && info.file_type == FileType::RegularFile)
            {
                self.entries[index].committed = true;
                Ok(())
            } else {
                Err(AppError::output_commit(
                    "output_target_changed",
                    "An overwrite target changed during publication. No success was reported; the image request may have been billed.",
                ))
            }
        }

        fn verify_final_entries(&self) -> bool {
            self.entries.iter().all(|entry| {
                entry
                    .expected_sha256
                    .as_deref()
                    .is_some_and(|expected_sha256| {
                        Self::entry_matches_digest(
                            self.directory.directory(),
                            &entry.final_name,
                            entry.stage.id,
                            expected_sha256,
                        )
                    })
            })
        }

        fn entry_matches_digest(
            directory: &File,
            name: &str,
            expected_id: FileId,
            expected_sha256: &str,
        ) -> bool {
            let Ok(mut file) = open_file_component(directory, name.as_ref()) else {
                return false;
            };
            let Ok(before) = file.metadata() else {
                return false;
            };
            let before_id = FileId {
                device: before.dev(),
                inode: before.ino(),
            };
            if !before.file_type().is_file()
                || before.nlink() != 1
                || before_id != expected_id
                || before.len() > crate::image::MAX_IMAGE_BYTES as u64
            {
                return false;
            }
            let mut bytes =
                Vec::with_capacity(before.len().min(crate::image::MAX_IMAGE_BYTES as u64) as usize);
            if Read::by_ref(&mut file)
                .take((crate::image::MAX_IMAGE_BYTES as u64).saturating_add(1))
                .read_to_end(&mut bytes)
                .is_err()
                || bytes.len() > crate::image::MAX_IMAGE_BYTES
            {
                return false;
            }
            let Ok(after) = file.metadata() else {
                return false;
            };
            if !before.file_type().is_file()
                || after.nlink() != 1
                || before.dev() != after.dev()
                || before.ino() != after.ino()
                || before.len() != after.len()
                || before.mtime() != after.mtime()
                || before.mtime_nsec() != after.mtime_nsec()
                || before.ctime() != after.ctime()
                || before.ctime_nsec() != after.ctime_nsec()
            {
                return false;
            }
            raw_entry_info(directory, name)
                .ok()
                .flatten()
                .is_some_and(|info| {
                    info.id == expected_id
                        && info.file_type == FileType::RegularFile
                        && sha256_hex(&bytes) == expected_sha256
                })
        }

        fn verify_retained_backups(&self) -> bool {
            self.entries.iter().all(|entry| match entry.reservation {
                Reservation::Overwrite {
                    expected_id: Some(expected_id),
                } => raw_entry_info(self.directory.directory(), &entry.stage.name)
                    .ok()
                    .flatten()
                    .is_some_and(|info| {
                        info.id == expected_id && info.file_type == FileType::RegularFile
                    }),
                _ => true,
            })
        }

        /// Error/abort paths intentionally retain only our private stage
        /// entries and any visibly modified final targets. There is no
        /// name-based cleanup that could unlink a competitor's replacement.
        pub fn abort(&mut self) -> Vec<PathBuf> {
            if self.finalized {
                return Vec::new();
            }
            self.finalized = true;
            self.preserve_artifacts()
        }

        pub fn staged_artifact_paths(&self) -> Vec<PathBuf> {
            self.entries
                .iter()
                .map(|entry| self.directory.display_path(&entry.stage.name))
                .collect()
        }

        pub fn expected_target_ids(&self) -> Vec<Option<OutputIdentity>> {
            self.entries
                .iter()
                .map(|entry| match entry.reservation {
                    Reservation::Overwrite { expected_id } => expected_id,
                    Reservation::NoOverwrite => None,
                })
                .collect()
        }

        pub fn staged_artifact_ids(&self) -> Vec<OutputIdentity> {
            self.entries.iter().map(|entry| entry.stage.id).collect()
        }

        pub fn planned_retained_artifact_paths(&self) -> Vec<PathBuf> {
            self.entries
                .iter()
                .filter_map(|entry| match entry.reservation {
                    Reservation::Overwrite {
                        expected_id: Some(_),
                    } => Some(self.directory.display_path(&entry.stage.name)),
                    _ => None,
                })
                .collect()
        }

        fn preserve_artifacts(&self) -> Vec<PathBuf> {
            let mut paths = Vec::new();
            for entry in &self.entries {
                if entry.publication_attempted {
                    paths.push(entry.destination.clone());
                    paths.push(self.directory.display_path(&entry.stage.name));
                }
                if !entry.publication_attempted
                    && raw_entry_info(self.directory.directory(), &entry.stage.name)
                        .ok()
                        .flatten()
                        .is_some_and(|info| info.id == entry.stage.id)
                {
                    paths.push(self.directory.display_path(&entry.stage.name));
                }
            }
            if !self.directory.verify_visible_chain() {
                paths.extend(self.destinations());
            }
            paths.sort();
            paths.dedup();
            paths
        }

        fn retained_backups(&self) -> Vec<PathBuf> {
            let mut paths = self
                .entries
                .iter()
                .filter_map(|entry| match entry.reservation {
                    Reservation::Overwrite {
                        expected_id: Some(expected_id),
                    } if raw_entry_info(self.directory.directory(), &entry.stage.name)
                        .ok()
                        .flatten()
                        .is_some_and(|info| info.id == expected_id) =>
                    {
                        Some(self.directory.display_path(&entry.stage.name))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>();
            paths.sort();
            paths.dedup();
            paths
        }

        fn destinations(&self) -> Vec<PathBuf> {
            self.entries
                .iter()
                .map(|entry| entry.destination.clone())
                .collect()
        }

        fn private_name(&self, kind: &str, index: usize) -> String {
            format!(".codex-image-{kind}-{}-{index}", self.transaction_id)
        }
    }

    fn absolute_clean_path(input: &Path) -> Result<PathBuf, AppError> {
        if input.as_os_str().is_empty()
            || input
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(AppError::preflight(
                "unsafe_output_directory",
                "--output-dir must be an existing directory without '..' path components.",
            ));
        }
        let absolute = if input.is_absolute() {
            input.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|_| {
                    AppError::preflight(
                        "working_directory_unavailable",
                        "The current working directory could not be resolved; no image request was sent.",
                    )
                })?
                .join(input)
        };
        let mut clean = PathBuf::new();
        for component in absolute.components() {
            match component {
                Component::RootDir => clean.push(component.as_os_str()),
                Component::Normal(name) => clean.push(name),
                Component::CurDir => {}
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(AppError::preflight(
                        "unsafe_output_directory",
                        "--output-dir must be an existing absolute/relative directory without unsafe components.",
                    ))
                }
            }
        }
        if !clean.is_absolute() {
            return Err(AppError::preflight(
                "unsafe_output_directory",
                "--output-dir must resolve to an absolute directory path.",
            ));
        }
        Ok(clean)
    }

    fn is_safe_output_name(name: &str) -> bool {
        let path = Path::new(name);
        path.components().count() == 1
            && matches!(path.components().next(), Some(Component::Normal(_)))
    }

    fn is_private_stage_name(name: &str) -> bool {
        name.starts_with(".codex-image-stage-") && is_safe_output_name(name)
    }

    fn random_token() -> Result<String, AppError> {
        let mut bytes = [0_u8; 16];
        random_fill(&mut bytes).map_err(|_| {
            AppError::preflight(
                "secure_random_unavailable",
                "A secure random transaction identifier could not be created; no image request was sent.",
            )
        })?;
        Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
    }

    fn open_root() -> rustix::io::Result<File> {
        let fd = openat(
            CWD,
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(fd.into())
    }

    fn open_directory_component(parent: &File, name: &std::ffi::OsStr) -> rustix::io::Result<File> {
        let fd = openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(fd.into())
    }

    pub fn read_regular_file(path: &Path, max_bytes: usize) -> Result<Vec<u8>, AppError> {
        read_regular_file_with_identity(path, max_bytes).map(|(bytes, _)| bytes)
    }

    pub fn read_regular_file_with_identity(
        path: &Path,
        max_bytes: usize,
    ) -> Result<(Vec<u8>, OutputIdentity), AppError> {
        let parent = path.parent().ok_or_else(|| {
            AppError::preflight(
                "publishing_output_unreadable",
                "A persisted publishing output has no safe parent directory.",
            )
        })?;
        let name = path.file_name().ok_or_else(|| {
            AppError::preflight(
                "publishing_output_unreadable",
                "A persisted publishing output has no safe filename.",
            )
        })?;
        let directory = PinnedDirectory::open(parent).map_err(|_| {
            AppError::preflight(
                "publishing_output_unreadable",
                "The persisted publishing output directory could not be pinned safely.",
            )
        })?;
        let mut file = open_file_component(directory.directory(), name).map_err(|_| {
            AppError::preflight(
                "publishing_output_missing",
                "A persisted publishing output is missing or could not be opened safely.",
            )
        })?;
        let metadata = file.metadata().map_err(|_| {
            AppError::preflight(
                "publishing_output_unreadable",
                "A persisted publishing output could not be inspected safely.",
            )
        })?;
        if !metadata.file_type().is_file() {
            return Err(AppError::preflight(
                "publishing_output_unsafe",
                "A persisted publishing output is not a regular non-symlink file.",
            ));
        }
        let identity = OutputIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        if metadata.len() > max_bytes as u64 {
            return Err(AppError::preflight(
                "publishing_output_too_large",
                "A persisted publishing output exceeds the local image safety limit.",
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len().min(max_bytes as u64) as usize);
        std::io::Read::by_ref(&mut file)
            .take((max_bytes as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| {
                AppError::preflight(
                    "publishing_output_unreadable",
                    "A persisted publishing output could not be read safely.",
                )
            })?;
        if bytes.len() > max_bytes {
            return Err(AppError::preflight(
                "publishing_output_too_large",
                "A persisted publishing output exceeds the local image safety limit.",
            ));
        }
        Ok((bytes, identity))
    }

    pub fn read_and_sync_regular_file(
        path: &Path,
        max_bytes: usize,
        expected_id: OutputIdentity,
    ) -> Result<Vec<u8>, AppError> {
        let parent = path.parent().ok_or_else(|| {
            AppError::output_commit(
                "published_output_path_invalid",
                "A published output has no safe parent directory.",
            )
        })?;
        let name = path.file_name().ok_or_else(|| {
            AppError::output_commit(
                "published_output_path_invalid",
                "A published output has no safe filename.",
            )
        })?;
        let directory = PinnedDirectory::open(parent).map_err(|_| {
            AppError::output_commit(
                "published_output_sync_failed",
                "The published output directory could not be pinned for durability verification.",
            )
        })?;
        let mut file = open_file_component(directory.directory(), name).map_err(|_| {
            AppError::output_commit(
                "published_output_sync_failed",
                "A published output could not be reopened safely for durability verification.",
            )
        })?;
        let metadata = file.metadata().map_err(|_| {
            AppError::output_commit(
                "published_output_sync_failed",
                "A published output could not be inspected for durability verification.",
            )
        })?;
        if !metadata.file_type().is_file() {
            return Err(AppError::output_commit(
                "published_output_sync_failed",
                "A published output is not a regular file during durability verification.",
            ));
        }
        let identity = OutputIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        if identity != expected_id {
            return Err(AppError::output_commit(
                "published_output_changed",
                "A published output changed identity during durability verification.",
            ));
        }
        if metadata.len() > max_bytes as u64 {
            return Err(AppError::output_commit(
                "published_output_too_large",
                "A published output exceeds the local image safety limit.",
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len().min(max_bytes as u64) as usize);
        std::io::Read::by_ref(&mut file)
            .take((max_bytes as u64).saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| {
                AppError::output_commit(
                    "published_output_sync_failed",
                    "A published output could not be read safely for durability verification.",
                )
            })?;
        if bytes.len() > max_bytes {
            return Err(AppError::output_commit(
                "published_output_too_large",
                "A published output exceeds the local image safety limit.",
            ));
        }
        file.sync_all().map_err(|_| {
            AppError::output_commit(
                "published_output_sync_failed",
                "A published output could not be synchronized safely.",
            )
        })?;
        let visible_name = name.to_str().ok_or_else(|| {
            AppError::output_commit(
                "published_output_path_invalid",
                "A published output has a non-UTF-8 filename.",
            )
        })?;
        let visible = raw_entry_info(directory.directory(), visible_name)
            .ok()
            .flatten()
            .is_some_and(|info| info.id == identity && info.file_type == FileType::RegularFile);
        if !visible || !directory.verify_visible_chain() {
            return Err(AppError::output_commit(
                "published_output_changed",
                "The published output path changed during durability verification.",
            ));
        }
        directory.directory().sync_all().map_err(|_| {
            AppError::output_commit(
                "published_output_sync_failed",
                "The published output directory could not be synchronized safely.",
            )
        })?;
        Ok(bytes)
    }

    pub fn sync_regular_file_identity(
        path: &Path,
        expected_id: OutputIdentity,
    ) -> Result<(), AppError> {
        let parent = path.parent().ok_or_else(|| {
            AppError::output_commit(
                "publishing_retained_changed",
                "A retained publication artifact has no safe parent directory.",
            )
        })?;
        let name = path.file_name().ok_or_else(|| {
            AppError::output_commit(
                "publishing_retained_changed",
                "A retained publication artifact has no safe filename.",
            )
        })?;
        let directory = PinnedDirectory::open(parent).map_err(|_| {
            AppError::output_commit(
                "publishing_retained_changed",
                "The retained publication artifact directory could not be pinned safely.",
            )
        })?;
        let file = open_file_component(directory.directory(), name).map_err(|_| {
            AppError::output_commit(
                "publishing_retained_changed",
                "A retained publication artifact is missing or could not be opened safely.",
            )
        })?;
        let metadata = file.metadata().map_err(|_| {
            AppError::output_commit(
                "publishing_retained_changed",
                "A retained publication artifact could not be inspected safely.",
            )
        })?;
        let identity = OutputIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        };
        if !metadata.file_type().is_file() || identity != expected_id {
            return Err(AppError::output_commit(
                "publishing_retained_changed",
                "A retained publication artifact changed since it was recorded.",
            ));
        }
        file.sync_all().map_err(|_| {
            AppError::output_commit(
                "publishing_retained_changed",
                "A retained publication artifact could not be synchronized safely.",
            )
        })?;
        let visible_name = name.to_str().ok_or_else(|| {
            AppError::output_commit(
                "publishing_retained_changed",
                "A retained publication artifact has a non-UTF-8 filename.",
            )
        })?;
        let visible = raw_entry_info(directory.directory(), visible_name)
            .ok()
            .flatten()
            .is_some_and(|info| info.id == identity && info.file_type == FileType::RegularFile);
        if !visible || !directory.verify_visible_chain() {
            return Err(AppError::output_commit(
                "publishing_retained_changed",
                "A retained publication artifact path changed during synchronization.",
            ));
        }
        directory.directory().sync_all().map_err(|_| {
            AppError::output_commit(
                "publishing_retained_changed",
                "A retained publication directory could not be synchronized safely.",
            )
        })?;
        Ok(())
    }

    pub fn verify_and_sync_plan(
        output_dir: &Path,
        artifacts: &[OutputVerificationArtifact],
        retained_artifacts: &[RetainedVerificationArtifact],
    ) -> Result<Vec<PathBuf>, AppError> {
        let directory = PinnedDirectory::open(output_dir).map_err(|_| {
            AppError::output_commit(
                "published_output_sync_failed",
                "The published output directory could not be pinned for plan verification.",
            )
        })?;
        let mut seen_names = HashSet::new();
        let mut checked = Vec::with_capacity(artifacts.len() + retained_artifacts.len());
        let mut outputs = Vec::with_capacity(artifacts.len());
        for artifact in artifacts {
            if !is_safe_output_name(&artifact.output_name)
                || !seen_names.insert(artifact.output_name.clone())
            {
                return Err(AppError::preflight(
                    "publishing_journal_invalid",
                    "The publication plan contains an unsafe or duplicate output name.",
                ));
            }
            if !OutputTransaction::entry_matches_digest(
                directory.directory(),
                &artifact.output_name,
                artifact.expected_output_id,
                &artifact.expected_sha256,
            ) {
                return Err(AppError::preflight(
                    "publishing_recovery_required",
                    "A published output changed or does not match the persisted digest.",
                ));
            }
            checked.push((artifact.output_name.clone(), artifact.expected_output_id));
            outputs.push(directory.display_path(&artifact.output_name));
        }
        for retained_artifact in retained_artifacts {
            if !is_private_stage_name(&retained_artifact.name)
                || !seen_names.insert(retained_artifact.name.clone())
            {
                return Err(AppError::preflight(
                    "publishing_journal_invalid",
                    "The publication plan contains an unsafe or duplicate retained artifact name.",
                ));
            }
            let retained =
                open_file_component(directory.directory(), retained_artifact.name.as_ref())
                    .map_err(|_| {
                        AppError::output_commit(
                    "publishing_retained_changed",
                    "A retained publication artifact is missing or could not be opened safely.",
                )
                    })?;
            let retained_metadata = retained.metadata().map_err(|_| {
                AppError::output_commit(
                    "publishing_retained_changed",
                    "A retained publication artifact could not be inspected safely.",
                )
            })?;
            let retained_identity = OutputIdentity {
                device: retained_metadata.dev(),
                inode: retained_metadata.ino(),
            };
            if !retained_metadata.file_type().is_file()
                || retained_identity != retained_artifact.expected_id
            {
                return Err(AppError::output_commit(
                    "publishing_retained_changed",
                    "A retained publication artifact changed since it was recorded.",
                ));
            }
            retained.sync_all().map_err(|_| {
                AppError::output_commit(
                    "publishing_retained_changed",
                    "A retained publication artifact could not be synchronized safely.",
                )
            })?;
            checked.push((retained_artifact.name.clone(), retained_identity));
        }
        directory.directory().sync_all().map_err(|_| {
            AppError::output_commit(
                "published_output_sync_failed",
                "The published output directory could not be synchronized after plan verification.",
            )
        })?;
        if !directory.verify_visible_chain()
            || checked.iter().any(|(name, expected_id)| {
                raw_entry_info(directory.directory(), name)
                    .ok()
                    .flatten()
                    .is_none_or(|info| {
                        info.id != *expected_id || info.file_type != FileType::RegularFile
                    })
            })
            || artifacts.iter().any(|artifact| {
                !OutputTransaction::entry_matches_digest(
                    directory.directory(),
                    &artifact.output_name,
                    artifact.expected_output_id,
                    &artifact.expected_sha256,
                )
            })
            || !directory.verify_visible_chain()
        {
            return Err(AppError::output_commit(
                "published_output_changed",
                "The publication directory or an artifact changed during plan verification.",
            ));
        }
        Ok(outputs)
    }

    pub fn inspect_recovery_plan(
        output_dir: &Path,
        artifacts: &[RecoveryVerificationArtifact],
    ) -> Result<Vec<RecoveryObservation>, AppError> {
        let directory = PinnedDirectory::open(output_dir).map_err(|_| {
            AppError::output_commit(
                "publishing_output_changed",
                "The output directory could not be pinned for recovery inspection.",
            )
        })?;
        let mut seen_names = HashSet::new();
        let mut checked = Vec::with_capacity(artifacts.len() * 2);
        let mut observations = Vec::with_capacity(artifacts.len());
        for artifact in artifacts {
            if !is_safe_output_name(&artifact.output_name)
                || !is_private_stage_name(&artifact.stage_name)
                || !seen_names.insert(artifact.output_name.clone())
                || !seen_names.insert(artifact.stage_name.clone())
            {
                return Err(AppError::preflight(
                    "publishing_recovery_invalid",
                    "The persisted recovery names are unsafe.",
                ));
            }
            let final_matches = recovery_entry_matches(
                directory.directory(),
                &artifact.output_name,
                artifact.expected_output_id,
                &artifact.expected_sha256,
            )?;
            if final_matches {
                checked.push((artifact.output_name.clone(), artifact.expected_output_id));
            }
            let stage_matches = recovery_entry_matches(
                directory.directory(),
                &artifact.stage_name,
                artifact.expected_stage_id,
                &artifact.expected_sha256,
            )?;
            if stage_matches {
                checked.push((artifact.stage_name.clone(), artifact.expected_stage_id));
            }
            observations.push(RecoveryObservation {
                final_matches,
                stage_matches,
            });
        }
        if !directory.verify_visible_chain()
            || checked.iter().any(|(name, expected_id)| {
                raw_entry_info(directory.directory(), name)
                    .ok()
                    .flatten()
                    .is_none_or(|info| {
                        info.id != *expected_id || info.file_type != FileType::RegularFile
                    })
            })
            || artifacts
                .iter()
                .zip(observations.iter())
                .any(|(artifact, observation)| {
                    (observation.final_matches
                        && !OutputTransaction::entry_matches_digest(
                            directory.directory(),
                            &artifact.output_name,
                            artifact.expected_output_id,
                            &artifact.expected_sha256,
                        ))
                        || (observation.stage_matches
                            && !OutputTransaction::entry_matches_digest(
                                directory.directory(),
                                &artifact.stage_name,
                                artifact.expected_stage_id,
                                &artifact.expected_sha256,
                            ))
                })
            || !directory.verify_visible_chain()
        {
            return Err(AppError::output_commit(
                "publishing_output_changed",
                "The output directory or a recovery artifact changed during inspection.",
            ));
        }
        Ok(observations)
    }

    pub fn sync_regular_files(paths: &[PathBuf]) -> Result<(), AppError> {
        let mut synced_directories = HashSet::new();
        for path in paths {
            let parent = path.parent().ok_or_else(|| {
                AppError::output_commit(
                    "published_output_path_invalid",
                    "A published output has no safe parent directory.",
                )
            })?;
            let name = path.file_name().ok_or_else(|| {
                AppError::output_commit(
                    "published_output_path_invalid",
                    "A published output has no safe filename.",
                )
            })?;
            let directory = PinnedDirectory::open(parent).map_err(|_| {
                AppError::output_commit(
                    "published_output_sync_failed",
                    "The published output directory could not be pinned for durability verification.",
                )
            })?;
            let file = open_file_component(directory.directory(), name).map_err(|_| {
                AppError::output_commit(
                    "published_output_sync_failed",
                    "A published output could not be reopened safely for durability verification.",
                )
            })?;
            if !file
                .metadata()
                .map_err(|_| {
                    AppError::output_commit(
                        "published_output_sync_failed",
                        "A published output could not be inspected for durability verification.",
                    )
                })?
                .file_type()
                .is_file()
            {
                return Err(AppError::output_commit(
                    "published_output_sync_failed",
                    "A published output is not a regular file during durability verification.",
                ));
            }
            file.sync_all().map_err(|_| {
                AppError::output_commit(
                    "published_output_sync_failed",
                    "A published output could not be synchronized safely.",
                )
            })?;
            if synced_directories.insert(parent.to_path_buf()) {
                directory.directory().sync_all().map_err(|_| {
                    AppError::output_commit(
                        "published_output_sync_failed",
                        "The published output directory could not be synchronized safely.",
                    )
                })?;
            }
        }
        Ok(())
    }

    pub fn verify_regular_file_identity(
        path: &Path,
        expected_id: OutputIdentity,
    ) -> Result<(), AppError> {
        let parent = path.parent().ok_or_else(|| {
            AppError::output_commit(
                "publishing_retained_changed",
                "A retained publication artifact has no safe parent directory.",
            )
        })?;
        let name = path.file_name().ok_or_else(|| {
            AppError::output_commit(
                "publishing_retained_changed",
                "A retained publication artifact has no safe filename.",
            )
        })?;
        let directory = PinnedDirectory::open(parent).map_err(|_| {
            AppError::output_commit(
                "publishing_retained_changed",
                "The retained publication artifact directory could not be pinned safely.",
            )
        })?;
        let file = open_file_component(directory.directory(), name).map_err(|_| {
            AppError::output_commit(
                "publishing_retained_changed",
                "A retained publication artifact is missing or could not be opened safely.",
            )
        })?;
        if !file
            .metadata()
            .map_err(|_| {
                AppError::output_commit(
                    "publishing_retained_changed",
                    "A retained publication artifact could not be inspected safely.",
                )
            })?
            .file_type()
            .is_file()
        {
            return Err(AppError::output_commit(
                "publishing_retained_changed",
                "A retained publication artifact is not a regular file.",
            ));
        }
        let identity = file_id(&file).map_err(|_| {
            AppError::output_commit(
                "publishing_retained_changed",
                "A retained publication artifact identity could not be inspected safely.",
            )
        })?;
        if identity != expected_id {
            return Err(AppError::output_commit(
                "publishing_retained_changed",
                "A retained publication artifact changed since it was recorded.",
            ));
        }
        let visible_name = name.to_str().ok_or_else(|| {
            AppError::output_commit(
                "publishing_retained_changed",
                "A retained publication artifact has a non-UTF-8 filename.",
            )
        })?;
        let visible = raw_entry_info(directory.directory(), visible_name)
            .ok()
            .flatten()
            .is_some_and(|info| info.id == identity && info.file_type == FileType::RegularFile);
        if !visible || !directory.verify_visible_chain() {
            return Err(AppError::output_commit(
                "publishing_retained_changed",
                "A retained publication artifact path changed during verification.",
            ));
        }
        Ok(())
    }

    fn create_private_file(directory: &File, name: &str) -> rustix::io::Result<(File, FileId)> {
        let fd = openat(
            directory,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let file: File = fd.into();
        let id = file_id(&file).map_err(|_| Errno::IO)?;
        Ok((file, id))
    }

    fn open_file_component(parent: &File, name: &std::ffi::OsStr) -> rustix::io::Result<File> {
        let fd = openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        Ok(fd.into())
    }

    fn file_id(file: &File) -> std::io::Result<FileId> {
        let metadata = file.metadata()?;
        Ok(FileId {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn recovery_entry_matches(
        directory: &File,
        name: &str,
        expected_id: OutputIdentity,
        expected_sha256: &str,
    ) -> Result<bool, AppError> {
        Ok(OutputTransaction::entry_matches_digest(
            directory,
            name,
            expected_id,
            expected_sha256,
        ))
    }

    fn raw_entry_info(directory: &File, name: &str) -> rustix::io::Result<Option<EntryInfo>> {
        match statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(Some(EntryInfo {
                id: FileId {
                    device: u64::try_from(i128::from(stat.st_dev)).map_err(|_| Errno::IO)?,
                    inode: stat.st_ino,
                },
                file_type: FileType::from_raw_mode(stat.st_mode),
            })),
            Err(Errno::NOENT) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn exchange_names(directory: &File, first: &str, second: &str) -> rustix::io::Result<()> {
        renameat_with(directory, first, directory, second, RenameFlags::EXCHANGE)
    }

    fn rename_no_replace(
        directory: &File,
        source: &str,
        destination: &str,
    ) -> rustix::io::Result<()> {
        renameat_with(
            directory,
            source,
            directory,
            destination,
            RenameFlags::NOREPLACE,
        )
    }

    fn preflight_create_error(error: Errno, default_code: &'static str) -> AppError {
        let code = if error == Errno::EXIST {
            "output_path_exists"
        } else {
            default_code
        };
        AppError::preflight(
            code,
            "An output destination could not be reserved securely before the image request; no image request was sent.",
        )
    }

    #[cfg(test)]
    mod tests {
        use std::fs;

        use super::*;

        fn safe_tempdir() -> tempfile::TempDir {
            tempfile::Builder::new()
                .prefix(".codex-image-output-test-")
                .tempdir_in(env!("CARGO_MANIFEST_DIR"))
                .unwrap()
        }

        #[test]
        fn transaction_stages_and_commits_a_reserved_file() {
            let directory = safe_tempdir();
            let destination = directory.path().join("hero.png");
            let mut transaction =
                OutputTransaction::reserve(directory.path(), vec!["hero.png".to_owned()], false)
                    .unwrap();
            transaction.stage_all(&[b"image bytes".to_vec()]).unwrap();
            let result = transaction.commit_all().unwrap();
            assert_eq!(result.outputs, vec![destination.clone()]);
            assert!(result.retained_artifacts.is_empty());
            assert_eq!(fs::read(destination).unwrap(), b"image bytes");
        }

        #[test]
        fn publication_rejects_same_inode_content_tampering() {
            let directory = safe_tempdir();
            let mut transaction =
                OutputTransaction::reserve(directory.path(), vec!["hero.png".to_owned()], false)
                    .unwrap();
            transaction.stage_all(&[b"image bytes".to_vec()]).unwrap();
            let stage = transaction.staged_artifact_paths()[0].clone();
            fs::write(stage, b"tampered bytes").unwrap();
            let error = transaction.commit_all().unwrap_err();
            assert_eq!(error.code, "output_path_changed");
        }

        #[test]
        fn recovery_read_uses_a_bounded_regular_file_descriptor() {
            let directory = safe_tempdir();
            let destination = directory.path().join("hero.png");
            fs::write(&destination, b"image bytes").unwrap();
            assert_eq!(read_regular_file(&destination, 32).unwrap(), b"image bytes");
            assert!(read_regular_file(&destination, 4).is_err());
        }

        #[test]
        fn overwrite_retains_an_identity_checked_backup() {
            let directory = safe_tempdir();
            let destination = directory.path().join("hero.png");
            fs::write(&destination, b"old image").unwrap();
            let mut transaction =
                OutputTransaction::reserve(directory.path(), vec!["hero.png".to_owned()], true)
                    .unwrap();
            transaction.stage_all(&[b"new image".to_vec()]).unwrap();
            let result = transaction.commit_all().unwrap();
            assert_eq!(fs::read(destination).unwrap(), b"new image");
            assert_eq!(result.retained_artifacts.len(), 1);
            assert_eq!(
                fs::read(&result.retained_artifacts[0]).unwrap(),
                b"old image"
            );
        }

        #[test]
        fn recovery_finishes_a_staged_overwrite_after_persisted_state() {
            let directory = safe_tempdir();
            let destination = directory.path().join("hero.png");
            fs::write(&destination, b"old image").unwrap();
            let mut transaction =
                OutputTransaction::reserve(directory.path(), vec!["hero.png".to_owned()], true)
                    .unwrap();
            transaction.stage_all(&[b"new image".to_vec()]).unwrap();
            let stage = transaction.staged_artifact_paths()[0].clone();
            let stage_id = transaction.staged_artifact_ids()[0];
            let expected_id = transaction.expected_target_ids()[0];
            drop(transaction);

            let mut recovery = OutputTransaction::recover(
                directory.path(),
                vec![RecoveryArtifact {
                    final_name: "hero.png".to_owned(),
                    stage_name: stage.file_name().unwrap().to_str().unwrap().to_owned(),
                    expected_stage_id: stage_id,
                    expected_id,
                    expected_sha256: sha256_hex(b"new image"),
                }],
            )
            .unwrap();
            let result = recovery.commit_all().unwrap();
            assert_eq!(fs::read(destination).unwrap(), b"new image");
            assert_eq!(result.retained_artifacts.len(), 1);
            assert_eq!(
                fs::read(&result.retained_artifacts[0]).unwrap(),
                b"old image"
            );
        }

        #[test]
        fn recovery_never_replaces_a_new_no_overwrite_target() {
            let directory = safe_tempdir();
            let mut transaction =
                OutputTransaction::reserve(directory.path(), vec!["hero.png".to_owned()], false)
                    .unwrap();
            transaction.stage_all(&[b"our image".to_vec()]).unwrap();
            let stage = transaction.staged_artifact_paths()[0].clone();
            let stage_id = transaction.staged_artifact_ids()[0];
            drop(transaction);
            fs::write(directory.path().join("hero.png"), b"competitor").unwrap();

            let mut recovery = OutputTransaction::recover(
                directory.path(),
                vec![RecoveryArtifact {
                    final_name: "hero.png".to_owned(),
                    stage_name: stage.file_name().unwrap().to_str().unwrap().to_owned(),
                    expected_stage_id: stage_id,
                    expected_id: None,
                    expected_sha256: sha256_hex(b"our image"),
                }],
            )
            .unwrap();
            assert!(recovery.commit_all().is_err());
            assert_eq!(
                fs::read(directory.path().join("hero.png")).unwrap(),
                b"competitor"
            );
        }

        #[test]
        fn retained_backup_identity_rejects_a_replaced_path() {
            let directory = safe_tempdir();
            let destination = directory.path().join("hero.png");
            fs::write(&destination, b"old image").unwrap();
            let mut transaction =
                OutputTransaction::reserve(directory.path(), vec!["hero.png".to_owned()], true)
                    .unwrap();
            let expected_id = transaction.expected_target_ids()[0].unwrap();
            transaction.stage_all(&[b"new image".to_vec()]).unwrap();
            let retained = transaction.commit_all().unwrap().retained_artifacts[0].clone();
            let replacement = directory.path().join("replacement");
            fs::write(&replacement, b"not the original").unwrap();
            fs::rename(replacement, &retained).unwrap();
            assert!(verify_regular_file_identity(&retained, expected_id).is_err());
        }

        #[test]
        fn non_overwrite_never_replaces_a_competitor() {
            let directory = safe_tempdir();
            let destination = directory.path().join("hero.png");
            let mut transaction =
                OutputTransaction::reserve(directory.path(), vec!["hero.png".to_owned()], false)
                    .unwrap();
            fs::write(&destination, b"competitor").unwrap();
            transaction.stage_all(&[b"our image".to_vec()]).unwrap();
            assert!(transaction.commit_all().is_err());
            assert_eq!(fs::read(&destination).unwrap(), b"competitor");
        }

        #[test]
        fn mismatched_overwrite_reports_both_public_and_private_paths() {
            let directory = safe_tempdir();
            let destination = directory.path().join("hero.png");
            fs::write(&destination, b"original").unwrap();
            let mut transaction =
                OutputTransaction::reserve(directory.path(), vec!["hero.png".to_owned()], true)
                    .unwrap();
            let preserved_original = directory.path().join("preserved-original.png");
            fs::rename(&destination, &preserved_original).unwrap();
            fs::write(&destination, b"competitor").unwrap();
            transaction.stage_all(&[b"our image".to_vec()]).unwrap();
            let error = transaction.commit_all().unwrap_err();
            assert!(error
                .possibly_modified_paths
                .iter()
                .any(|path| path == &destination));
            let private_path = error
                .possibly_modified_paths
                .iter()
                .find(|path| {
                    path.file_name()
                        .and_then(|name| name.to_str())
                        .is_some_and(|name| name.starts_with(".codex-image-stage-"))
                })
                .expect("mismatched exchange must identify the private recovery artifact");
            assert_eq!(fs::read(private_path).unwrap(), b"our image");
            assert_eq!(fs::read(destination).unwrap(), b"competitor");
        }

        #[test]
        fn detects_replaced_output_directory_before_success() {
            let root = safe_tempdir();
            let output = root.path().join("output");
            fs::create_dir(&output).unwrap();
            let old = root.path().join("old-output");
            let mut transaction =
                OutputTransaction::reserve(&output, vec!["hero.png".to_owned()], false).unwrap();
            fs::rename(&output, &old).unwrap();
            fs::create_dir(&output).unwrap();
            transaction.stage_all(&[b"our image".to_vec()]).unwrap();
            assert!(transaction.commit_all().is_err());
            assert!(!output.join("hero.png").exists());
        }

        #[cfg(unix)]
        #[test]
        fn refuses_symlinked_output_target() {
            use std::os::unix::fs::symlink;

            let directory = safe_tempdir();
            let outside = directory.path().join("outside.png");
            let target = directory.path().join("hero.png");
            fs::write(&outside, b"outside").unwrap();
            symlink(&outside, &target).unwrap();
            let error =
                OutputTransaction::reserve(directory.path(), vec!["hero.png".to_owned()], true)
                    .err()
                    .unwrap();
            assert_eq!(error.code, "non_regular_output_target");
            assert_eq!(fs::read(outside).unwrap(), b"outside");
        }
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use secure::{
    entry_exists, inspect_recovery_plan, read_and_sync_regular_file, read_regular_file,
    read_regular_file_with_identity, sync_regular_file_identity, sync_regular_files,
    verify_and_sync_plan, verify_regular_file_identity, CommitResult, OutputTransaction,
};

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub struct OutputTransaction;

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn entry_exists(output_dir: &Path, name: &str) -> Result<bool, AppError> {
    if name.is_empty()
        || Path::new(name).components().count() != 1
        || !matches!(
            Path::new(name).components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        return Err(AppError::preflight(
            "unsafe_output_name",
            "The output entry name is not a safe single filename.",
        ));
    }
    match std::fs::symlink_metadata(output_dir.join(name)) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(AppError::preflight(
            "output_directory_unavailable",
            "The output directory could not be inspected safely before the run.",
        )),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn read_regular_file(_path: &Path, _max_bytes: usize) -> Result<Vec<u8>, AppError> {
    Err(AppError::preflight(
        "secure_output_transactions_unsupported",
        "Secure output reads are currently supported only on macOS and Linux.",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn read_regular_file_with_identity(
    _path: &Path,
    _max_bytes: usize,
) -> Result<(Vec<u8>, OutputIdentity), AppError> {
    Err(AppError::preflight(
        "secure_output_transactions_unsupported",
        "Secure output reads are currently supported only on macOS and Linux.",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn read_and_sync_regular_file(
    _path: &Path,
    _max_bytes: usize,
    _expected_id: OutputIdentity,
) -> Result<Vec<u8>, AppError> {
    Err(AppError::preflight(
        "secure_output_transactions_unsupported",
        "Secure output durability checks are currently supported only on macOS and Linux.",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn sync_regular_files(_paths: &[PathBuf]) -> Result<(), AppError> {
    Err(AppError::preflight(
        "secure_output_transactions_unsupported",
        "Secure output durability checks are currently supported only on macOS and Linux.",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn verify_regular_file_identity(
    _path: &Path,
    _expected_id: OutputIdentity,
) -> Result<(), AppError> {
    Err(AppError::preflight(
        "secure_output_transactions_unsupported",
        "Secure output durability checks are currently supported only on macOS and Linux.",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn sync_regular_file_identity(
    _path: &Path,
    _expected_id: OutputIdentity,
) -> Result<(), AppError> {
    Err(AppError::preflight(
        "secure_output_transactions_unsupported",
        "Secure output durability checks are currently supported only on macOS and Linux.",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn verify_and_sync_plan(
    _output_dir: &Path,
    _artifacts: &[OutputVerificationArtifact],
    _retained_artifacts: &[RetainedVerificationArtifact],
) -> Result<Vec<PathBuf>, AppError> {
    Err(AppError::preflight(
        "secure_output_transactions_unsupported",
        "Secure output durability checks are currently supported only on macOS and Linux.",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn inspect_recovery_plan(
    _output_dir: &Path,
    _artifacts: &[RecoveryVerificationArtifact],
) -> Result<Vec<RecoveryObservation>, AppError> {
    Err(AppError::preflight(
        "secure_output_transactions_unsupported",
        "Secure output recovery checks are currently supported only on macOS and Linux.",
    ))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
#[derive(Debug)]
pub struct CommitResult {
    pub outputs: Vec<PathBuf>,
    pub retained_artifacts: Vec<PathBuf>,
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl OutputTransaction {
    pub fn reserve(
        _output_dir: &Path,
        _file_names: Vec<String>,
        _overwrite: bool,
    ) -> Result<Self, AppError> {
        Err(AppError::preflight(
            "secure_output_transactions_unsupported",
            "Secure output transactions are currently supported only on macOS and Linux. Use --dry-run on this platform; no image request was sent.",
        ))
    }

    pub fn reserve_with_expected_targets(
        _output_dir: &Path,
        _file_names: Vec<String>,
        _expected_ids: Vec<Option<OutputIdentity>>,
    ) -> Result<Self, AppError> {
        Err(AppError::preflight(
            "secure_output_transactions_unsupported",
            "Secure output transactions are currently supported only on macOS and Linux. Use --dry-run on this platform; no image request was sent.",
        ))
    }

    pub fn recover(
        _output_dir: &Path,
        _artifacts: Vec<RecoveryArtifact>,
    ) -> Result<Self, AppError> {
        Err(AppError::preflight(
            "secure_output_transactions_unsupported",
            "Secure output transactions are currently supported only on macOS and Linux. Use --dry-run on this platform; no image request was sent.",
        ))
    }

    pub fn stage_all(&mut self, _images: &[Vec<u8>]) -> Result<(), AppError> {
        unreachable!("unsupported platforms cannot reserve an output transaction")
    }

    pub fn stage_selected(
        &mut self,
        _selected_indices: &[usize],
        _images: &[Vec<u8>],
    ) -> Result<(), AppError> {
        unreachable!("unsupported platforms cannot reserve an output transaction")
    }

    pub fn stage_one(&mut self, _index: usize, _image: &[u8]) -> Result<(), AppError> {
        unreachable!("unsupported platforms cannot reserve an output transaction")
    }

    pub fn commit_all(&mut self) -> Result<CommitResult, AppError> {
        unreachable!("unsupported platforms cannot reserve an output transaction")
    }

    pub fn abort(&mut self) -> Vec<PathBuf> {
        Vec::new()
    }

    pub fn staged_artifact_paths(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    pub fn planned_retained_artifact_paths(&self) -> Vec<PathBuf> {
        Vec::new()
    }

    pub fn expected_target_ids(&self) -> Vec<Option<OutputIdentity>> {
        Vec::new()
    }

    pub fn staged_artifact_ids(&self) -> Vec<OutputIdentity> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_deterministic_filenames() {
        assert_eq!(
            derive_file_names(1, Some("hero"), None, OutputFormat::Png).unwrap(),
            vec!["hero.png"]
        );
        assert_eq!(
            derive_file_names(3, None, Some("hero"), OutputFormat::Webp).unwrap(),
            vec!["hero-01.webp", "hero-02.webp", "hero-03.webp"]
        );
    }

    #[test]
    fn rejects_path_like_stems() {
        for stem in [
            "../escape",
            "name.png",
            ".hidden",
            "has space",
            "name/slash",
        ] {
            assert!(derive_file_names(1, Some(stem), None, OutputFormat::Png).is_err());
        }
    }
}
