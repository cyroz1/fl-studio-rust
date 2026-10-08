use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

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
