//! Private journal artifacts and synced publication on Unix and Windows.
//!
//! Windows uses write-through replacement, not a fictitious directory fsync.
//! Deletion reclaims a retired original; the synced accepted head remains the
//! retirement authority if a deleted original reappears after a power loss.

use std::{
    fs::{self, File, OpenOptions},
    io,
    path::Path,
};

#[cfg(windows)]
mod windows;

fn unsafe_storage(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn checked_path(path: &Path) -> io::Result<()> {
    if !path.is_absolute()
        || path.components().any(|part| {
            matches!(part, std::path::Component::ParentDir)
                || part.as_os_str() == ".agent-shell"
                || (cfg!(windows) && part.as_os_str().to_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case(".agent-shell")))
                // A colon in a Windows name selects an alternate data stream.
                || (cfg!(windows) && matches!(part, std::path::Component::Normal(name)
                    if name.to_str().is_none_or(|name| !plain_windows_name(name))))
        })
    {
        return Err(unsafe_storage("unsafe event journal path"));
    }
    Ok(())
}

fn plain_windows_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).trim_end_matches(' ');
    !name.contains(':')
        && !name.ends_with(['.', ' '])
        && ![
            "CON",
            "PRN",
            "AUX",
            "NUL",
            "COM1",
            "COM2",
            "COM3",
            "COM4",
            "COM5",
            "COM6",
            "COM7",
            "COM8",
            "COM9",
            "LPT1",
            "LPT2",
            "LPT3",
            "LPT4",
            "LPT5",
            "LPT6",
            "LPT7",
            "LPT8",
            "LPT9",
            // Windows also reads the ISO 8859-1 superscript digits as port digits.
            "COM\u{b9}",
            "COM\u{b2}",
            "COM\u{b3}",
            "LPT\u{b9}",
            "LPT\u{b2}",
            "LPT\u{b3}",
        ]
        .iter()
        .any(|device| stem.eq_ignore_ascii_case(device))
}

/// Create a new account-private journal directory without adopting or changing
/// an existing directory. The parent must already exist.
///
/// Unix creation uses mode 0700. Windows creation uses a protected DACL owned
/// by the current process account, granting access only to that account,
/// LocalSystem, and Administrators. Existing storage is validated separately;
/// an existing path returns `AlreadyExists`, never a permission repair.
pub fn create_event_journal_directory(path: &Path) -> io::Result<()> {
    checked_path(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| unsafe_storage("journal directory has no parent"))?;
    checked_path(&fs::canonicalize(parent)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new().mode(0o700).create(path)?;
        File::open(parent)?.sync_all()?;
    }
    #[cfg(windows)]
    windows::create_directory(path)?;
    #[cfg(not(any(unix, windows)))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private journal storage is unsupported on this platform",
    ));
    validate_directory(path)
}

pub(super) fn validate_directory(path: &Path) -> io::Result<()> {
    let _directory = Directory::open(path)?;
    Ok(())
}

struct Directory {
    // Windows handles deny sharing for deletion to pin the checked path during
    // publication. Unix retains the directory handle for deletion flushing.
    handles: Vec<File>,
}

impl Directory {
    fn open(path: &Path) -> io::Result<Self> {
        checked_path(path)?;
        let canonical = fs::canonicalize(path)?;
        checked_path(&canonical)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let metadata = fs::symlink_metadata(path)?;
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || metadata.permissions().mode() & 0o077 != 0
            {
                return Err(unsafe_storage(
                    "event journal directory must be private (mode 0700)",
                ));
            }
            Ok(Self {
                handles: vec![File::open(path)?],
            })
        }
        #[cfg(windows)]
        {
            Ok(Self {
                handles: windows::open_directory_chain(path)?,
            })
        }
        #[cfg(not(any(unix, windows)))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "private journal storage is unsupported on this platform",
        ))
    }

    fn parent(path: &Path) -> io::Result<Self> {
        checked_path(path)?;
        Self::open(
            path.parent()
                .ok_or_else(|| unsafe_storage("event journal path has no parent"))?,
        )
    }

    fn finish_reclamation(&self) -> io::Result<()> {
        #[cfg(unix)]
        self.handles
            .last()
            .ok_or_else(|| unsafe_storage("event journal directory handle is absent"))?
            .sync_all()?;
        // Windows has no corresponding per-directory flush here. Authoritative
        // retirement precedes reclamation; a surviving original cannot reemit.
        #[cfg(windows)]
        let _ = &self.handles;
        Ok(())
    }
}

