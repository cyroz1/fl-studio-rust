use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::FlpDocument;
use crate::media::SamplePathResolver;

static WORKSPACE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct WorkspaceInner {
    root: PathBuf,
    project_relative_path: PathBuf,
    sample_project_path: PathBuf,
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
        let source = File::open(path.as_ref())
            .map_err(|error| format!("could not open {}: {error}", path.as_ref().display()))?;
        let mut archive = ZipArchive::new(source)
            .map_err(|error| format!("could not read ZIP project: {error}"))?;
        let workspace_directory = WorkspaceDirectory::new(create_workspace_directory()?);
        let root = workspace_directory.path();
        let mut project_files = Vec::new();
        let mut extracted_files = HashSet::new();

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

            if entry.is_dir() {
                fs::create_dir_all(&target).map_err(|error| {
                    format!(
                        "could not create package directory {}: {error}",
                        target.display()
                    )
                })?;
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
        })))
    }

    pub fn project_path(&self) -> PathBuf {
        self.0.root.join(&self.0.project_relative_path)
    }

    pub fn sample_project_path(&self) -> &Path {
        &self.0.sample_project_path
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
        let result = self.write_archive(file, project_bytes);
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

        let mut archive = ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for entry in entries {
            let name = archive_name(&entry.relative);
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

struct WorkspaceEntry {
    relative: PathBuf,
    is_directory: bool,
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
    use super::ProjectPackageWorkspace;
    use crate::FlpDocument;
    use crate::media::SamplePathResolver;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::Ordering;

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
}
