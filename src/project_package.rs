use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use zip::write::{FullFileOptions, SimpleFileOptions};
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::FlpDocument;
use crate::media::SamplePathResolver;

static WORKSPACE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Copies a sample to a directory under a collision-safe “(Unique)” filename.
/// Existing files are never overwritten.
pub fn copy_unique_sample_file(
    source: &Path,
    destination_directory: &Path,
) -> Result<PathBuf, String> {
    if !source.is_file() {
        return Err(format!("sample file does not exist: {}", source.display()));
    }
    fs::create_dir_all(destination_directory).map_err(|error| {
        format!(
            "could not create sample directory {}: {error}",
            destination_directory.display()
        )
    })?;

    let stem = source
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .filter(|stem| !stem.is_empty())
        .unwrap_or_else(|| "sample".to_owned());
    let extension = source
        .extension()
        .map(|extension| extension.to_string_lossy().into_owned())
        .filter(|extension| !extension.is_empty());
    let mut input = File::open(source)
        .map_err(|error| format!("could not open sample {}: {error}", source.display()))?;

    for suffix in 1usize.. {
        let unique_suffix = if suffix == 1 {
            " (Unique)".to_owned()
        } else {
            format!(" (Unique {suffix})")
        };
        let filename = match &extension {
            Some(extension) => format!("{stem}{unique_suffix}.{extension}"),
            None => format!("{stem}{unique_suffix}"),
        };
        let destination = destination_directory.join(filename);
        let mut output = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&destination)
        {
            Ok(output) => output,
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(format!(
                    "could not create unique sample {}: {error}",
                    destination.display()
                ));
            }
        };

        let copy_result = io::copy(&mut input, &mut output)
            .and_then(|_| output.flush())
            .and_then(|_| output.sync_all());
        if let Err(error) = copy_result {
            drop(output);
            let _ = fs::remove_file(&destination);
            return Err(format!(
                "could not copy sample {} to {}: {error}",
                source.display(),
                destination.display()
            ));
        }
        return Ok(destination);
    }

    unreachable!("a unique sample filename is always available")
}

struct WorkspaceInner {
    root: PathBuf,
    project_relative_path: PathBuf,
    sample_project_path: PathBuf,
    archive_comment: Option<Vec<u8>>,
    entry_metadata: HashMap<PathBuf, PackageEntryMetadata>,
}

#[derive(Clone)]
struct PackageEntryMetadata {
    options: SimpleFileOptions,
    comment: String,
    local_extra_fields: Vec<PackageExtraField>,
    central_extra_fields: Vec<PackageExtraField>,
    local_unicode_path_fields: Vec<PackageExtraField>,
    central_unicode_path_fields: Vec<PackageExtraField>,
    local_file_name_raw: Vec<u8>,
    local_uses_utf8_name: bool,
    central_file_name_raw: Vec<u8>,
    central_uses_utf8_name: bool,
}

#[derive(Clone, PartialEq, Eq)]
struct PackageExtraField {
    header_id: u16,
    data: Vec<u8>,
}

impl PackageEntryMetadata {
    fn full_options(&self) -> Result<FullFileOptions<'static>, String> {
        let mut options = self
            .options
            .into_full_options()
            .with_file_comment(self.comment.clone());
        for field in &self.local_extra_fields {
            options
                .add_extra_data(field.header_id, &field.data, false)
                .map_err(|error| {
                    format!(
                        "could not preserve local ZIP extra field {:#06x}: {error}",
                        field.header_id
                    )
                })?;
        }
        for field in &self.central_extra_fields {
            options
                .add_extra_data(field.header_id, &field.data, true)
                .map_err(|error| {
                    format!(
                        "could not preserve central ZIP extra field {:#06x}: {error}",
                        field.header_id
                    )
                })?;
        }
        Ok(options)
    }
}

struct WorkspaceDirectory(Option<PathBuf>);

impl WorkspaceDirectory {
    fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }

    fn path(&self) -> &Path {
        self.0.as_deref().expect("workspace directory is active")
    }

    fn keep(mut self) -> PathBuf {
        self.0.take().expect("workspace directory is active")
    }
}

impl Drop for WorkspaceDirectory {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = fs::remove_dir_all(path);
        }
    }
}

impl Drop for WorkspaceInner {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// A safely extracted FL Studio project package kept alive while the project is open.
#[derive(Clone)]
pub struct ProjectPackageWorkspace(Arc<WorkspaceInner>);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SampleBundleSummary {
    pub bundled_files: usize,
    pub unresolved_references: usize,
}

impl ProjectPackageWorkspace {
    /// Opens a standard ZIP package, extracts its files, and reads the first FLP.
    /// Root-level projects are preferred when an archive contains more than one.
    pub fn open(path: impl AsRef<Path>) -> Result<(Self, Vec<u8>), String> {
        let archive_comment = read_archive_comment(path.as_ref())?;
        let source = File::open(path.as_ref())
            .map_err(|error| format!("could not open {}: {error}", path.as_ref().display()))?;
        let mut archive = ZipArchive::new(source)
            .map_err(|error| format!("could not read ZIP project: {error}"))?;
        let mut source_headers = File::open(path.as_ref()).map_err(|error| {
            format!(
                "could not inspect ZIP project {}: {error}",
                path.as_ref().display()
            )
        })?;
        let workspace_directory = WorkspaceDirectory::new(create_workspace_directory()?);
        let root = workspace_directory.path();
        let mut project_files = Vec::new();
        let mut extracted_files = HashSet::new();
        let mut entry_metadata = HashMap::new();

        for index in 0..archive.len() {
            let mut entry = archive
                .by_index(index)
                .map_err(|error| format!("could not read ZIP entry {index}: {error}"))?;
            let raw_name = entry.name().to_owned();
            let relative_path = safe_archive_path(&raw_name)?;
            if relative_path.as_os_str().is_empty() {
                if entry.is_dir() {
                    continue;
                }
                return Err(format!("ZIP project contains an unsafe path {raw_name:?}"));
            }
            let target = root.join(&relative_path);
            let mut options = entry.options();
            if entry.is_dir() {
                options = options.unix_permissions(entry.unix_mode().unwrap_or(0o755) | 0o040000);
            }
            let local_extra_data = read_local_extra_data(&mut source_headers, entry.header_start())
                .map_err(|error| {
                    format!("could not inspect ZIP entry {raw_name:?} local extra fields: {error}")
                })?;
            let (local_file_name_raw, local_uses_utf8_name) =
                read_local_file_name(&mut source_headers, entry.header_start()).map_err(
                    |error| {
                        format!("could not inspect ZIP entry {raw_name:?} local filename: {error}")
                    },
                )?;
            let local_extra_fields = preserve_unknown_extra_fields(Some(&local_extra_data));
            let local_unicode_path_fields =
                preserve_extra_fields_with_id(&local_extra_data, 0x7075);
            let central_extra_data =
                read_central_extra_data(&mut source_headers, entry.central_header_start())
                    .map_err(|error| {
                        format!(
                            "could not inspect ZIP entry {raw_name:?} central extra fields: {error}"
                        )
                    })?;
            let (central_file_name_raw, central_uses_utf8_name) = read_central_file_name(
                &mut source_headers,
                entry.central_header_start(),
            )
            .map_err(|error| {
                format!("could not inspect ZIP entry {raw_name:?} central filename: {error}")
            })?;
            let central_unicode_path_fields =
                preserve_extra_fields_with_id(&central_extra_data, 0x7075);
            if contains_zip64_extra_field(&local_extra_data)
                || contains_zip64_extra_field(&central_extra_data)
            {
                options = options.large_file(true);
            }
            let metadata = PackageEntryMetadata {
                options,
                comment: entry.comment().to_owned(),
                local_extra_fields: local_extra_fields.clone(),
                central_extra_fields: remove_mirrored_local_extra_fields(
                    &local_extra_fields,
                    &preserve_unknown_extra_fields(Some(&central_extra_data)),
                ),
                local_unicode_path_fields,
                central_unicode_path_fields,
                local_file_name_raw,
                local_uses_utf8_name,
                central_file_name_raw,
                central_uses_utf8_name,
            };

            if entry.is_dir() {
                fs::create_dir_all(&target).map_err(|error| {
                    format!(
                        "could not create package directory {}: {error}",
                        target.display()
                    )
                })?;
                entry_metadata.insert(relative_path, metadata);
                continue;
            }

            // Do not materialize links or special files from the archive.
            if entry.unix_mode().is_some_and(|mode| {
                let kind = mode & 0o170000;
                kind != 0 && kind != 0o100000
            }) {
                continue;
            }
            if !extracted_files.insert(relative_path.clone()) {
                return Err(format!("ZIP project contains duplicate file {raw_name:?}"));
            }
            let parent = target.parent().ok_or_else(|| {
                format!("ZIP project entry has no parent directory: {raw_name:?}")
            })?;
            fs::create_dir_all(parent).map_err(|error| {
                format!(
                    "could not create package directory {}: {error}",
                    parent.display()
                )
            })?;
            let mut output = File::create(&target).map_err(|error| {
                format!(
                    "could not create package file {}: {error}",
                    target.display()
                )
            })?;
            io::copy(&mut entry, &mut output).map_err(|error| {
                format!(
                    "could not extract package file {}: {error}",
                    target.display()
                )
            })?;
            entry_metadata.insert(relative_path.clone(), metadata);

            if relative_path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("flp"))
            {
                project_files.push(relative_path);
            }
        }

