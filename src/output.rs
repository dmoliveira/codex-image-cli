use std::{
    collections::HashSet,
    path::{Path, PathBuf},
};

use crate::{cli::OutputFormat, report::AppError};

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
        io::{Seek, SeekFrom, Write},
        os::unix::fs::MetadataExt,
        path::{Component, Path, PathBuf},
    };

    use getrandom::fill as random_fill;
    use rustix::{
        fs::{openat, renameat_with, statat, AtFlags, FileType, Mode, OFlags, RenameFlags, CWD},
        io::Errno,
    };

    use crate::report::AppError;

    use super::HashSet;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct FileId {
        device: u64,
        inode: u64,
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

        pub fn stage_all(&mut self, images: &[Vec<u8>]) -> Result<(), AppError> {
            if images.len() != self.entries.len() {
                return Err(AppError::output_commit(
                    "internal_image_count_mismatch",
                    "The validated image count did not match the reserved output count; no success was reported.",
                ));
            }
            for (entry, image) in self.entries.iter_mut().zip(images) {
                let Some(file) = entry.stage.file.as_mut() else {
                    return Err(AppError::output_commit(
                        "missing_staged_output",
                        "A staged output was unexpectedly unavailable. The image request may have been billed; no success was reported.",
                    ));
                };
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
            }
            Ok(())
        }

        pub fn commit_all(&mut self) -> Result<CommitResult, AppError> {
            for index in 0..self.entries.len() {
                if self.entries[index].committed {
                    continue;
                }
                let result = if self.overwrite {
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
                raw_entry_info(self.directory.directory(), &entry.final_name)
                    .ok()
                    .flatten()
                    .is_some_and(|info| {
                        info.id == entry.stage.id && info.file_type == FileType::RegularFile
                    })
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

    fn file_id(file: &File) -> std::io::Result<FileId> {
        let metadata = file.metadata()?;
        Ok(FileId {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn raw_entry_info(directory: &File, name: &str) -> rustix::io::Result<Option<EntryInfo>> {
        match statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(Some(EntryInfo {
                id: FileId {
                    device: stat.st_dev.try_into().map_err(|_| Errno::IO)?,
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
            fs::remove_file(&destination).unwrap();
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
            assert_eq!(fs::read(private_path).unwrap(), b"competitor");
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
pub use secure::{CommitResult, OutputTransaction};

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub struct OutputTransaction;

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

    pub fn stage_all(&mut self, _images: &[Vec<u8>]) -> Result<(), AppError> {
        unreachable!("unsupported platforms cannot reserve an output transaction")
    }

    pub fn commit_all(&mut self) -> Result<CommitResult, AppError> {
        unreachable!("unsupported platforms cannot reserve an output transaction")
    }

    pub fn abort(&mut self) -> Vec<PathBuf> {
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
