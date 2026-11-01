//! Auto-extract of finished downloads, and the "organize" rule for finished files.
//!
//! Supported archives: `.zip`, `.tar`, `.tgz`/`.tar.gz`, `.txz`/`.tar.xz`.
//! Deliberately not supported: bare `.gz`/`.xz` (a single compressed file, not an
//! archive), `.tar.bz2` (no bzip2 crate in the tree) and 7z (no 7z crate in the tree).
//!
//! Safety rules, all load-bearing:
//! * The archive is only ever opened for reading. A failed extraction never touches it.
//! * On any extraction error the destination folder is removed in full, so a failed
//!   extraction never leaves a half-unpacked folder behind.
//! * Entries that would escape the destination (absolute paths, `..` components) or
//!   that are links are refused: links are skipped in tar archives, and an unsafe
//!   path in either format fails the whole extraction.

use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// Archive format detected from a file name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveKind {
    Zip,
    Tar,
    TarGz,
    TarXz,
}

/// Detect an archive format from a file name, or `None` when it is not a
/// supported archive.
pub fn detect_archive(file_name: &str) -> Option<ArchiveKind> {
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".zip") {
        Some(ArchiveKind::Zip)
    } else if lower.ends_with(".tgz") || lower.ends_with(".tar.gz") {
        Some(ArchiveKind::TarGz)
    } else if lower.ends_with(".txz") || lower.ends_with(".tar.xz") {
        Some(ArchiveKind::TarXz)
    } else if lower.ends_with(".tar") {
        Some(ArchiveKind::Tar)
    } else {
        None
    }
}

/// The folder an archive extracts into: its file name without the archive
/// extension (`backup.tar.gz` → `backup`, `photos.zip` → `photos`).
pub fn archive_stem(file_name: &str) -> String {
    let lower = file_name.to_ascii_lowercase();
    for suffix in [".tar.gz", ".tar.xz", ".tgz", ".txz", ".zip", ".tar"] {
        if lower.ends_with(suffix) {
            return file_name[..file_name.len() - suffix.len()].to_string();
        }
    }
    Path::new(file_name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_name.to_string())
}

/// Extract `archive` into `dest_dir` (created if needed). Returns the
/// destination on success. On any error the destination is removed in full
/// and the archive is left untouched.
pub fn extract_archive(archive: &Path, dest_dir: &Path) -> Result<PathBuf, String> {
    let kind = detect_archive(
        &archive
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    )
    .ok_or_else(|| "not a supported archive".to_string())?;
    fs::create_dir_all(dest_dir).map_err(|e| e.to_string())?;
    let result = match kind {
        ArchiveKind::Zip => extract_zip(archive, dest_dir),
        ArchiveKind::Tar => extract_tar(archive, dest_dir, TarReader::Plain),
        ArchiveKind::TarGz => extract_tar(archive, dest_dir, TarReader::Gzip),
        ArchiveKind::TarXz => extract_tar(archive, dest_dir, TarReader::Xz),
    };
    match result {
        Ok(()) => Ok(dest_dir.to_path_buf()),
        Err(e) => {
            let _ = fs::remove_dir_all(dest_dir);
            Err(e)
        }
    }
}

enum TarReader {
    Plain,
    Gzip,
    Xz,
}

