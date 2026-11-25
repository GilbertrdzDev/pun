//! The shared package store, after uv, pnpm and Bun: each dist archive is extracted once into Pun's cache, keyed by
//! what identifies its contents (the dist's type, URL, reference and checksum, and the umask its files were created
//! under, since that sets their modes as it sets Composer's), and `vendor/` gets the files linked from there.
//!
//! The link mode decides how a file reaches `vendor/` (uv's `link-mode` names):
//! - `hardlink`, the default, as in pnpm and Bun on Linux: one inode for the store and every project, so the second
//!   project costs no disk on any filesystem. Writing to a file in `vendor/` in place writes the store and every
//!   project linked to it. The store notices such an edit (`place`) and extracts the package again for the next
//!   install, but the projects already linked to the file keep sharing it. Across filesystems (or past the link
//!   limit) files are copied.
//! - `clone`: a copy-on-write clone (`FICLONE` on Linux for btrfs, XFS and bcachefs; `clonefile` on
//!   macOS for APFS). The blocks are shared until either side writes, so an edit in `vendor/` stays in that
//!   project. Where the filesystem can't clone, files are copied.
//! - `copy`: an ordinary copy.
//!
//! Modes are copied from the store, which was extracted as `vendor/` was before the store, so `vendor/` keeps
//! Composer's modes in every link mode.

use crate::error::{Context, Result, bail};
use crate::lock::LockedPackage;
use crate::php::sha256;
use rayon::prelude::*;
use serde_json::Value;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LinkMode {
    Clone,
    #[default]
    Hardlink,
    Copy,
}

impl LinkMode {
    pub fn parse(name: &str) -> Result<LinkMode> {
        Ok(match name {
            "clone" => LinkMode::Clone,
            "hardlink" => LinkMode::Hardlink,
            "copy" => LinkMode::Copy,
            other => bail!("unknown link mode `{other}`: use clone, hardlink or copy"),
        })
    }
}

/// The process's umask, read once before any thread creates files.
static UMASK: LazyLock<u32> = LazyLock::new(umask);

#[cfg(windows)]
fn umask() -> u32 {
    // Windows has no umask. The store key still records one so a cache built on either OS does not collide
    // with the other's layout when the same machine shares a directory (a dual-boot checkout).
    0o022
}

#[cfg(unix)]
#[cfg_attr(
    target_os = "linux",
    expect(clippy::useless_conversion, reason = "mode_t is u16 on macOS")
)]
fn umask() -> u32 {
    // SAFETY: umask only swaps the process's file mode mask; it is set back at once.
    let old = unsafe { libc::umask(0o022) };
    // SAFETY: as above.
    unsafe { libc::umask(old) };
    u32::from(old)
}

pub struct Store {
    root: PathBuf,
    mode: LinkMode,
    /// Set at the first clone the filesystem refuses; the rest are copied.
    no_clone: AtomicBool,
    counter: AtomicUsize,
}

impl Store {
    pub fn new(cache: &Path, mode: LinkMode) -> Store {
        LazyLock::force(&UMASK);
        Store {
            root: cache.join("store").join("v1"),
            mode,
            no_clone: AtomicBool::new(false),
            counter: AtomicUsize::new(0),
        }
    }

    /// The entry a package's dist archive extracts to; `None` for a dist that is not an archive.
    pub fn key(package: &LockedPackage) -> Option<String> {
        let dist = package.dist()?;
        let get = |k: &str| dist.get(k).and_then(Value::as_str).unwrap_or_default();
        if !matches!(get("type"), "zip" | "tar") || get("url").is_empty() {
            return None;
        }
        let id = format!(
            "{}\0{}\0{}\0{}\0{:o}",
            get("type"),
            get("url"),
            get("reference"),
            get("shasum"),
            *UMASK
        );
        Some(sha256(id.as_bytes()))
    }

    /// Links the entry `key` into `target`, extracting it first with `fill` (which unpacks into the directory it is
    /// given) when the store does not have it. True when the store had it.
    pub fn place(
        &self,
        key: &str,
        target: &Path,
        fill: impl FnOnce(&Path) -> Result<()>,
    ) -> Result<bool> {
        let entry = self.root.join(&key[..2]).join(key);
        let parent = target.parent().context("target without a parent")?;
        std::fs::create_dir_all(parent)?;
        let tmp = self.temp(parent);
        std::fs::create_dir(&tmp)?;
        // The entry's own modification time is when it was stored: a file modified later was edited through a
        // hardlink in some project's vendor/, as pnpm's `verify-store-integrity` finds it. Such an entry, or one
        // that can't be read, or isn't a directory, is extracted again.
        let stored = (std::fs::symlink_metadata(&entry)).and_then(|m| {
            if m.is_dir() {
                m.modified()
            } else {
                Err(std::io::ErrorKind::NotADirectory.into())
            }
        });
        let hit = stored.is_ok_and(|t| matches!(self.link_tree(&entry, &tmp, Some(t)), Ok(true)));
        if !hit {
            let _ = std::fs::remove_dir_all(&tmp);
            std::fs::create_dir(&tmp)?;
            self.fill(&entry, fill)?;
            if let Err(e) = self.link_tree(&entry, &tmp, None) {
                let _ = std::fs::remove_dir_all(&tmp);
                return Err(e.into());
            }
        }
        crate::fetch::remove(target)?;
        std::fs::rename(&tmp, target)?;
        Ok(hit)
    }