        project_files.sort_by_key(|candidate| {
            (
                candidate.components().count() != 1,
                candidate.to_string_lossy().to_lowercase(),
            )
        });
        let project_relative_path = project_files
            .into_iter()
            .next()
            .ok_or_else(|| "ZIP project does not contain an FLP file".to_owned())?;
        let project_path = root.join(&project_relative_path);
        let project_bytes = fs::read(&project_path).map_err(|error| {
            format!(
                "could not read bundled project {}: {error}",
                project_path.display()
            )
        })?;
        let root = workspace_directory.keep();

        Ok((
            Self(Arc::new(WorkspaceInner {
                root,
                project_relative_path,
                sample_project_path: project_path,
                archive_comment,
                entry_metadata,
            })),
            project_bytes,
        ))
    }

    /// Creates a package workspace containing only a project file.
    /// The source path remains the sample lookup base for this open session.
    pub fn single_project(
        project_name: &str,
        project_bytes: &[u8],
        sample_project_path: impl AsRef<Path>,
    ) -> Result<Self, String> {
        let name = Path::new(project_name)
            .file_name()
            .filter(|name| {
                name.to_string_lossy()
                    .to_ascii_lowercase()
                    .ends_with(".flp")
            })
            .map(PathBuf::from)
            .ok_or_else(|| "ZIP package project name must end in .flp".to_owned())?;
        let workspace_directory = WorkspaceDirectory::new(create_workspace_directory()?);
        let root = workspace_directory.path();
        let project_path = root.join(&name);
        fs::write(&project_path, project_bytes).map_err(|error| {
            format!(
                "could not create package project {}: {error}",
                project_path.display()
            )
        })?;
        let root = workspace_directory.keep();
        Ok(Self(Arc::new(WorkspaceInner {
            root,
            project_relative_path: name,
            sample_project_path: sample_project_path.as_ref().to_path_buf(),
            archive_comment: None,
            entry_metadata: HashMap::new(),
        })))
    }

    pub fn project_path(&self) -> PathBuf {
        self.0.root.join(&self.0.project_relative_path)
    }

    pub fn sample_project_path(&self) -> &Path {
        &self.0.sample_project_path
    }

    /// Copies a sample next to the packaged FLP and returns its project-relative path.
    pub fn copy_unique_sample(&self, source: &Path) -> Result<(PathBuf, String), String> {
        let project_directory = self
            .0
            .project_relative_path
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let sample_directory = self.0.root.join(project_directory).join("Samples");
        let copied_path = copy_unique_sample_file(source, &sample_directory)?;
        let filename = copied_path
            .file_name()
            .ok_or_else(|| "copied sample has no filename".to_owned())?;
        let project_relative_path = Path::new("Samples").join(filename);
        Ok((copied_path, archive_name(&project_relative_path)))
    }

    /// Adds resolvable Sampler and audio-channel samples to the package and rewrites their
    /// project references to package-relative paths.
    pub fn bundle_channel_samples(
        &self,
        document: &mut FlpDocument,
        source_project_path: impl AsRef<Path>,
    ) -> Result<SampleBundleSummary, String> {
        let resolver = SamplePathResolver::new(source_project_path);
        let mut packaged_document = document.clone();
        let mut bundled_sources = HashMap::<PathBuf, PathBuf>::new();
        let mut reserved_paths = HashSet::<String>::new();
        let mut summary = SampleBundleSummary::default();

        for channel in packaged_document.channels() {
            if !matches!(channel.kind(), Some(0 | 4)) {
                continue;
            }
            let Some(sample_path) = channel.sample_path() else {
                continue;
            };
            let resolved = match resolver.resolve(sample_path) {
                Ok(path) if path.is_file() => path,
                _ => {
                    summary.unresolved_references += 1;
                    continue;
                }
            };
            let canonical_source = resolved.canonicalize().map_err(|error| {
                format!(
                    "could not resolve sample file {}: {error}",
                    resolved.display()
                )
            })?;
            let relative_path = if let Some(relative) = bundled_sources.get(&canonical_source) {
                relative.clone()
            } else {
                let preferred = sample_archive_path(sample_path);
                let relative = self.available_sample_archive_path(
                    preferred,
                    &canonical_source,
                    &reserved_paths,
                );
                self.add_file(&relative, &canonical_source)?;
                reserved_paths.insert(archive_name(&relative).to_ascii_lowercase());
                bundled_sources.insert(canonical_source, relative.clone());
                summary.bundled_files += 1;
                relative
            };
            packaged_document
                .set_channel_sample_path(channel.id(), &archive_name(&relative_path))
                .map_err(|error| {
                    format!(
                        "could not rewrite sample path for channel {}: {error}",
                        channel.id()
                    )
                })?;
        }

        *document = packaged_document;
        Ok(summary)
    }

    fn available_sample_archive_path(
        &self,
        preferred: PathBuf,
        source: &Path,
        reserved_paths: &HashSet<String>,
    ) -> PathBuf {
        let stem = preferred
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .filter(|stem| !stem.is_empty())
            .unwrap_or_else(|| "sample".to_owned());
        let extension = preferred
            .extension()
            .map(|extension| extension.to_string_lossy().into_owned());

        for suffix in 1usize.. {
            let filename = if suffix == 1 {
                preferred
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .filter(|name| !name.is_empty())
                    .unwrap_or_else(|| "sample".to_owned())
            } else if let Some(extension) = &extension {
                format!("{stem} ({suffix}).{extension}")
            } else {
                format!("{stem} ({suffix})")
            };
            let relative = Path::new("Samples").join(filename);
            let name = archive_name(&relative).to_ascii_lowercase();
            if reserved_paths.contains(&name) {
                continue;
            }
            if !self.path_exists(&relative) || self.is_same_file(&relative, source) {
                return relative;
            }
        }

        unreachable!("a unique sample archive path is always available")
    }

    fn path_exists(&self, relative_path: &Path) -> bool {
        validated_workspace_path(&self.0.root, relative_path).is_ok_and(|path| path.exists())
    }

    fn is_same_file(&self, relative_path: &Path, source: &Path) -> bool {
        let Ok(target) = validated_workspace_path(&self.0.root, relative_path) else {
            return false;
        };
        target.is_file() && target.canonicalize().ok().as_deref() == Some(source)
    }