fn validate_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(unsafe_storage(
            "event journal artifact must be a regular file",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(unsafe_storage(
                "event journal artifact must be private (mode 0600)",
            ));
        }
    }
    #[cfg(windows)]
    windows::validate_private(file)?;
    Ok(())
}

pub(super) fn open_existing(path: &Path, write: bool) -> io::Result<File> {
    let _directory = Directory::parent(path)?;
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(unsafe_storage(
            "event journal artifact must be a regular file, not a symlink",
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true).write(write);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
        };
        options
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE);
    }
    let file = options.open(path)?;
    // An 8.3 short name aliases its long name. The open handle denies delete
    // sharing, so the canonical name belongs to the object being validated.
    #[cfg(windows)]
    checked_path(&fs::canonicalize(path)?)?;
    validate_file(&file)?;
    Ok(file)
}

pub(super) fn validate_artifact(path: &Path) -> io::Result<()> {
    drop(open_existing(path, false)?);
    Ok(())
}

pub(super) fn create_private_file(path: &Path) -> io::Result<File> {
    let _directory = Directory::parent(path)?;
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt;
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?
    };
    #[cfg(windows)]
    let file = windows::create_file(path)?;
    #[cfg(not(any(unix, windows)))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "private journal storage is unsupported on this platform",
    ));
    validate_file(&file)?;
    Ok(file)
}

pub(super) fn publish(temporary: &Path, target: &Path) -> io::Result<()> {
    if temporary.parent() != target.parent() {
        return Err(unsafe_storage(
            "journal replacement must remain in one directory",
        ));
    }
    let _directory = Directory::parent(target)?;
    validate_artifact(temporary)?;
    match validate_artifact(target) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // The caller synced and closed the temporary before publication. The
    // library syncs the Unix directory or uses Windows MOVEFILE_WRITE_THROUGH.
    atomicwrites::replace_atomic(temporary, target)
}

/// Reclaim a checked artifact. Callers must establish that it is not an
/// authoritative pending original before deleting it.
pub(super) fn reclaim(path: &Path) -> io::Result<()> {
    let directory = Directory::parent(path)?;
    validate_artifact(path)?;
    fs::remove_file(path)?;
    directory.finish_reclamation()
}

