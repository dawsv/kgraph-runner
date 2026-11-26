//! Atomic gate writes anchored to the workflow's validated directory.
use serde_json::Value;
use std::{
    fs::File,
    io::{self, Write},
    path::Path,
};

fn denied() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "Expected a regular output inside .kgraph",
    )
}

fn write_contents(file: &mut File, doc: &Value) -> io::Result<()> {
    serde_json::to_writer_pretty(&mut *file, doc)?;
    file.write_all(b"\n")?;
    #[cfg(target_vendor = "apple")]
    {
        use std::os::fd::AsRawFd;
        if unsafe { libc::fsync(file.as_raw_fd()) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(not(target_vendor = "apple"))]
    {
        file.sync_data()
    }
}

#[cfg(unix)]
mod platform {
    use super::*;
    use std::{
        ffi::{CStr, CString},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::ffi::OsStrExt,
        },
        path::Component,
    };

    const DIRECTORY: libc::c_int = libc::O_RDONLY | libc::O_DIRECTORY;

    fn open_at(parent: &File, name: &CStr, flags: libc::c_int) -> io::Result<File> {
        // The parent remains open during the lookup. Never follow the final component.
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                0o600 as libc::c_int,
            )
        };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // openat transferred ownership of this descriptor to the caller.
            Ok(unsafe { File::from_raw_fd(fd) })
        }
    }

    pub(crate) fn open_absolute(path: &Path, directory: bool) -> io::Result<File> {
        let mut parts = path.components().peekable();
        if parts.next() != Some(Component::RootDir) {
            return Err(denied());
        }
        let mut parent = File::open("/")?;
        while let Some(component) = parts.next() {
            let Component::Normal(name) = component else {
                return Err(denied());
            };
            let name = CString::new(name.as_bytes())?;
            let flags = if directory || parts.peek().is_some() {
                DIRECTORY
            } else {
                libc::O_RDONLY
            };
            parent = open_at(&parent, &name, flags)?;
        }
        Ok(parent)
    }

    fn output_parent(root: &Path, path: &Path) -> io::Result<(File, CString)> {
        let relative = path.strip_prefix(root).map_err(|_| denied())?;
        let name = CString::new(relative.file_name().ok_or_else(denied)?.as_bytes())?;
        let mut parent = open_absolute(root, true)?;
        for component in relative.parent().ok_or_else(denied)?.components() {
            let Component::Normal(part) = component else {
                return Err(denied());
            };
            let part = CString::new(part.as_bytes())?;
            parent = match open_at(&parent, &part, DIRECTORY) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    if unsafe { libc::mkdirat(parent.as_raw_fd(), part.as_ptr(), 0o755) } < 0 {
                        let e = io::Error::last_os_error();
                        if e.kind() != io::ErrorKind::AlreadyExists {
                            return Err(e);
                        }
                    }
                    open_at(&parent, &part, DIRECTORY)?
                }
                result => result?,
            };
        }
        Ok((parent, name))
    }

    pub(crate) fn lock(root: &Path, path: &Path) -> io::Result<File> {
        let (parent, name) = output_parent(root, path)?;
        let file = open_at(&parent, &name, libc::O_CREAT | libc::O_RDWR)?;
        if !file.metadata()?.is_file() {
            return Err(denied());
        }
        file.lock()?;
        Ok(file)
    }

    pub(crate) fn write(root: &Path, path: &Path, doc: &Value) -> io::Result<()> {
        let (parent, target) = output_parent(root, path)?;
        let dir = parent.as_raw_fd();
        let temp = CString::new(format!(".kgraph-{:032x}.tmp", rand::random::<u128>()))?;
        let mut file = open_at(
            &parent,
            &temp,
            libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY,
        )?;
        let result = (|| {
            write_contents(&mut file, doc)?;
            let mut stat = std::mem::MaybeUninit::uninit();
            if unsafe {
                libc::fstatat(
                    dir,
                    target.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } == 0
            {
                // fstatat initialized stat and reported the link itself if present.
                if unsafe { stat.assume_init() }.st_mode & libc::S_IFMT != libc::S_IFREG {
                    return Err(denied());
                }
            } else {
                let e = io::Error::last_os_error();
                if e.kind() != io::ErrorKind::NotFound {
                    return Err(e);
                }
            }
            // renameat replaces a directory entry and never follows the destination.
            if unsafe { libc::renameat(dir, temp.as_ptr(), dir, target.as_ptr()) } < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        })();
        if result.is_err() {
            unsafe { libc::unlinkat(dir, temp.as_ptr(), 0) };
        }
        result
    }
}

#[cfg(not(unix))]
mod platform {
    use super::*;
    use crate::interactions::resolve;
    use std::fs::{self, OpenOptions};

    fn check(root: &Path, path: &Path) -> io::Result<()> {
        if !path.starts_with(root) || path == root || resolve(path)? != path || path.is_symlink() {
            return Err(denied());
        }
        Ok(())
    }

    pub(crate) fn lock(root: &Path, path: &Path) -> io::Result<File> {
        check(root, path)?;
        let file = OpenOptions::new()
            .read(true)
            .append(true)
            .create(true)
            .open(path)?;
        if !file.metadata()?.is_file() {
            return Err(denied());
        }
        file.lock()?;
        Ok(file)
    }

    pub(crate) fn write(root: &Path, path: &Path, doc: &Value) -> io::Result<()> {
        check(root, path)?;
        fs::create_dir_all(path.parent().ok_or_else(denied)?)?;
        let temp = path.with_file_name(format!(".kgraph-{:032x}.tmp", rand::random::<u128>()));
        check(root, &temp)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        let result = (|| {
            check(root, &temp)?;
            write_contents(&mut file, doc)?;
            drop(file);
            check(root, &temp)?;
            check(root, path)?;
            fs::rename(&temp, path)
        })();
        if result.is_err() && check(root, &temp).is_ok() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

#[cfg(unix)]
pub(crate) use platform::open_absolute;
pub(crate) use platform::{lock, write};