    fn add_file(&self, relative_path: &Path, source: &Path) -> Result<(), String> {
        let target = validated_workspace_path(&self.0.root, relative_path)?;
        if target.is_file() && target.canonicalize().ok().as_deref() == Some(source) {
            return Ok(());
        }
        let parent = target
            .parent()
            .ok_or_else(|| "sample archive path has no parent directory".to_owned())?;
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "could not create package directory {}: {error}",
                parent.display()
            )
        })?;
        fs::copy(source, &target).map_err(|error| {
            format!(
                "could not add sample {} to package: {error}",
                source.display()
            )
        })?;
        Ok(())
    }

    /// Writes the updated FLP and all other extracted package files to a ZIP.
    pub fn write_to(&self, path: impl AsRef<Path>, project_bytes: &[u8]) -> Result<(), String> {
        let path = path.as_ref();
        let (temporary_path, file) = create_temporary_file(path)?;
        let result = self
            .write_archive(file, project_bytes)
            .and_then(|()| restore_raw_entry_names(&temporary_path, &self.0.entry_metadata));
        if let Err(error) = result {
            let _ = fs::remove_file(&temporary_path);
            return Err(error);
        }
        replace_file(&temporary_path, path).map_err(|error| {
            let _ = fs::remove_file(&temporary_path);
            format!("could not replace {}: {error}", path.display())
        })
    }

    fn write_archive(&self, file: File, project_bytes: &[u8]) -> Result<(), String> {
        let mut entries = Vec::new();
        collect_workspace_entries(&self.0.root, &self.0.root, &mut entries)?;
        entries.sort_by(|left, right| left.relative.cmp(&right.relative));
        if !entries
            .iter()
            .any(|entry| !entry.is_directory && entry.relative == self.0.project_relative_path)
        {
            return Err("the bundled FLP is missing from its package workspace".to_owned());
        }

        let mut archive = ZipWriter::new(file).set_auto_large_file();
        if let Some(comment) = &self.0.archive_comment {
            archive
                .set_raw_comment(comment.clone().into_boxed_slice())
                .map_err(|error| format!("could not preserve ZIP archive comment: {error}"))?;
        }
        for entry in entries {
            let name = archive_name(&entry.relative);
            let options = match self.0.entry_metadata.get(&entry.relative) {
                Some(metadata) => metadata.full_options()?,
                None => SimpleFileOptions::default()
                    .compression_method(CompressionMethod::Deflated)
                    .into_full_options(),
            };
            if entry.is_directory {
                archive
                    .add_directory(format!("{name}/"), options)
                    .map_err(|error| format!("could not add ZIP directory {name:?}: {error}"))?;
                continue;
            }
            archive
                .start_file(&name, options)
                .map_err(|error| format!("could not add ZIP file {name:?}: {error}"))?;
            if entry.relative == self.0.project_relative_path {
                archive
                    .write_all(project_bytes)
                    .map_err(|error| format!("could not write bundled FLP: {error}"))?;
            } else {
                let source = self.0.root.join(&entry.relative);
                let mut input = File::open(&source).map_err(|error| {
                    format!("could not read package file {}: {error}", source.display())
                })?;
                io::copy(&mut input, &mut archive).map_err(|error| {
                    format!("could not write package file {}: {error}", source.display())
                })?;
            }
        }
        let file = archive
            .finish()
            .map_err(|error| format!("could not finish ZIP project: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("could not flush ZIP project: {error}"))
    }
}

fn preserve_unknown_extra_fields(raw: Option<&[u8]>) -> Vec<PackageExtraField> {
    // These fields are interpreted by zip 8.x or depend on data rewritten by
    // the archive writer, so reusing their payloads can leave stale offsets,
    // checksums, or names in the archive. Unicode Path (0x7075) is restored
    // after the original raw filename bytes are written back.
    const ZIP_MANAGED_EXTRA_FIELD_IDS: &[u16] = &[
        0x0001, // ZIP64 extended information
        0x000a, // NTFS timestamps
        0x5455, // Extended timestamps
        0x6375, // Unicode comment
        0x7075, // Unicode path
        0x9901, // AES encryption
        0xa11e, // Data stream alignment
    ];
    let Some(raw) = raw else {
        return Vec::new();
    };
    let mut fields = Vec::new();
    let mut offset = 0;
    while offset + 4 <= raw.len() {
        let header_id = u16::from_le_bytes([raw[offset], raw[offset + 1]]);
        let data_length = u16::from_le_bytes([raw[offset + 2], raw[offset + 3]]) as usize;
        let data_start = offset + 4;
        let Some(data_end) = data_start.checked_add(data_length) else {
            break;
        };
        if data_end > raw.len() {
            break;
        }
        if !ZIP_MANAGED_EXTRA_FIELD_IDS.contains(&header_id) {
            fields.push(PackageExtraField {
                header_id,
                data: raw[data_start..data_end].to_vec(),
            });
        }
        offset = data_end;
    }
    fields
}

fn preserve_extra_fields_with_id(raw: &[u8], requested_id: u16) -> Vec<PackageExtraField> {
    let mut fields = Vec::new();
    let mut offset = 0;
    while offset + 4 <= raw.len() {
        let header_id = u16::from_le_bytes([raw[offset], raw[offset + 1]]);
        let data_length = usize::from(u16::from_le_bytes([raw[offset + 2], raw[offset + 3]]));
        let data_start = offset + 4;
        let Some(data_end) = data_start.checked_add(data_length) else {
            break;
        };
        if data_end > raw.len() {
            break;
        }
        if header_id == requested_id {
            fields.push(PackageExtraField {
                header_id,
                data: raw[data_start..data_end].to_vec(),
            });
        }
        offset = data_end;
    }
    fields
}

fn contains_zip64_extra_field(raw: &[u8]) -> bool {
    let mut offset = 0usize;
    while offset + 4 <= raw.len() {
        let header_id = u16::from_le_bytes([raw[offset], raw[offset + 1]]);
        let data_length = usize::from(u16::from_le_bytes([raw[offset + 2], raw[offset + 3]]));
        let Some(data_end) = offset
            .checked_add(4)
            .and_then(|start| start.checked_add(data_length))
        else {
            return false;
        };
        if data_end > raw.len() {
            return false;
        }
        if header_id == 0x0001 {
            return true;
        }
        offset = data_end;
    }
    false
}

fn remove_mirrored_local_extra_fields(
    local_fields: &[PackageExtraField],
    central_fields: &[PackageExtraField],
) -> Vec<PackageExtraField> {
    if central_fields.starts_with(local_fields) {
        central_fields[local_fields.len()..].to_vec()
    } else {
        central_fields.to_vec()
    }
}

fn read_local_file_name(file: &mut File, header_start: u64) -> io::Result<(Vec<u8>, bool)> {
    const HEADER_LENGTH: usize = 30;
    let mut fixed_header = [0_u8; HEADER_LENGTH];
    file.seek(io::SeekFrom::Start(header_start))?;
    file.read_exact(&mut fixed_header)?;
    if &fixed_header[..4] != b"PK\x03\x04" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid local file header signature",
        ));
    }
    let flags = u16::from_le_bytes([fixed_header[6], fixed_header[7]]);
    let name_length = u16::from_le_bytes([fixed_header[26], fixed_header[27]]) as usize;
    let mut name = vec![0; name_length];
    file.read_exact(&mut name)?;
    Ok((name, flags & 0x0800 != 0))
}

fn read_central_file_name(file: &mut File, header_start: u64) -> io::Result<(Vec<u8>, bool)> {
    const HEADER_LENGTH: usize = 46;
    let mut fixed_header = [0_u8; HEADER_LENGTH];
    file.seek(io::SeekFrom::Start(header_start))?;
    file.read_exact(&mut fixed_header)?;
    if &fixed_header[..4] != b"PK\x01\x02" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid central directory file header signature",
        ));
    }
    let flags = u16::from_le_bytes([fixed_header[8], fixed_header[9]]);
    let name_length = u16::from_le_bytes([fixed_header[28], fixed_header[29]]) as usize;
    let mut name = vec![0; name_length];
    file.read_exact(&mut name)?;
    Ok((name, flags & 0x0800 != 0))
}