#[cfg(test)]
pub(super) fn file_identity(path: &Path) -> io::Result<(u64, u64)> {
    let file = open_existing(path, false)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata()?;
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        use windows_sys::Win32::Storage::FileSystem::{
            BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
        };
        let mut information = BY_HANDLE_FILE_INFORMATION::default();
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut information) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((
            u64::from(information.dwVolumeSerialNumber),
            (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        sync::atomic::{AtomicU64, Ordering},
    };

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    fn directory() -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "dispatch-journal-fs-{}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        create_event_journal_directory(&path).unwrap();
        path
    }

    #[test]
    fn new_directory_is_private_and_existing_directory_is_never_adopted() {
        let root = directory();
        validate_directory(&root).unwrap();
        assert_eq!(
            create_event_journal_directory(&root).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        validate_directory(&root).unwrap();
        let path = root.join("private");
        drop(create_private_file(&path).unwrap());
        validate_artifact(&path).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publication_replaces_exact_bytes_and_failed_publication_preserves_original() {
        let root = directory();
        let target = root.join("journal");
        let temporary = root.join("journal.pending");
        for bytes in [b"first".as_slice(), b"second".as_slice()] {
            let mut file = create_private_file(&temporary).unwrap();
            file.write_all(bytes).unwrap();
            file.sync_all().unwrap();
            drop(file);
            publish(&temporary, &target).unwrap();
            let mut observed = Vec::new();
            open_existing(&target, false)
                .unwrap()
                .read_to_end(&mut observed)
                .unwrap();
            assert_eq!(observed, bytes);
            assert!(!temporary.exists());
        }
        assert!(publish(&temporary, &target).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"second");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsafe_artifact_is_neither_read_replaced_nor_reclaimed() {
        let root = directory();
        let path = root.join("public");
        let mut file = create_private_file(&path).unwrap();
        file.write_all(b"unchanged").unwrap();
        file.sync_all().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o644))
                .unwrap();
        }
        #[cfg(windows)]
        windows::tests::grant_everyone(&path);
        drop(file);
        assert!(open_existing(&path, false).is_err());
        assert!(reclaim(&path).is_err());
        let temporary = root.join("replacement");
        let mut file = create_private_file(&temporary).unwrap();
        file.write_all(b"replacement").unwrap();
        file.sync_all().unwrap();
        drop(file);
        assert!(publish(&temporary, &path).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"unchanged");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn directory_permissions_and_symlinks_are_rejected_without_repair() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = directory();
        let directory = root.join("public-directory");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(validate_directory(&directory).is_err());
        assert_eq!(
            fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o755
        );
        let target = root.join("target");
        drop(create_private_file(&target).unwrap());
        let link = root.join("link");
        symlink(&target, &link).unwrap();
        assert!(validate_artifact(&link).is_err());
        assert!(reclaim(&link).is_err());
        assert!(target.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn broad_directory_acl_is_rejected_without_repair() {
        let root = directory();
        windows::tests::grant_everyone(&root);
        assert!(validate_directory(&root).is_err());
        assert!(create_private_file(&root.join("not-created")).is_err());
        assert!(!root.join("not-created").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn null_dacl_is_not_treated_as_private_storage() {
        let root = directory();
        let path = root.join("unrestricted");
        drop(create_private_file(&path).unwrap());
        windows::tests::remove_dacl(&path);
        assert!(validate_artifact(&path).is_err());
        assert!(reclaim(&path).is_err());
        assert!(path.exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn stream_and_normalized_alias_names_are_rejected_without_creation() {
        let root = directory();
        let base = root.join("journal");
        drop(create_private_file(&base).unwrap());
        let stream = root.join("journal:payload");
        assert!(create_private_file(&stream).is_err());
        assert!(validate_artifact(&stream).is_err());
        assert!(reclaim(&stream).is_err());
        assert_eq!(fs::metadata(&base).unwrap().len(), 0);
        // Protected names are checked without any filesystem attempt, so a
        // regression cannot create a session entry even in a temporary root.
        for name in [
            ".agent-shell",
            ".AGENT-SHELL",
            ".agent-shell.",
            ".agent-shell ",
        ] {
            assert!(checked_path(&root.join(name)).is_err(), "{name}");
        }
        for name in [
            "trailing.",
            "scope ",
            "NUL",
            "con.txt",
            "COM9 .db",
            "lpt\u{b3}.log",
        ] {
            let path = root.join(name);
            assert!(create_event_journal_directory(&path).is_err());
            assert!(create_private_file(&path).is_err());
            assert!(validate_artifact(&path).is_err());
            assert!(reclaim(&path).is_err());
            assert!(!root.join(name.trim_end_matches(['.', ' '])).exists());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn short_name_alias_of_an_unsafe_path_is_not_modified() {
        use std::os::windows::ffi::{OsStrExt, OsStringExt};
        use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
        let root = directory();
        // A verbatim trailing-dot fixture exercises the canonical-name guard
        // without creating or modifying any protected session entry. Its base
        // exceeds eight characters, so an enabled volume must give it an alias.
        let original = fs::canonicalize(&root).unwrap().join("unsafe-journal.");
        let mut file = windows::create_file(&original).unwrap();
        file.write_all(b"unchanged").unwrap();
        file.sync_all().unwrap();
        drop(file);
        let wide: Vec<u16> = original.as_os_str().encode_wide().chain(Some(0)).collect();
        let mut short = vec![0u16; 4096];
        let length =
            unsafe { GetShortPathNameW(wide.as_ptr(), short.as_mut_ptr(), short.len() as u32) }
                as usize;
        assert!(
            length > 0 && length < short.len(),
            "{}",
            io::Error::last_os_error()
        );
        let short = std::path::PathBuf::from(std::ffi::OsString::from_wide(&short[..length]));
        // Volumes with 8.3 name generation disabled have no alias to test.
        if short.file_name() != original.file_name() {
            let alias = root.join(short.file_name().unwrap());
            assert!(validate_artifact(&alias).is_err());
            assert!(open_existing(&alias, true).is_err());
            assert!(create_private_file(&alias).is_err());
            assert!(reclaim(&alias).is_err());
            let temporary = root.join("replacement");
            let mut file = create_private_file(&temporary).unwrap();
            file.write_all(b"replacement").unwrap();
            file.sync_all().unwrap();
            drop(file);
            assert!(publish(&temporary, &alias).is_err());
        }
        assert_eq!(fs::read(&original).unwrap(), b"unchanged");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn directory_junction_is_rejected_without_touching_its_target() {
        let root = directory();
        let target = root.join("target");
        create_event_journal_directory(&target).unwrap();
        let link = root.join("link");
        let output = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J"])
            .arg(&link)
            .arg(&target)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction creation failed: {output:?}"
        );
        assert!(validate_directory(&link).is_err());
        assert!(create_private_file(&link.join("not-created")).is_err());
        assert!(!target.join("not-created").exists());
        fs::remove_dir(&link).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}
