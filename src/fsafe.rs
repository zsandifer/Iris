//! File access beneath one person's folder that cannot be redirected.
//!
//! A person's agent can write in their own workspace and memory folders, so
//! it could swap a folder for a symlink to someone else's files between a
//! path check and the access. Here every step of a path is opened relative
//! to the folder opened before it, with `O_NOFOLLOW`, and the final read,
//! write, rename, or delete uses that folder's descriptor: a symlink at any
//! step fails, and nothing is resolved twice.

use std::ffi::CString;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path};

/// Split a relative path into plain names: no `..`, no absolute paths, no
/// hidden names.
pub fn components(rel: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for comp in Path::new(rel).components() {
        match comp {
            Component::Normal(name) => {
                let name = name.to_string_lossy();
                if name.starts_with('.') {
                    return Err("hidden files are not accessible".into());
                }
                out.push(name.into_owned());
            }
            Component::CurDir => {}
            _ => return Err("invalid path".into()),
        }
    }
    Ok(out)
}

fn cstr(s: &[u8]) -> io::Result<CString> {
    CString::new(s).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "bad name"))
}

fn check(rc: libc::c_int) -> io::Result<libc::c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

fn open_root(root: &Path) -> io::Result<OwnedFd> {
    let p = cstr(root.as_os_str().as_bytes())?;
    // SAFETY: valid C string; the returned descriptor is owned below.
    let fd = check(unsafe {
        libc::open(
            p.as_ptr(),
            libc::O_DIRECTORY | libc::O_RDONLY | libc::O_CLOEXEC,
        )
    })?;
    // SAFETY: fd was just opened and is not owned elsewhere.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn open_dir_at(dir: &OwnedFd, name: &str, create: bool) -> io::Result<OwnedFd> {
    let n = cstr(name.as_bytes())?;
    if create {
        // SAFETY: valid descriptor and C string.
        let rc = unsafe { libc::mkdirat(dir.as_raw_fd(), n.as_ptr(), 0o700) };
        if rc < 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
            return Err(io::Error::last_os_error());
        }
    }
    // SAFETY: as above; O_NOFOLLOW refuses a symlink in place of the folder.
    let fd = check(unsafe {
        libc::openat(
            dir.as_raw_fd(),
            n.as_ptr(),
            libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_RDONLY | libc::O_CLOEXEC,
        )
    })?;
    // SAFETY: freshly opened descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Open the folder `root/comps...` step by step (creating missing folders
/// when asked).
fn walk(root: &Path, comps: &[String], create: bool) -> io::Result<OwnedFd> {
    let mut dir = open_root(root)?;
    for c in comps {
        dir = open_dir_at(&dir, c, create)?;
    }
    Ok(dir)
}

fn split(comps: &[String]) -> io::Result<(&[String], &str)> {
    match comps.split_last() {
        Some((last, parents)) => Ok((parents, last.as_str())),
        None => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a file name is required",
        )),
    }
}

fn stat_at(dir: &OwnedFd, name: &str) -> io::Result<libc::stat> {
    let n = cstr(name.as_bytes())?;
    // SAFETY: zeroed stat is a valid out-parameter for fstatat.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    check(unsafe {
        libc::fstatat(
            dir.as_raw_fd(),
            n.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })?;
    Ok(st)
}

fn is_type(st: &libc::stat, kind: libc::mode_t) -> bool {
    st.st_mode & libc::S_IFMT == kind
}

/// One entry of a folder listing.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified_ms: i64,
}

/// List a folder: plain files and folders only (no symlinks, no hidden
/// names).
pub fn list(root: &Path, comps: &[String]) -> io::Result<Vec<Entry>> {
    let dir = walk(root, comps, false)?;
    // The descriptor's /proc entry names this exact folder, not a path.
    let mut out = Vec::new();
    for e in std::fs::read_dir(format!("/proc/self/fd/{}", dir.as_raw_fd()))?.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let Ok(st) = stat_at(&dir, &name) else {
            continue;
        };
        let (is_dir, is_file) = (is_type(&st, libc::S_IFDIR), is_type(&st, libc::S_IFREG));
        if !is_dir && !is_file {
            continue;
        }
        out.push(Entry {
            name,
            is_dir,
            size: st.st_size as u64,
            modified_ms: st.st_mtime * 1000 + st.st_mtime_nsec / 1_000_000,
        });
    }
    Ok(out)
}