fn restore_raw_entry_names(
    path: &Path,
    metadata: &HashMap<PathBuf, PackageEntryMetadata>,
) -> Result<(), String> {
    let mut input = File::open(path).map_err(|error| {
        format!(
            "could not inspect rewritten ZIP {}: {error}",
            path.display()
        )
    })?;
    let mut entries = {
        let mut archive = ZipArchive::new(input.try_clone().map_err(|error| {
            format!(
                "could not inspect rewritten ZIP {}: {error}",
                path.display()
            )
        })?)
        .map_err(|error| {
            format!(
                "could not inspect rewritten ZIP {}: {error}",
                path.display()
            )
        })?;
        let mut entries = Vec::with_capacity(archive.len());
        for index in 0..archive.len() {
            let entry = archive.by_index(index).map_err(|error| {
                format!("could not inspect rewritten ZIP entry {index}: {error}")
            })?;
            let relative = safe_archive_path(entry.name())?;
            let local_header_start = entry.header_start();
            let central_header_start = entry.central_header_start();
            let (current_local_name, current_local_utf8) =
                read_local_file_name(&mut input, local_header_start).map_err(|error| {
                    format!("could not inspect rewritten ZIP local filename: {error}")
                })?;
            let (current_central_name, current_central_utf8) =
                read_central_file_name(&mut input, central_header_start).map_err(|error| {
                    format!("could not inspect rewritten ZIP central filename: {error}")
                })?;
            let entry_metadata = metadata.get(&relative);
            entries.push(ArchiveRewriteEntry {
                relative,
                local_header_start,
                central_header_start,
                local_unicode_path_fields: entry_metadata
                    .map(|metadata| metadata.local_unicode_path_fields.clone())
                    .unwrap_or_default(),
                central_unicode_path_fields: entry_metadata
                    .map(|metadata| metadata.central_unicode_path_fields.clone())
                    .unwrap_or_default(),
                local_file_name_raw: entry_metadata
                    .map(|metadata| metadata.local_file_name_raw.clone())
                    .unwrap_or(current_local_name),
                local_uses_utf8_name: entry_metadata
                    .map(|metadata| metadata.local_uses_utf8_name)
                    .unwrap_or(current_local_utf8),
                central_file_name_raw: entry_metadata
                    .map(|metadata| metadata.central_file_name_raw.clone())
                    .unwrap_or(current_central_name),
                central_uses_utf8_name: entry_metadata
                    .map(|metadata| metadata.central_uses_utf8_name)
                    .unwrap_or(current_central_utf8),
            });
        }
        entries
    };

    let mut needs_rewrite = false;
    for entry in &entries {
        let (current_local_name, current_local_utf8) =
            read_local_file_name(&mut input, entry.local_header_start).map_err(|error| {
                format!("could not inspect rewritten ZIP local filename: {error}")
            })?;
        let (current_central_name, current_central_utf8) =
            read_central_file_name(&mut input, entry.central_header_start).map_err(|error| {
                format!("could not inspect rewritten ZIP central filename: {error}")
            })?;
        needs_rewrite |= current_local_name != entry.local_file_name_raw
            || current_local_utf8 != entry.local_uses_utf8_name
            || current_central_name != entry.central_file_name_raw
            || current_central_utf8 != entry.central_uses_utf8_name
            || !entry.local_unicode_path_fields.is_empty()
            || !entry.central_unicode_path_fields.is_empty();
    }
    if !needs_rewrite {
        return Ok(());
    }

    entries.sort_by_key(|entry| entry.local_header_start);
    if entries.is_empty() {
        return Err("rewritten ZIP unexpectedly contains no entries".to_owned());
    }
    let central_directory_start = entries
        .iter()
        .map(|entry| entry.central_header_start)
        .min()
        .expect("nonempty archive entries have central headers");
    let file_length = input
        .metadata()
        .map_err(|error| {
            format!(
                "could not inspect rewritten ZIP {}: {error}",
                path.display()
            )
        })?
        .len();
    let (end_record_offset, zip64_end_record_offset) =
        locate_archive_end_records(&mut input, file_length)
            .map_err(|error| format!("could not locate rewritten ZIP end records: {error}"))?;
    let central_directory_end = zip64_end_record_offset.unwrap_or(end_record_offset);
    if central_directory_start > central_directory_end || central_directory_end > file_length {
        return Err("rewritten ZIP has invalid central directory offsets".to_owned());
    }

    let (repacked_path, mut output) = create_temporary_file(path)?;
    let rewrite_result = (|| -> Result<(), String> {
        let mut local_header_offsets = HashMap::with_capacity(entries.len());
        let mut input_cursor = 0_u64;
        for index in 0..entries.len() {
            let entry = &entries[index];
            let local_end = entries
                .get(index + 1)
                .map(|next| next.local_header_start)
                .unwrap_or(central_directory_start);
            if entry.local_header_start < input_cursor || local_end < entry.local_header_start {
                return Err("rewritten ZIP local headers overlap".to_owned());
            }
            copy_archive_range(
                &mut input,
                &mut output,
                input_cursor,
                entry.local_header_start,
            )
            .map_err(|error| format!("could not copy rewritten ZIP data: {error}"))?;

            let mut fixed_header = [0_u8; 30];
            input
                .seek(io::SeekFrom::Start(entry.local_header_start))
                .and_then(|_| input.read_exact(&mut fixed_header))
                .map_err(|error| format!("could not read rewritten ZIP local header: {error}"))?;
            if &fixed_header[..4] != b"PK\x03\x04" {
                return Err("rewritten ZIP has an invalid local file header".to_owned());
            }
            let old_name_length =
                usize::from(u16::from_le_bytes([fixed_header[26], fixed_header[27]]));
            let old_name_end = entry
                .local_header_start
                .checked_add(30 + old_name_length as u64)
                .ok_or_else(|| "rewritten ZIP local header offset overflowed".to_owned())?;
            if old_name_end > local_end {
                return Err("rewritten ZIP local filename exceeds its entry".to_owned());
            }
            let old_extra_length =
                usize::from(u16::from_le_bytes([fixed_header[28], fixed_header[29]]));
            let old_extra_end = old_name_end
                .checked_add(old_extra_length as u64)
                .ok_or_else(|| "rewritten ZIP local extra offset overflowed".to_owned())?;
            if old_extra_end > local_end {
                return Err("rewritten ZIP local extra fields exceed their entry".to_owned());
            }
            let mut extra = vec![0; old_extra_length];
            input
                .seek(io::SeekFrom::Start(old_name_end))
                .and_then(|_| input.read_exact(&mut extra))
                .map_err(|error| {
                    format!("could not read rewritten ZIP local extra fields: {error}")
                })?;
            append_extra_fields(&mut extra, &entry.local_unicode_path_fields)
                .map_err(|error| format!("could not restore Unicode ZIP filename: {error}"))?;
            let new_name_length = u16::try_from(entry.local_file_name_raw.len())
                .map_err(|_| "original ZIP local filename is too long".to_owned())?;
            let new_extra_length = u16::try_from(extra.len())
                .map_err(|_| "restored ZIP local extra fields are too long".to_owned())?;
            let mut flags = u16::from_le_bytes([fixed_header[6], fixed_header[7]]);
            flags = (flags & !0x0800)
                | if entry.local_uses_utf8_name {
                    0x0800
                } else {
                    0
                };
            fixed_header[6..8].copy_from_slice(&flags.to_le_bytes());
            fixed_header[26..28].copy_from_slice(&new_name_length.to_le_bytes());
            fixed_header[28..30].copy_from_slice(&new_extra_length.to_le_bytes());

            let new_header_start = output
                .stream_position()
                .map_err(|error| format!("could not write rewritten ZIP: {error}"))?;
            local_header_offsets.insert(entry.relative.clone(), new_header_start);
            output
                .write_all(&fixed_header)
                .and_then(|_| output.write_all(&entry.local_file_name_raw))
                .and_then(|_| output.write_all(&extra))
                .map_err(|error| format!("could not write rewritten ZIP local header: {error}"))?;
            copy_archive_range(&mut input, &mut output, old_extra_end, local_end)
                .map_err(|error| format!("could not copy rewritten ZIP data: {error}"))?;
            input_cursor = local_end;
        }
        copy_archive_range(
            &mut input,
            &mut output,
            input_cursor,
            central_directory_start,
        )
        .map_err(|error| format!("could not copy rewritten ZIP data: {error}"))?;

        let new_central_directory_start = output
            .stream_position()
            .map_err(|error| format!("could not write rewritten ZIP: {error}"))?;
        let mut central_entries = entries.iter().collect::<Vec<_>>();
        central_entries.sort_by_key(|entry| entry.central_header_start);
        let mut central_cursor = central_directory_start;
        for entry in central_entries {
            if entry.central_header_start < central_cursor {
                return Err("rewritten ZIP central directory headers overlap".to_owned());
            }
            copy_archive_range(
                &mut input,
                &mut output,
                central_cursor,
                entry.central_header_start,
            )
            .map_err(|error| format!("could not copy rewritten ZIP data: {error}"))?;

            let mut fixed_header = [0_u8; 46];
            input
                .seek(io::SeekFrom::Start(entry.central_header_start))
                .and_then(|_| input.read_exact(&mut fixed_header))
                .map_err(|error| format!("could not read rewritten ZIP central header: {error}"))?;
            if &fixed_header[..4] != b"PK\x01\x02" {
                return Err("rewritten ZIP has an invalid central directory header".to_owned());
            }
            let old_name_length =
                usize::from(u16::from_le_bytes([fixed_header[28], fixed_header[29]]));
            let extra_length =
                usize::from(u16::from_le_bytes([fixed_header[30], fixed_header[31]]));
            let comment_length =
                usize::from(u16::from_le_bytes([fixed_header[32], fixed_header[33]]));
            let old_variable_start = entry
                .central_header_start
                .checked_add(46 + old_name_length as u64)
                .ok_or_else(|| "rewritten ZIP central header offset overflowed".to_owned())?;
            let comment_start = old_variable_start
                .checked_add(extra_length as u64)
                .ok_or_else(|| "rewritten ZIP central header offset overflowed".to_owned())?;
            let old_header_end = comment_start
                .checked_add(comment_length as u64)
                .ok_or_else(|| "rewritten ZIP central header offset overflowed".to_owned())?;
            if old_header_end > central_directory_end {
                return Err("rewritten ZIP central header exceeds its directory".to_owned());
            }
            let mut extra = vec![0; extra_length];
            input
                .seek(io::SeekFrom::Start(old_variable_start))
                .and_then(|_| input.read_exact(&mut extra))
                .map_err(|error| format!("could not read rewritten ZIP extra fields: {error}"))?;
            append_extra_fields(&mut extra, &entry.central_unicode_path_fields)
                .map_err(|error| format!("could not restore Unicode ZIP filename: {error}"))?;
            let new_extra_length = u16::try_from(extra.len())
                .map_err(|_| "restored ZIP central extra fields are too long".to_owned())?;
            let new_name_length = u16::try_from(entry.central_file_name_raw.len())
                .map_err(|_| "original ZIP central filename is too long".to_owned())?;
            let mut flags = u16::from_le_bytes([fixed_header[8], fixed_header[9]]);
            flags = (flags & !0x0800)
                | if entry.central_uses_utf8_name {
                    0x0800
                } else {
                    0
                };
            fixed_header[8..10].copy_from_slice(&flags.to_le_bytes());
            fixed_header[28..30].copy_from_slice(&new_name_length.to_le_bytes());
            fixed_header[30..32].copy_from_slice(&new_extra_length.to_le_bytes());
            let new_local_header_start = *local_header_offsets
                .get(&entry.relative)
                .ok_or_else(|| "rewritten ZIP entry has no local header".to_owned())?;
            patch_central_local_header_offset(
                &mut fixed_header,
                &mut extra,
                new_local_header_start,
            )
            .map_err(|error| format!("could not update rewritten ZIP entry offset: {error}"))?;
            output
                .write_all(&fixed_header)
                .and_then(|_| output.write_all(&entry.central_file_name_raw))
                .and_then(|_| output.write_all(&extra))
                .map_err(|error| {
                    format!("could not write rewritten ZIP central header: {error}")
                })?;
            copy_archive_range(&mut input, &mut output, comment_start, old_header_end)
                .map_err(|error| format!("could not copy rewritten ZIP data: {error}"))?;
            central_cursor = old_header_end;
        }
        copy_archive_range(
            &mut input,
            &mut output,
            central_cursor,
            central_directory_end,
        )
        .map_err(|error| format!("could not copy rewritten ZIP data: {error}"))?;

        let new_central_directory_end = output
            .stream_position()
            .map_err(|error| format!("could not write rewritten ZIP: {error}"))?;
        let new_central_directory_size = new_central_directory_end
            .checked_sub(new_central_directory_start)
            .ok_or_else(|| "rewritten ZIP central directory size underflowed".to_owned())?;
        let mut trailing_records = read_archive_range(
            &mut input,
            central_directory_end,
            file_length,
            2 * u64::from(u16::MAX) + 4096,
        )
        .map_err(|error| format!("could not read rewritten ZIP end records: {error}"))?;
        patch_archive_end_records(
            &mut trailing_records,
            zip64_end_record_offset.is_some(),
            new_central_directory_start,
            new_central_directory_size,
            new_central_directory_end,
        )
        .map_err(|error| format!("could not update rewritten ZIP end records: {error}"))?;
        output
            .write_all(&trailing_records)
            .and_then(|_| output.sync_all())
            .map_err(|error| format!("could not flush rewritten ZIP: {error}"))?;
        Ok(())
    })();
    drop(output);
    drop(input);
    if let Err(error) = rewrite_result {
        let _ = fs::remove_file(&repacked_path);
        return Err(error);
    }
    replace_file(&repacked_path, path)
        .map_err(|error| format!("could not preserve ZIP entry filename bytes: {error}"))
}