    /// Extracts the entry with `fill`, setting aside the one there.
    fn fill(&self, entry: &Path, fill: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
        std::fs::create_dir_all(entry.parent().unwrap_or(&self.root))?;
        if std::fs::symlink_metadata(entry).is_ok() {
            let aside = self.temp(&self.root);
            std::fs::rename(entry, &aside)?;
            let _ = std::fs::remove_dir_all(&aside);
        }
        let tmp = self.temp(&self.root);
        let filled = fill(&tmp).and_then(|()| {
            std::fs::File::open(&tmp)?.set_modified(std::time::SystemTime::now())?;
            Ok(())
        });
        if let Err(e) = filled {
            let _ = std::fs::remove_dir_all(&tmp);
            return Err(e);
        }
        // Another process may have stored it meanwhile; either copy is the same.
        if std::fs::rename(&tmp, entry).is_err() {
            let _ = std::fs::remove_dir_all(&tmp);
        }
        Ok(())
    }

    fn temp(&self, dir: &Path) -> PathBuf {
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        dir.join(format!(".pun-{}-{n}.tmp", std::process::id()))
    }

    /// Links the files below `from` into the directory `to`, the entries of each directory in parallel (one
    /// package can hold thousands of files). With `stored`, false as soon as a file was modified after it.
    fn link_tree(
        &self,
        from: &Path,
        to: &Path,
        stored: Option<std::time::SystemTime>,
    ) -> std::io::Result<bool> {
        let entries: Vec<std::fs::DirEntry> = std::fs::read_dir(from)?.collect::<Result<_, _>>()?;
        let intact = (entries.par_iter())
            .map(|e| {
                let (src, dst) = (e.path(), to.join(e.file_name()));
                let kind = e.file_type()?;
                if kind.is_dir() {
                    std::fs::create_dir(&dst)?;
                    return self.link_tree(&src, &dst, stored);
                }
                if kind.is_symlink() {
                    crate::fetch::symlink_or_copy(&std::fs::read_link(&src)?, &dst)?;
                } else if stored
                    .is_some_and(|t| e.metadata().and_then(|m| m.modified()).is_ok_and(|m| m > t))
                {
                    return Ok(false);
                } else {
                    self.link_file(&src, &dst)?;
                }
                Ok(true)
            })
            .try_reduce(|| true, |a, b| Ok(a && b))?;
        // Last, so a read-only directory can still be filled.
        same_mode(&std::fs::metadata(from)?, to)?;
        Ok(intact)
    }

    fn link_file(&self, src: &Path, dst: &Path) -> std::io::Result<()> {
        match self.mode {
            // Across filesystems (or past the link limit) a hardlink can't be made; the file is copied.
            LinkMode::Hardlink if std::fs::hard_link(src, dst).is_ok() => return Ok(()),
            LinkMode::Clone if !self.no_clone.load(Ordering::Relaxed) => match clone(src, dst) {
                Ok(()) => return same_mode(&std::fs::metadata(src)?, dst),
                Err(_) => self.no_clone.store(true, Ordering::Relaxed),
            },
            _ => {}
        }
        std::fs::copy(src, dst).map(|_| ())
    }
}

#[cfg(windows)]
fn same_mode(_meta: &std::fs::Metadata, _path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn same_mode(meta: &std::fs::Metadata, path: &Path) -> std::io::Result<()> {
    let want = meta.mode() & 0o7777;
    if std::fs::metadata(path)?.mode() & 0o7777 != want {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(want))?;
    }
    Ok(())
}

/// A copy-on-write clone of `src` at `dst`, or the error of a filesystem that can't make one.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn clone(_src: &Path, _dst: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "this system has no copy-on-write clone",
    ))
}

#[cfg(target_os = "linux")]
fn clone(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let from = std::fs::File::open(src)?;
    let to = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    // SAFETY: both descriptors are open for the duration of the call.
    if unsafe { libc::ioctl(to.as_raw_fd(), libc::FICLONE, from.as_raw_fd()) } == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    drop(to);
    std::fs::remove_file(dst)?;
    Err(error)
}