/// Read a plain text file (one link, at most `max` bytes).
pub fn read_text(root: &Path, comps: &[String], max: u64) -> io::Result<String> {
    let (parents, last) = split(comps)?;
    let dir = walk(root, parents, false)?;
    let n = cstr(last.as_bytes())?;
    // SAFETY: valid descriptor and C string.
    let fd = check(unsafe {
        libc::openat(
            dir.as_raw_fd(),
            n.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    })?;
    // SAFETY: freshly opened descriptor.
    let mut f = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    let meta = f.metadata()?;
    use std::os::unix::fs::MetadataExt;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a plain file",
        ));
    }
    if meta.len() > max {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "file is too large",
        ));
    }
    let mut s = String::new();
    f.read_to_string(&mut s)?;
    Ok(s)
}

/// Write a file atomically (private temp file, then rename), creating
/// missing folders.
pub fn write(root: &Path, comps: &[String], bytes: &[u8]) -> io::Result<()> {
    let (parents, last) = split(comps)?;
    let dir = walk(root, parents, true)?;
    let tmp = format!(".{last}.{}.tmp", crate::util::new_id());
    let (t, n) = (cstr(tmp.as_bytes())?, cstr(last.as_bytes())?);
    // SAFETY: valid descriptor and C strings; O_EXCL creates a new file.
    let fd = check(unsafe {
        libc::openat(
            dir.as_raw_fd(),
            t.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    })?;
    // SAFETY: freshly opened descriptor.
    let mut f = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(fd) });
    let written = f.write_all(bytes).and_then(|_| f.flush());
    drop(f);
    // SAFETY: rename within the same opened folder; a symlink at `last` is
    // replaced, never followed.
    let renamed = written.and_then(|_| {
        check(unsafe { libc::renameat(dir.as_raw_fd(), t.as_ptr(), dir.as_raw_fd(), n.as_ptr()) })
            .map(|_| ())
    });
    if renamed.is_err() {
        // SAFETY: removing our own temp file.
        unsafe { libc::unlinkat(dir.as_raw_fd(), t.as_ptr(), 0) };
    }
    renamed
}

/// Delete a file, or a folder if it is empty. Returns whether it was a
/// folder.
pub fn remove(root: &Path, comps: &[String]) -> io::Result<bool> {
    let (parents, last) = split(comps)?;
    let dir = walk(root, parents, false)?;
    let st = stat_at(&dir, last)?;
    let is_dir = is_type(&st, libc::S_IFDIR);
    let n = cstr(last.as_bytes())?;
    let flags = if is_dir { libc::AT_REMOVEDIR } else { 0 };
    // SAFETY: valid descriptor and C string; unlinkat never follows links.
    check(unsafe { libc::unlinkat(dir.as_raw_fd(), n.as_ptr(), flags) })?;
    Ok(is_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stays_beneath_the_root_without_following_links() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("me");
        let other = tmp.path().join("other");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("secret.md"), "theirs").unwrap();
        let c = |s: &str| components(s).unwrap();
        write(&root, &c("notes/a.md"), b"mine").unwrap();
        assert_eq!(read_text(&root, &c("notes/a.md"), 100).unwrap(), "mine");
        let names: Vec<String> = list(&root, &c("notes"))
            .unwrap()
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, ["a.md"]);
        // A folder swapped for a symlink to someone else's folder.
        std::os::unix::fs::symlink(&other, root.join("swapped")).unwrap();
        assert!(read_text(&root, &c("swapped/secret.md"), 100).is_err());
        assert!(list(&root, &c("swapped")).is_err());
        assert!(write(&root, &c("swapped/new.md"), b"x").is_err());
        assert!(!other.join("new.md").exists());
        assert!(remove(&root, &c("swapped/secret.md")).is_err());
        assert!(other.join("secret.md").exists());
        // A file that is a symlink or a hard link is not read.
        std::os::unix::fs::symlink(other.join("secret.md"), root.join("link.md")).unwrap();
        assert!(read_text(&root, &c("link.md"), 100).is_err());
        std::fs::hard_link(other.join("secret.md"), root.join("hard.md")).unwrap();
        assert!(read_text(&root, &c("hard.md"), 100).is_err());
        // Writing over a symlink replaces the link, never the target.
        write(&root, &c("link.md"), b"new").unwrap();
        assert_eq!(
            std::fs::read_to_string(other.join("secret.md")).unwrap(),
            "theirs"
        );
        assert!(read_text(&root, &c("notes/a.md"), 2).is_err(), "size limit");
        assert!(!remove(&root, &c("notes/a.md")).unwrap());
        assert!(remove(&root, &c("notes")).unwrap());
        assert!(components("../x").is_err() && components("/etc").is_err());
        assert!(components(".git/config").is_err());
    }
}