fn locate_archive_end_records(file: &mut File, file_length: u64) -> io::Result<(u64, Option<u64>)> {
    const EOCD_FIXED_LENGTH: usize = 22;
    const MAX_COMMENT_LENGTH: usize = u16::MAX as usize;
    let tail_length = file_length.min((EOCD_FIXED_LENGTH + MAX_COMMENT_LENGTH) as u64) as usize;
    if tail_length < EOCD_FIXED_LENGTH {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ZIP is too short to contain an end record",
        ));
    }
    file.seek(io::SeekFrom::End(-(tail_length as i64)))?;
    let mut tail = vec![0; tail_length];
    file.read_exact(&mut tail)?;
    let eocd_in_tail = (0..=tail.len() - EOCD_FIXED_LENGTH)
        .rev()
        .find(|&offset| {
            &tail[offset..offset + 4] == b"PK\x05\x06"
                && offset
                    + EOCD_FIXED_LENGTH
                    + usize::from(u16::from_le_bytes([tail[offset + 20], tail[offset + 21]]))
                    == tail.len()
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "ZIP end record not found"))?;
    let eocd_offset = file_length - tail_length as u64 + eocd_in_tail as u64;
    let mut zip64_locator = [0_u8; 20];
    let zip64_end_record_offset = if eocd_offset >= zip64_locator.len() as u64 {
        file.seek(io::SeekFrom::Start(
            eocd_offset - zip64_locator.len() as u64,
        ))?;
        file.read_exact(&mut zip64_locator)?;
        (&zip64_locator[..4] == b"PK\x06\x07").then(|| {
            u64::from_le_bytes([
                zip64_locator[8],
                zip64_locator[9],
                zip64_locator[10],
                zip64_locator[11],
                zip64_locator[12],
                zip64_locator[13],
                zip64_locator[14],
                zip64_locator[15],
            ])
        })
    } else {
        None
    };
    Ok((eocd_offset, zip64_end_record_offset))
}

fn copy_archive_range(input: &mut File, output: &mut File, start: u64, end: u64) -> io::Result<()> {
    if end < start {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ZIP byte range is reversed",
        ));
    }
    input.seek(io::SeekFrom::Start(start))?;
    let length = end - start;
    let mut limited_input = (&mut *input).take(length);
    let copied = io::copy(&mut limited_input, output)?;
    if copied != length {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "ZIP byte range ended early",
        ));
    }
    Ok(())
}

fn read_archive_range(
    input: &mut File,
    start: u64,
    end: u64,
    max_length: u64,
) -> io::Result<Vec<u8>> {
    let length = end
        .checked_sub(start)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "ZIP byte range is reversed"))?;
    if length > max_length {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ZIP end records exceed the supported size",
        ));
    }
    let length = usize::try_from(length)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "ZIP end records are too large"))?;
    let mut bytes = vec![0; length];
    input.seek(io::SeekFrom::Start(start))?;
    input.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn append_extra_fields(extra: &mut Vec<u8>, fields: &[PackageExtraField]) -> io::Result<()> {
    for field in fields {
        let length = u16::try_from(field.data.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "ZIP extra field is too long")
        })?;
        extra.extend_from_slice(&field.header_id.to_le_bytes());
        extra.extend_from_slice(&length.to_le_bytes());
        extra.extend_from_slice(&field.data);
    }
    if extra.len() > u16::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "combined ZIP extra fields are too long",
        ));
    }
    Ok(())
}

fn patch_central_local_header_offset(
    fixed_header: &mut [u8; 46],
    extra: &mut [u8],
    local_header_offset: u64,
) -> io::Result<()> {
    let stored_offset = u32::from_le_bytes([
        fixed_header[42],
        fixed_header[43],
        fixed_header[44],
        fixed_header[45],
    ]);
    if stored_offset != u32::MAX {
        let offset = u32::try_from(local_header_offset).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "rewritten ZIP offset requires an existing ZIP64 offset field",
            )
        })?;
        fixed_header[42..46].copy_from_slice(&offset.to_le_bytes());
        return Ok(());
    }

    let uncompressed_size = u32::from_le_bytes([
        fixed_header[24],
        fixed_header[25],
        fixed_header[26],
        fixed_header[27],
    ]);
    let compressed_size = u32::from_le_bytes([
        fixed_header[20],
        fixed_header[21],
        fixed_header[22],
        fixed_header[23],
    ]);
    let mut cursor = 0_usize;
    while cursor + 4 <= extra.len() {
        let id = u16::from_le_bytes([extra[cursor], extra[cursor + 1]]);
        let length = usize::from(u16::from_le_bytes([extra[cursor + 2], extra[cursor + 3]]));
        let data_start = cursor + 4;
        let data_end = data_start.checked_add(length).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "ZIP64 extra field length overflowed",
            )
        })?;
        if data_end > extra.len() {
            break;
        }
        if id == 0x0001 {
            let mut offset = data_start;
            if uncompressed_size == u32::MAX {
                offset = offset.checked_add(8).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "ZIP64 field offset overflowed")
                })?;
            }
            if compressed_size == u32::MAX {
                offset = offset.checked_add(8).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "ZIP64 field offset overflowed")
                })?;
            }
            let value_end = offset.checked_add(8).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "ZIP64 field offset overflowed")
            })?;
            if value_end > data_end {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "ZIP64 extra field omits the local header offset",
                ));
            }
            extra[offset..value_end].copy_from_slice(&local_header_offset.to_le_bytes());
            return Ok(());
        }
        cursor = data_end;
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "ZIP64 local header offset is missing its extra field",
    ))
}

fn patch_archive_end_records(
    trailing_records: &mut [u8],
    has_zip64: bool,
    central_directory_start: u64,
    central_directory_size: u64,
    zip64_end_record_offset: u64,
) -> io::Result<()> {
    const EOCD_LENGTH: usize = 22;
    let eocd_offset = (0..=trailing_records.len().saturating_sub(EOCD_LENGTH))
        .rev()
        .find(|&offset| {
            offset + EOCD_LENGTH <= trailing_records.len()
                && &trailing_records[offset..offset + 4] == b"PK\x05\x06"
                && offset
                    + EOCD_LENGTH
                    + usize::from(u16::from_le_bytes([
                        trailing_records[offset + 20],
                        trailing_records[offset + 21],
                    ]))
                    == trailing_records.len()
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "ZIP end record not found"))?;

    if has_zip64 {
        if trailing_records.len() < 56 || &trailing_records[..4] != b"PK\x06\x06" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ZIP64 end record is missing",
            ));
        }
        let zip64_central_size = central_directory_size.to_le_bytes();
        let zip64_central_offset = central_directory_start.to_le_bytes();
        trailing_records[40..48].copy_from_slice(&zip64_central_size);
        trailing_records[48..56].copy_from_slice(&zip64_central_offset);

        let locator_offset = eocd_offset.checked_sub(20).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "ZIP64 locator is missing")
        })?;
        if &trailing_records[locator_offset..locator_offset + 4] != b"PK\x06\x07" {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ZIP64 locator is missing",
            ));
        }
        trailing_records[locator_offset + 8..locator_offset + 16]
            .copy_from_slice(&zip64_end_record_offset.to_le_bytes());
    }

    let classic_size = u32::from_le_bytes([
        trailing_records[eocd_offset + 12],
        trailing_records[eocd_offset + 13],
        trailing_records[eocd_offset + 14],
        trailing_records[eocd_offset + 15],
    ]);
    let classic_offset = u32::from_le_bytes([
        trailing_records[eocd_offset + 16],
        trailing_records[eocd_offset + 17],
        trailing_records[eocd_offset + 18],
        trailing_records[eocd_offset + 19],
    ]);
    let updated_size = if central_directory_size <= u64::from(u32::MAX) {
        central_directory_size as u32
    } else if has_zip64 {
        u32::MAX
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "rewritten ZIP central directory requires ZIP64",
        ));
    };
    let updated_offset = if central_directory_start <= u64::from(u32::MAX) {
        central_directory_start as u32
    } else if has_zip64 {
        u32::MAX
    } else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "rewritten ZIP central directory offset requires ZIP64",
        ));
    };
    let updated_size = if classic_size == u32::MAX {
        u32::MAX
    } else {
        updated_size
    };
    let updated_offset = if classic_offset == u32::MAX {
        u32::MAX
    } else {
        updated_offset
    };
    trailing_records[eocd_offset + 12..eocd_offset + 16]
        .copy_from_slice(&updated_size.to_le_bytes());
    trailing_records[eocd_offset + 16..eocd_offset + 20]
        .copy_from_slice(&updated_offset.to_le_bytes());
    Ok(())
}