/// A path inside an archive is safe only when it is relative and has no
/// parent-directory component.
fn safe_entry_path(name: &Path) -> Result<PathBuf, String> {
    let mut out = PathBuf::new();
    for part in name.components() {
        match part {
            Component::Normal(p) => out.push(p),
            Component::CurDir => {}
            Component::RootDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(format!(
                    "unsafe entry in archive: {}",
                    name.to_string_lossy()
                ));
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err("empty entry in archive".to_string());
    }
    Ok(out)
}

fn extract_zip(archive: &Path, dest_dir: &Path) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|e| e.to_string())?;
    let mut zip = zip::ZipArchive::new(io::BufReader::new(file)).map_err(|e| e.to_string())?;
    for i in 0..zip.len() {
        let mut entry = zip.by_index(i).map_err(|e| e.to_string())?;
        let name = entry.name().map_err(|e| e.to_string())?.into_owned();
        let rel = safe_entry_path(Path::new(&name))?;
        let out_path = dest_dir.join(rel);
        if entry.is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = fs::File::create(&out_path).map_err(|e| e.to_string())?;
        // A read error here covers encrypted entries (no password support) and
        // corrupt data: the caller removes the whole destination folder.
        io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn extract_tar(archive: &Path, dest_dir: &Path, reader: TarReader) -> Result<(), String> {
    let file = fs::File::open(archive).map_err(|e| e.to_string())?;
    let buf = io::BufReader::with_capacity(1 << 20, file);
    match reader {
        TarReader::Plain => extract_tar_entries(tar::Archive::new(buf), dest_dir),
        TarReader::Gzip => extract_tar_entries(
            tar::Archive::new(flate2::read::GzDecoder::new(buf)),
            dest_dir,
        ),
        TarReader::Xz => extract_tar_entries(
            tar::Archive::new(lzma_rust2::XzReader::new(buf, true)),
            dest_dir,
        ),
    }
}

fn extract_tar_entries<R: io::Read>(
    mut tar: tar::Archive<R>,
    dest_dir: &Path,
) -> Result<(), String> {
    let entries = tar.entries().map_err(|e| e.to_string())?;
    for entry in entries {
        let mut entry = entry.map_err(|e| e.to_string())?;
        let header = entry.header();
        // Links are never followed: a link pointing outside the destination
        // would write outside it on a later read.
        if !header.entry_type().is_file() && !header.entry_type().is_dir() {
            continue;
        }
        let path = entry.path().map_err(|e| e.to_string())?.into_owned();
        let rel = safe_entry_path(&path)?;
        let out_path = dest_dir.join(rel);
        if header.entry_type().is_dir() {
            fs::create_dir_all(&out_path).map_err(|e| e.to_string())?;
            continue;
        }
        if let Some(parent) = out_path.parent() {
            fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let mut out = fs::File::create(&out_path).map_err(|e| e.to_string())?;
        io::copy(&mut entry, &mut out).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// Where a finished file should move under the organize rule, or `None` when
/// it already sits where it belongs.
///
/// The rule is deliberately narrow: only a file sitting directly in the
/// download folder moves, and only into its own category's folder (never for
/// the "other" category, which has no folder by design). Files in a subfolder
/// — a category folder, or a folder the user picked — are left alone.
pub fn organize_target(
    file_path: &Path,
    default_dir: &Path,
    use_category_folders: bool,
    category_id: &str,
    category_name: &str,
) -> Option<PathBuf> {
    if !use_category_folders || category_id == "other" || category_name.is_empty() {
        return None;
    }
    let parent = file_path.parent()?;
    if parent != default_dir {
        return None;
    }
    let file_name = file_path.file_name()?;
    Some(default_dir.join(category_name).join(file_name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn write_zip(path: &Path, files: &[(&str, &str)]) {
        let file = fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let opts = zip::write::SimpleFileOptions::default();
        for (name, content) in files {
            zip.start_file(*name, opts).unwrap();
            zip.write_all(content.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    fn write_tar_gz(path: &Path, files: &[(&str, &str)]) {
        let file = fs::File::create(path).unwrap();
        let gz = flate2::write::GzEncoder::new(file, flate2::Compression::fast());
        let mut tar = tar::Builder::new(gz);
        for (name, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            tar.append_data(&mut header, name, content.as_bytes())
                .unwrap();
        }
        tar.finish().unwrap();
    }

    #[test]
    fn detect_and_stem() {
        assert_eq!(detect_archive("a.zip"), Some(ArchiveKind::Zip));
        assert_eq!(detect_archive("a.TAR.GZ"), Some(ArchiveKind::TarGz));
        assert_eq!(detect_archive("a.tgz"), Some(ArchiveKind::TarGz));
        assert_eq!(detect_archive("a.tar.xz"), Some(ArchiveKind::TarXz));
        assert_eq!(detect_archive("a.txz"), Some(ArchiveKind::TarXz));
        assert_eq!(detect_archive("a.tar"), Some(ArchiveKind::Tar));
        assert_eq!(detect_archive("a.gz"), None);
        assert_eq!(detect_archive("a.7z"), None);
        assert_eq!(detect_archive("a.rar"), None);
        assert_eq!(archive_stem("backup.tar.gz"), "backup");
        assert_eq!(archive_stem("photos.ZIP"), "photos");
        assert_eq!(archive_stem("data.txz"), "data");
    }

    #[test]
    fn zip_extracts_and_keeps_archive() {
        let dir = tmp();
        let archive = dir.path().join("pack.zip");
        write_zip(
            &archive,
            &[("hello.txt", "hello"), ("sub/world.txt", "world")],
        );
        let dest = dir.path().join("pack");
        let out = extract_archive(&archive, &dest).unwrap();
        assert_eq!(out, dest);
        assert_eq!(fs::read_to_string(dest.join("hello.txt")).unwrap(), "hello");
        assert_eq!(
            fs::read_to_string(dest.join("sub/world.txt")).unwrap(),
            "world"
        );
        assert!(archive.exists(), "archive must survive extraction");
    }

    #[test]
    fn tar_gz_extracts() {
        let dir = tmp();
        let archive = dir.path().join("pack.tar.gz");
        write_tar_gz(&archive, &[("a.txt", "A"), ("d/b.txt", "B")]);
        let dest = dir.path().join("pack");
        extract_archive(&archive, &dest).unwrap();
        assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "A");
        assert_eq!(fs::read_to_string(dest.join("d/b.txt")).unwrap(), "B");
        assert!(archive.exists());
    }

    #[test]
    fn unsafe_zip_entry_fails_and_leaves_nothing() {
        let dir = tmp();
        let archive = dir.path().join("evil.zip");
        write_zip(&archive, &[("../escape.txt", "x")]);
        let dest = dir.path().join("evil");
        let err = extract_archive(&archive, &dest).unwrap_err();
        assert!(err.contains("unsafe entry"), "{err}");
        assert!(!dest.exists(), "failed extraction must not leave a folder");
        assert!(archive.exists(), "archive must survive a failed extraction");
    }

    #[test]
    fn corrupt_archive_fails_and_leaves_nothing() {
        let dir = tmp();
        let archive = dir.path().join("broken.zip");
        fs::write(&archive, b"this is not a zip at all").unwrap();
        let dest = dir.path().join("broken");
        assert!(extract_archive(&archive, &dest).is_err());
        assert!(!dest.exists());
        assert!(archive.exists());
    }

    #[test]
    fn organize_rule_only_moves_loose_files() {
        let root = Path::new("/dl");
        assert_eq!(
            organize_target(&root.join("a.txt"), root, true, "documents", "Documents"),
            Some(root.join("Documents").join("a.txt"))
        );
        // Already in a category folder: leave alone.
        assert_eq!(
            organize_target(
                &root.join("Documents").join("a.txt"),
                root,
                true,
                "documents",
                "Documents"
            ),
            None
        );
        // Category folders off, or "other": leave alone.
        assert_eq!(
            organize_target(&root.join("a.txt"), root, false, "documents", "Documents"),
            None
        );
        assert_eq!(
            organize_target(&root.join("a.bin"), root, true, "other", "Other"),
            None
        );
    }
}