#[cfg(target_os = "macos")]
fn clone(src: &Path, dst: &Path) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    // <sys/clonefile.h>; the libc crate declares clonefile but not its flags.
    const CLONE_NOFOLLOW: u32 = 0x0001;
    let c =
        |p: &Path| std::ffi::CString::new(p.as_os_str().as_bytes()).map_err(std::io::Error::other);
    let (s, d) = (c(src)?, c(dst)?);
    // SAFETY: both are valid NUL-terminated paths.
    if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), CLONE_NOFOLLOW) } == 0 {
        return Ok(());
    }
    Err(std::io::Error::last_os_error())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn package(dist: &serde_json::Value) -> LockedPackage {
        LockedPackage(
            json!({"name": "a/b", "version": "1.0.0", "dist": dist})
                .as_object()
                .unwrap()
                .clone(),
        )
    }

    #[test]
    fn keys_name_the_archive() {
        let zip = package(
            &json!({"type": "zip", "url": "https://x/a.zip", "reference": "abc", "shasum": ""}),
        );
        let other = package(
            &json!({"type": "zip", "url": "https://x/a.zip", "reference": "abd", "shasum": ""}),
        );
        assert_eq!(Store::key(&zip).unwrap().len(), 64);
        assert_ne!(Store::key(&zip), Store::key(&other));
        assert_eq!(
            Store::key(&package(&json!({"type": "path", "url": "../p"}))),
            None
        );
        assert_eq!(Store::key(&package(&json!({"type": "tar"}))), None);
        assert_eq!(LinkMode::parse("hardlink").unwrap(), LinkMode::default());
        assert_eq!(LinkMode::parse("copy").unwrap(), LinkMode::Copy);
        assert_eq!(LinkMode::parse("clone").unwrap(), LinkMode::Clone);
        assert_eq!(
            LinkMode::parse("symlink").unwrap_err().to_string(),
            "unknown link mode `symlink`: use clone, hardlink or copy"
        );
    }

    /// Each mode gives the same files, modes and symlinks; only hardlinks share the store's inodes, and a failed
    /// fill leaves neither an entry nor a target behind.
    #[cfg(unix)]
    #[test]
    fn every_mode_links_the_same_tree() {
        let t = std::env::temp_dir().join(format!("pun-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let fill = |dir: &Path| -> Result<()> {
            std::fs::create_dir_all(dir.join("bin"))?;
            std::fs::write(dir.join("bin/tool"), "#!/bin/sh\n")?;
            std::fs::set_permissions(dir.join("bin/tool"), std::fs::Permissions::from_mode(0o755))?;
            std::fs::write(dir.join("README"), "hi")?;
            std::os::unix::fs::symlink("README", dir.join("link"))?;
            Ok(())
        };
        let mut filled = 0;
        for mode in [LinkMode::Clone, LinkMode::Hardlink, LinkMode::Copy] {
            let store = Store::new(&t.join("cache"), mode);
            let target = t.join(format!("{mode:?}/vendor/a/b"));
            std::fs::create_dir_all(&target).unwrap();
            std::fs::write(target.join("stale"), "").unwrap();
            let hit = store
                .place("ab12", &target, |d| {
                    filled += 1;
                    fill(d)
                })
                .unwrap();
            assert_eq!(hit, mode != LinkMode::Clone, "{mode:?}");
            assert!(!target.join("stale").exists());
            let meta = |p: &str| std::fs::metadata(target.join(p)).unwrap();
            assert_eq!(meta("bin/tool").mode() & 0o777, 0o755);
            assert_eq!(
                std::fs::read_link(target.join("link")).unwrap(),
                Path::new("README")
            );
            assert_eq!(
                std::fs::read_to_string(target.join("README")).unwrap(),
                "hi"
            );
            assert_eq!(
                meta("README").nlink() > 1,
                mode == LinkMode::Hardlink,
                "{mode:?}"
            );
        }
        assert_eq!(filled, 1);

        // An in-place edit through a hardlink reaches the store; the next install extracts the package again.
        let edited = t.join("Hardlink/vendor/a/b/README");
        std::fs::OpenOptions::new()
            .write(true)
            .open(&edited)
            .and_then(|mut f| std::io::Write::write_all(&mut f, b"HI"))
            .unwrap();
        let store = Store::new(&t.join("cache"), LinkMode::Hardlink);
        let target = t.join("again/vendor/a/b");
        let hit = store
            .place("ab12", &target, |d| {
                filled += 1;
                fill(d)
            })
            .unwrap();
        assert!(!hit);
        assert_eq!(filled, 2);
        assert_eq!(
            std::fs::read_to_string(target.join("README")).unwrap(),
            "hi"
        );
        assert_eq!(std::fs::read_to_string(&edited).unwrap(), "HI");
        assert!(
            store
                .place("ab12", &t.join("third/a/b"), |_| unreachable!())
                .unwrap()
        );
        let store = Store::new(&t.join("cache"), LinkMode::Copy);
        let target = t.join("failed/a/b");
        let err = store
            .place("cd34", &target, |_| bail!("no archive"))
            .unwrap_err();
        assert_eq!(err.to_string(), "no archive");
        assert!(!target.exists() && !t.join("cache/store/v1/cd/cd34").exists());
        std::fs::remove_dir_all(&t).unwrap();
    }

    /// A directory keeps the mode it was extracted with (a 0700 or a read-only 0555 one too); another
    /// process storing the same entry first wins without an error; an entry that can't be read fails the install
    /// and leaves no temporary directory in `vendor/`.
    #[cfg(unix)]
    #[test]
    fn modes_races_and_unreadable_entries() {
        let t = std::env::temp_dir().join(format!("pun-store-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let store = Store::new(&t.join("cache"), LinkMode::Copy);
        let entry = t.join("cache/store/v1/ef/ef56");
        let vendor = t.join("vendor");
        let hit = store
            .place("ef56", &vendor.join("a/b"), |dir| {
                // The other process: its entry is renamed in while this one is still extracting.
                std::fs::create_dir_all(entry.join("private"))?;
                std::fs::create_dir_all(entry.join("locked"))?;
                std::fs::write(entry.join("locked/file"), "x")?;
                std::fs::set_permissions(
                    entry.join("locked"),
                    std::fs::Permissions::from_mode(0o555),
                )?;
                std::fs::set_permissions(
                    entry.join("private"),
                    std::fs::Permissions::from_mode(0o700),
                )?;
                std::fs::create_dir_all(dir.join("mine"))?;
                Ok(())
            })
            .unwrap();
        assert!(!hit);
        let mode = std::fs::metadata(vendor.join("a/b/private"))
            .unwrap()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
        let mode = std::fs::metadata(vendor.join("a/b/locked")).unwrap().mode();
        assert_eq!(mode & 0o777, 0o555);
        assert_eq!(
            std::fs::read_to_string(vendor.join("a/b/locked/file")).unwrap(),
            "x"
        );
        assert!(!vendor.join("a/b/mine").exists());
        let leftovers = |dir: &Path| {
            (std::fs::read_dir(dir).unwrap())
                .filter(|e| {
                    e.as_ref()
                        .unwrap()
                        .file_name()
                        .to_string_lossy()
                        .ends_with(".tmp")
                })
                .count()
        };
        assert_eq!(leftovers(&t.join("cache/store/v1")), 0);

        for dir in [entry.join("locked"), vendor.join("a/b/locked")] {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        // An entry that can't be read is extracted again; a vendor/ that can't be written fails, leaving nothing.
        let unreadable = t.join("cache/store/v1/gh/gh78/private");
        std::fs::create_dir_all(&unreadable).unwrap();
        std::fs::set_permissions(&unreadable, std::fs::Permissions::from_mode(0o000)).unwrap();
        let hit = store.place("gh78", &vendor.join("c/d"), |d| {
            std::fs::create_dir_all(d.join("fresh"))?;
            Ok(())
        });
        assert!(!hit.unwrap() && vendor.join("c/d/fresh").is_dir());
        // A fresh extraction that can't be read back fails too.
        let err = store.place("ij90", &vendor.join("c/f"), |d| {
            std::fs::create_dir_all(d.join("closed"))?;
            std::fs::set_permissions(d.join("closed"), std::fs::Permissions::from_mode(0o000))?;
            Ok(())
        });
        assert_eq!(
            err.unwrap_err().to_string(),
            "Permission denied (os error 13)"
        );
        assert!(!vendor.join("c/f").exists() && leftovers(&vendor.join("c")) == 0);
        let closed = t.join("cache/store/v1/ij/ij90/closed");
        std::fs::set_permissions(closed, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(vendor.join("c"), std::fs::Permissions::from_mode(0o555)).unwrap();
        let err = store.place("gh78", &vendor.join("c/e"), |_| unreachable!());
        assert_eq!(
            err.unwrap_err().to_string(),
            "Permission denied (os error 13)"
        );
        assert!(!vendor.join("c/e").exists() && leftovers(&vendor.join("c")) == 0);
        std::fs::set_permissions(vendor.join("c"), std::fs::Permissions::from_mode(0o755)).unwrap();
        for dir in std::fs::read_dir(t.join("cache/store/v1")).unwrap() {
            let dir = dir.unwrap().path();
            if dir.to_string_lossy().ends_with(".tmp") {
                std::fs::set_permissions(
                    dir.join("private"),
                    std::fs::Permissions::from_mode(0o755),
                )
                .unwrap();
            }
        }
        std::fs::remove_dir_all(&t).unwrap();
    }
}