fn read_central_extra_data(file: &mut File, header_start: u64) -> io::Result<Vec<u8>> {
    const CENTRAL_FILE_HEADER_LENGTH: usize = 46;
    let mut fixed_header = [0_u8; CENTRAL_FILE_HEADER_LENGTH];
    file.seek(io::SeekFrom::Start(header_start))?;
    file.read_exact(&mut fixed_header)?;
    if &fixed_header[..4] != b"PK\x01\x02" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid central directory file header signature",
        ));
    }
    let name_length = u16::from_le_bytes([fixed_header[28], fixed_header[29]]) as usize;
    let extra_length = u16::from_le_bytes([fixed_header[30], fixed_header[31]]) as usize;
    file.seek(io::SeekFrom::Current(name_length as i64))?;
    let mut extra_data = vec![0; extra_length];
    file.read_exact(&mut extra_data)?;
    Ok(extra_data)
}

fn read_local_extra_data(file: &mut File, header_start: u64) -> io::Result<Vec<u8>> {
    const LOCAL_FILE_HEADER_LENGTH: usize = 30;
    let mut fixed_header = [0_u8; LOCAL_FILE_HEADER_LENGTH];
    file.seek(io::SeekFrom::Start(header_start))?;
    file.read_exact(&mut fixed_header)?;
    if &fixed_header[..4] != b"PK\x03\x04" {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid local file header signature",
        ));
    }
    let name_length = u16::from_le_bytes([fixed_header[26], fixed_header[27]]) as usize;
    let extra_length = u16::from_le_bytes([fixed_header[28], fixed_header[29]]) as usize;
    file.seek(io::SeekFrom::Current(name_length as i64))?;
    let mut extra_data = vec![0; extra_length];
    file.read_exact(&mut extra_data)?;
    Ok(extra_data)
}

fn read_archive_comment(path: &Path) -> Result<Option<Vec<u8>>, String> {
    const END_OF_CENTRAL_DIRECTORY: &[u8; 4] = b"PK\x05\x06";
    const END_RECORD_LENGTH: usize = 22;
    const MAX_COMMENT_LENGTH: usize = u16::MAX as usize;
    let mut file = File::open(path)
        .map_err(|error| format!("could not inspect ZIP project {}: {error}", path.display()))?;
    let file_length = file
        .metadata()
        .map_err(|error| format!("could not inspect ZIP project {}: {error}", path.display()))?
        .len();
    let tail_length = file_length.min((END_RECORD_LENGTH + MAX_COMMENT_LENGTH) as u64) as usize;
    if tail_length < END_RECORD_LENGTH {
        return Ok(None);
    }
    file.seek(io::SeekFrom::End(-(tail_length as i64)))
        .map_err(|error| format!("could not inspect ZIP project {}: {error}", path.display()))?;
    let mut tail = Vec::with_capacity(tail_length);
    file.read_to_end(&mut tail)
        .map_err(|error| format!("could not inspect ZIP project {}: {error}", path.display()))?;

    for offset in (0..=tail.len() - END_RECORD_LENGTH).rev() {
        if &tail[offset..offset + END_OF_CENTRAL_DIRECTORY.len()] != END_OF_CENTRAL_DIRECTORY {
            continue;
        }
        let comment_length = u16::from_le_bytes([tail[offset + 20], tail[offset + 21]]) as usize;
        let comment_start = offset + END_RECORD_LENGTH;
        if comment_start + comment_length == tail.len() {
            return Ok(Some(tail[comment_start..].to_vec()));
        }
    }
    Ok(None)
}

struct WorkspaceEntry {
    relative: PathBuf,
    is_directory: bool,
}

struct ArchiveRewriteEntry {
    relative: PathBuf,
    local_header_start: u64,
    central_header_start: u64,
    local_unicode_path_fields: Vec<PackageExtraField>,
    central_unicode_path_fields: Vec<PackageExtraField>,
    local_file_name_raw: Vec<u8>,
    local_uses_utf8_name: bool,
    central_file_name_raw: Vec<u8>,
    central_uses_utf8_name: bool,
}

fn sample_archive_path(sample_path: &str) -> PathBuf {
    let filename = sample_path
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .filter(|part| !matches!(*part, "." | ".."))
        .map(|part| {
            part.chars()
                .map(|character| {
                    if matches!(character, ':' | '\0') {
                        '_'
                    } else {
                        character
                    }
                })
                .collect::<String>()
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "sample".to_owned());
    Path::new("Samples").join(filename)
}

fn validated_workspace_path(root: &Path, relative_path: &Path) -> Result<PathBuf, String> {
    let normalized = safe_archive_path(&archive_name(relative_path))?;
    if normalized.as_os_str().is_empty() {
        return Err("package workspace path cannot be empty".to_owned());
    }
    Ok(root.join(normalized))
}

fn collect_workspace_entries(
    root: &Path,
    directory: &Path,
    entries: &mut Vec<WorkspaceEntry>,
) -> Result<(), String> {
    let mut children = fs::read_dir(directory)
        .map_err(|error| {
            format!(
                "could not list package directory {}: {error}",
                directory.display()
            )
        })?
        .map(|entry| entry.map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            format!(
                "could not list package directory {}: {error}",
                directory.display()
            )
        })?;
    children.sort_by_key(|entry| entry.file_name());

    for child in children {
        let kind = child
            .file_type()
            .map_err(|error| format!("could not inspect package entry: {error}"))?;
        let path = child.path();
        if kind.is_symlink() {
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|_| "package entry escaped its workspace".to_owned())?
            .to_path_buf();
        if kind.is_dir() {
            entries.push(WorkspaceEntry {
                relative: relative.clone(),
                is_directory: true,
            });
            collect_workspace_entries(root, &path, entries)?;
        } else if kind.is_file() {
            entries.push(WorkspaceEntry {
                relative,
                is_directory: false,
            });
        }
    }
    Ok(())
}

fn safe_archive_path(name: &str) -> Result<PathBuf, String> {
    if name.is_empty()
        || name.starts_with('/')
        || name.contains('\\')
        || name.contains(':')
        || name.contains('\0')
    {
        return Err(format!("ZIP project contains an unsafe path {name:?}"));
    }
    let path = Path::new(name);
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => normalized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("ZIP project contains an unsafe path {name:?}"));
            }
        }
    }
    Ok(normalized)
}

fn archive_name(path: &Path) -> String {
    path.components()
        .map(|component| component.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn create_workspace_directory() -> Result<PathBuf, String> {
    let parent = std::env::temp_dir().join("flp-rebuild-project-packages");
    fs::create_dir_all(&parent)
        .map_err(|error| format!("could not create project workspace: {error}"))?;
    for _ in 0..16 {
        let sequence = WORKSPACE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path = parent.join(format!("{}-{timestamp}-{sequence}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("could not create project workspace: {error}")),
        }
    }
    Err("could not allocate a unique project workspace".to_owned())
}

fn create_temporary_file(target: &Path) -> Result<(PathBuf, File), String> {
    let parent = target
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)
        .map_err(|error| format!("could not create {}: {error}", parent.display()))?;
    let file_name = target
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "project.zip".into());
    for _ in 0..16 {
        let sequence = WORKSPACE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{file_name}.tmp-{}-{sequence}",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
        {
            Ok(file) => return Ok((temporary, file)),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("could not create temporary ZIP: {error}")),
        }
    }
    Err("could not allocate a temporary ZIP file".to_owned())
}

fn replace_file(temporary: &Path, target: &Path) -> io::Result<()> {
    #[cfg(not(windows))]
    {
        fs::rename(temporary, target)
    }
    #[cfg(windows)]
    {
        if !target.exists() {
            return fs::rename(temporary, target);
        }
        let sequence = WORKSPACE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let backup =
            target.with_extension(format!("zip-replace-{}-{sequence}", std::process::id()));
        fs::rename(target, &backup)?;
        match fs::rename(temporary, target) {
            Ok(()) => {
                let _ = fs::remove_file(backup);
                Ok(())
            }
            Err(error) => {
                let _ = fs::rename(backup, target);
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ProjectPackageWorkspace, contains_zip64_extra_field, preserve_extra_fields_with_id,
        read_archive_comment, read_central_extra_data, read_central_file_name,
        read_local_extra_data, read_local_file_name,
    };
    use crate::FlpDocument;
    use crate::media::SamplePathResolver;
    use std::fs::{self, OpenOptions};
    use std::io::{Read, Seek, SeekFrom, Write};
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;
    use zip::write::{FullFileOptions, SimpleFileOptions};
    use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "flp-package-test-{}-{}",
                std::process::id(),
                super::WORKSPACE_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("test directory should be created");
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn sample_project_fixture(paths: &[&str]) -> Vec<u8> {
        let mut events = Vec::new();
        for (index, path) in paths.iter().enumerate() {
            events.extend_from_slice(&[0x40, (index + 1) as u8, 0, 0x15, 0, 0xC4]);
            let payload = path
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>();
            events.extend_from_slice(&crate::encode_leb128(payload.len() as u32));
            events.extend_from_slice(&payload);
        }
        events.extend_from_slice(&[0x62, 0, 0]);

        let mut project = Vec::new();
        project.extend_from_slice(b"FLhd");
        project.extend_from_slice(&6u32.to_le_bytes());
        project.extend_from_slice(&0u16.to_le_bytes());
        project.extend_from_slice(&0u16.to_le_bytes());
        project.extend_from_slice(&96u16.to_le_bytes());
        project.extend_from_slice(b"FLdt");
        project.extend_from_slice(&(events.len() as u32).to_le_bytes());
        project.extend_from_slice(&events);
        project
    }

    fn fixture_crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0_u32;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                let mask = 0_u32.wrapping_sub(crc & 1);
                crc = (crc >> 1) ^ (0xedb8_8320 & mask);
            }
        }
        !crc
    }

    fn patch_unicode_path_crc(file: &mut fs::File, extra_start: u64, extra: &[u8], crc: u32) {
        let mut offset = 0;
        while offset + 4 <= extra.len() {
            let id = u16::from_le_bytes([extra[offset], extra[offset + 1]]);
            let length = usize::from(u16::from_le_bytes([extra[offset + 2], extra[offset + 3]]));
            let data_start = offset + 4;
            let Some(data_end) = data_start.checked_add(length) else {
                break;
            };
            if data_end > extra.len() {
                break;
            }
            if id == 0x7075 && length >= 5 {
                file.seek(SeekFrom::Start(extra_start + data_start as u64 + 1))
                    .unwrap();
                file.write_all(&crc.to_le_bytes()).unwrap();
            }
            offset = data_end;
        }
    }

    #[test]
    fn bundles_channel_samples_and_rewrites_relative_paths() {
        let temporary = TestDirectory::new();
        let source_project = temporary.path().join("song.flp");
        let first_sample = temporary.path().join("left").join("kick.wav");
        let second_sample = temporary.path().join("right").join("kick.wav");
        fs::create_dir_all(first_sample.parent().unwrap()).unwrap();
        fs::create_dir_all(second_sample.parent().unwrap()).unwrap();
        fs::write(&first_sample, b"left sample data").unwrap();
        fs::write(&second_sample, b"right sample data").unwrap();

        let project_bytes = sample_project_fixture(&["left/kick.wav", "right/kick.wav"]);
        let mut document = FlpDocument::parse(&project_bytes).unwrap();
        let workspace =
            ProjectPackageWorkspace::single_project("song.flp", &project_bytes, &source_project)
                .unwrap();
        let summary = workspace
            .bundle_channel_samples(&mut document, &source_project)
            .unwrap();
        assert_eq!(summary.bundled_files, 2);
        assert_eq!(summary.unresolved_references, 0);
        assert_eq!(
            document.channels()[0].sample_path(),
            Some("Samples/kick.wav")
        );
        assert_eq!(
            document.channels()[1].sample_path(),
            Some("Samples/kick (2).wav")
        );

        let package_path = temporary.path().join("song.zip");
        workspace
            .write_to(&package_path, &document.encode_lossless().unwrap())
            .unwrap();
        let (opened_workspace, bundled_project) =
            ProjectPackageWorkspace::open(&package_path).expect("the generated ZIP should reopen");
        let bundled_document = FlpDocument::parse(&bundled_project).unwrap();
        let channels = bundled_document.channels();
        let resolver = SamplePathResolver::new(opened_workspace.sample_project_path());
        let first_resolved = resolver
            .resolve(channels[0].sample_path().unwrap())
            .unwrap();
        let second_resolved = resolver
            .resolve(channels[1].sample_path().unwrap())
            .unwrap();
        assert_eq!(fs::read(first_resolved).unwrap(), b"left sample data");
        assert_eq!(fs::read(second_resolved).unwrap(), b"right sample data");
    }

    #[test]
    fn copies_unique_samples_without_overwriting_existing_files() {
        let temporary = TestDirectory::new();
        let source = temporary.path().join("kick.wav");
        let sample_directory = temporary.path().join("Samples");
        let first_unique = sample_directory.join("kick (Unique).wav");
        fs::create_dir_all(&sample_directory).unwrap();
        fs::write(&source, b"source sample").unwrap();
        fs::write(&first_unique, b"keep this file").unwrap();

        let copied = super::copy_unique_sample_file(&source, &sample_directory).unwrap();

        assert_eq!(copied.file_name().unwrap(), "kick (Unique 2).wav");
        assert_eq!(fs::read(&first_unique).unwrap(), b"keep this file");
        assert_eq!(fs::read(copied).unwrap(), b"source sample");
    }

    #[test]
    fn package_unique_sample_reference_resolves_after_writing_the_archive() {
        let temporary = TestDirectory::new();
        let source_project = temporary.path().join("song.flp");
        let source_sample = temporary.path().join("source").join("kick.wav");
        fs::create_dir_all(source_sample.parent().unwrap()).unwrap();
        fs::write(&source_sample, b"package sample data").unwrap();
        let project_bytes = sample_project_fixture(&["source/kick.wav"]);
        let mut document = FlpDocument::parse(&project_bytes).unwrap();
        let workspace =
            ProjectPackageWorkspace::single_project("song.flp", &project_bytes, &source_project)
                .unwrap();

        let (copied_path, sample_reference) = workspace.copy_unique_sample(&source_sample).unwrap();
        document
            .set_channel_sample_path(document.channels()[0].id(), &sample_reference)
            .unwrap();
        let package_path = temporary.path().join("song.zip");
        workspace
            .write_to(&package_path, &document.encode_lossless().unwrap())
            .unwrap();

        let (opened_workspace, bundled_project) = ProjectPackageWorkspace::open(&package_path)
            .expect("the package with a unique sample should reopen");
        let bundled_document = FlpDocument::parse(&bundled_project).unwrap();
        let resolver = SamplePathResolver::new(opened_workspace.sample_project_path());
        let resolved = resolver
            .resolve(bundled_document.channels()[0].sample_path().unwrap())
            .unwrap();
        assert_eq!(fs::read(resolved).unwrap(), b"package sample data");
        assert!(copied_path.is_file());
    }

    #[test]
    fn preserves_zip_comments_compression_timestamps_and_permissions() {
        let temporary = TestDirectory::new();
        let source_path = temporary.path().join("source.zip");
        let output_path = temporary.path().join("saved.zip");
        let timestamp = DateTime::from_date_and_time(2020, 2, 3, 4, 5, 6).unwrap();
        let archive_comment = vec![0x80, b'p', b'a', b'c', b'k', b'a', b'g', b'e'];

        let mut source = ZipWriter::new(fs::File::create(&source_path).unwrap());
        let project_options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .last_modified_time(timestamp)
            .unix_permissions(0o640)
            .into_full_options()
            .with_file_comment("project note");
        source.start_file("song.flp", project_options).unwrap();
        source.write_all(b"original project").unwrap();

        let extra_options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Deflated)
            .last_modified_time(timestamp)
            .unix_permissions(0o600)
            .into_full_options()
            .with_file_comment("keep this comment");
        source
            .start_file("extras/notes.txt", extra_options)
            .unwrap();
        source.write_all(b"supporting file").unwrap();
        source
            .set_raw_comment(archive_comment.clone().into_boxed_slice())
            .unwrap();
        source.finish().unwrap().sync_all().unwrap();

        let (workspace, original_project) = ProjectPackageWorkspace::open(&source_path).unwrap();
        assert_eq!(original_project, b"original project");
        workspace
            .write_to(&output_path, b"updated project")
            .unwrap();

        assert_eq!(
            read_archive_comment(&output_path).unwrap().as_deref(),
            Some(archive_comment.as_slice())
        );
        let mut saved = ZipArchive::new(fs::File::open(&output_path).unwrap()).unwrap();
        let mut project = saved.by_name("song.flp").unwrap();
        assert_eq!(project.compression(), CompressionMethod::Stored);
        assert_eq!(project.last_modified(), Some(timestamp));
        assert_eq!(project.unix_mode().unwrap() & 0o777, 0o640);
        assert_eq!(project.comment(), "project note");
        let mut project_bytes = Vec::new();
        project.read_to_end(&mut project_bytes).unwrap();
        assert_eq!(project_bytes, b"updated project");
        drop(project);

        let mut extra = saved.by_name("extras/notes.txt").unwrap();
        assert_eq!(extra.compression(), CompressionMethod::Deflated);
        assert_eq!(extra.last_modified(), Some(timestamp));
        assert_eq!(extra.unix_mode().unwrap() & 0o777, 0o600);
        assert_eq!(extra.comment(), "keep this comment");
        let mut extra_bytes = Vec::new();
        extra.read_to_end(&mut extra_bytes).unwrap();
        assert_eq!(extra_bytes, b"supporting file");
    }

    #[test]
    fn preserves_legacy_raw_file_names_when_rewriting_packages() {
        let temporary = TestDirectory::new();
        let source_path = temporary.path().join("source.zip");
        let output_path = temporary.path().join("saved.zip");

        let legacy_name = b"caf\x82.flp";
        let mut unicode_path_data = vec![1, 0, 0, 0, 0];
        unicode_path_data.extend_from_slice("café.flp".as_bytes());
        let mut project_options: FullFileOptions<'static> = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .into_full_options();
        project_options
            .add_extra_data(0x7075, &unicode_path_data, true)
            .unwrap();
        let mut source = ZipWriter::new(fs::File::create(&source_path).unwrap());
        source.start_file("cafe.flp", project_options).unwrap();
        source.write_all(b"original project").unwrap();
        source
            .start_file("supporting.txt", SimpleFileOptions::default())
            .unwrap();
        source.write_all(b"supporting file").unwrap();
        source.finish().unwrap().sync_all().unwrap();

        let local_header_start = 0;
        let mut source = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&source_path)
            .unwrap();
        let file_length = source.metadata().unwrap().len();
        let mut end_record = [0; 22];
        source
            .seek(SeekFrom::End(-(end_record.len() as i64)))
            .unwrap();
        source.read_exact(&mut end_record).unwrap();
        let central_header_start = u64::from(u32::from_le_bytes([
            end_record[16],
            end_record[17],
            end_record[18],
            end_record[19],
        ]));
        assert!(central_header_start < file_length);
        let original_path_crc = fixture_crc32(b"cafe.flp");
        let central_extra = read_central_extra_data(&mut source, central_header_start).unwrap();
        patch_unicode_path_crc(
            &mut source,
            central_header_start + 46 + b"cafe.flp".len() as u64,
            &central_extra,
            original_path_crc,
        );
        source.sync_all().unwrap();
        drop(source);

        let mut archive = ZipArchive::new(fs::File::open(&source_path).unwrap()).unwrap();
        let project = archive.by_index(0).unwrap();
        assert_eq!(project.name(), "café.flp");
        assert_eq!(project.header_start(), local_header_start);
        assert_eq!(project.central_header_start(), central_header_start);
        drop(project);
        drop(archive);

        let mut source = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&source_path)
            .unwrap();
        source
            .seek(SeekFrom::Start(local_header_start + 30))
            .unwrap();
        source.write_all(legacy_name).unwrap();
        source
            .seek(SeekFrom::Start(central_header_start + 46))
            .unwrap();
        source.write_all(legacy_name).unwrap();
        for offset in [local_header_start + 6, central_header_start + 8] {
            let mut flags = [0; 2];
            source.seek(SeekFrom::Start(offset)).unwrap();
            source.read_exact(&mut flags).unwrap();
            let flags = u16::from_le_bytes(flags) & !0x0800;
            source.seek(SeekFrom::Start(offset)).unwrap();
            source.write_all(&flags.to_le_bytes()).unwrap();
        }
        let legacy_path_crc = fixture_crc32(legacy_name);
        let central_extra = read_central_extra_data(&mut source, central_header_start).unwrap();
        patch_unicode_path_crc(
            &mut source,
            central_header_start + 46 + legacy_name.len() as u64,
            &central_extra,
            legacy_path_crc,
        );
        source.sync_all().unwrap();
        drop(source);

        let (workspace, original_project) = ProjectPackageWorkspace::open(&source_path).unwrap();
        assert_eq!(original_project, b"original project");
        workspace
            .write_to(&output_path, b"updated project")
            .unwrap();

        let mut saved = ZipArchive::new(fs::File::open(&output_path).unwrap()).unwrap();
        let mut project = saved.by_index(0).unwrap();
        assert_eq!(project.name(), "café.flp");
        let mut project_bytes = Vec::new();
        project.read_to_end(&mut project_bytes).unwrap();
        assert_eq!(project_bytes, b"updated project");
        let local_header_start = project.header_start();
        let central_header_start = project.central_header_start();
        drop(project);
        let mut saved_file = fs::File::open(&output_path).unwrap();
        let (local_name, local_utf8) =
            read_local_file_name(&mut saved_file, local_header_start).unwrap();
        let (central_name, central_utf8) =
            read_central_file_name(&mut saved_file, central_header_start).unwrap();
        assert_eq!(local_name, legacy_name);
        assert_eq!(central_name, legacy_name);
        assert!(!local_utf8);
        assert!(!central_utf8);
        let central_extra = read_central_extra_data(&mut saved_file, central_header_start).unwrap();
        let unicode_path_fields = preserve_extra_fields_with_id(&central_extra, 0x7075);
        assert_eq!(unicode_path_fields.len(), 1);
        let mut expected_unicode_path_data = vec![1];
        expected_unicode_path_data.extend_from_slice(&fixture_crc32(legacy_name).to_le_bytes());
        expected_unicode_path_data.extend_from_slice("café.flp".as_bytes());
        assert_eq!(unicode_path_fields[0].data, expected_unicode_path_data);

        let mut supporting = saved.by_name("supporting.txt").unwrap();
        let mut supporting_bytes = Vec::new();
        supporting.read_to_end(&mut supporting_bytes).unwrap();
        assert_eq!(supporting_bytes, b"supporting file");
    }

    #[test]
    fn preserves_forced_zip64_entry_sizes_when_rewriting_small_packages() {
        let temporary = TestDirectory::new();
        let source_path = temporary.path().join("source.zip");
        let output_path = temporary.path().join("saved.zip");

        let options = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .large_file(true)
            .into_full_options();
        let mut source = ZipWriter::new(fs::File::create(&source_path).unwrap());
        source.start_file("song.flp", options).unwrap();
        source.write_all(b"original project").unwrap();
        source.finish().unwrap().sync_all().unwrap();

        let (workspace, original_project) = ProjectPackageWorkspace::open(&source_path).unwrap();
        assert_eq!(original_project, b"original project");
        workspace
            .write_to(&output_path, b"updated project")
            .unwrap();

        let mut saved = ZipArchive::new(fs::File::open(&output_path).unwrap()).unwrap();
        let project = saved.by_name("song.flp").unwrap();
        let header_start = project.header_start();
        let central_header_start = project.central_header_start();
        drop(project);
        let mut file = fs::File::open(&output_path).unwrap();
        let local_extra_data = read_local_extra_data(&mut file, header_start).unwrap();
        let central_extra_data = read_central_extra_data(&mut file, central_header_start).unwrap();
        assert!(contains_zip64_extra_field(&local_extra_data));
        assert!(contains_zip64_extra_field(&central_extra_data));

        let mut project = saved.by_name("song.flp").unwrap();
        let mut project_bytes = Vec::new();
        project.read_to_end(&mut project_bytes).unwrap();
        assert_eq!(project_bytes, b"updated project");
    }

    #[test]
    fn preserves_unrecognized_local_and_central_zip_extra_fields() {
        let temporary = TestDirectory::new();
        let source_path = temporary.path().join("source.zip");
        let output_path = temporary.path().join("saved.zip");
        let local_payload = b"vendor local metadata";
        let central_payload = b"vendor central metadata";

        let mut options: FullFileOptions<'static> = SimpleFileOptions::default()
            .compression_method(CompressionMethod::Stored)
            .into_full_options();
        options
            .add_extra_data(0xcafe, local_payload, false)
            .unwrap();
        options
            .add_extra_data(0xbeef, central_payload, true)
            .unwrap();
        let mut source = ZipWriter::new(fs::File::create(&source_path).unwrap());
        source.start_file("song.flp", options).unwrap();
        source.write_all(b"original project").unwrap();
        source.finish().unwrap().sync_all().unwrap();

        let (workspace, original_project) = ProjectPackageWorkspace::open(&source_path).unwrap();
        assert_eq!(original_project, b"original project");
        workspace
            .write_to(&output_path, b"updated project")
            .unwrap();

        let mut saved = ZipArchive::new(fs::File::open(&output_path).unwrap()).unwrap();
        let project = saved.by_name("song.flp").unwrap();
        let mut expected_local = Vec::new();
        expected_local.extend_from_slice(&0xcafe_u16.to_le_bytes());
        expected_local.extend_from_slice(&(local_payload.len() as u16).to_le_bytes());
        expected_local.extend_from_slice(local_payload);
        let mut local_file = fs::File::open(&output_path).unwrap();
        let local_extra_data =
            read_local_extra_data(&mut local_file, project.header_start()).unwrap();
        assert_eq!(local_extra_data, expected_local);

        let mut central_file = fs::File::open(&output_path).unwrap();
        let central_extra_data =
            read_central_extra_data(&mut central_file, project.central_header_start()).unwrap();
        let mut expected_central = expected_local;
        expected_central.extend_from_slice(&0xbeef_u16.to_le_bytes());
        expected_central.extend_from_slice(&(central_payload.len() as u16).to_le_bytes());
        expected_central.extend_from_slice(central_payload);
        assert_eq!(central_extra_data, expected_central);
    }
}
