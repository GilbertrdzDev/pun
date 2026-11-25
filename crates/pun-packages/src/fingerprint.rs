//! The no-op install, after Cargo's fingerprints: an install that succeeded records what it read and what it wrote,
//! and the next one with the same inputs and untouched outputs skips every write and the autoload dump. The record
//! lives in Pun's cache, keyed by the vendor directory, so `vendor/` holds only what Composer writes there.
//!
//! Inputs: Pun's binary, composer.json as read, composer.lock's file, the command's options, the platform probe,
//! `COMPOSER_ROOT_VERSION`, the git refs the root version comes from, and every file and directory under the root's
//! autoload paths and the path-repository packages. Outputs: vendor/autoload.php, every entry of vendor/composer and
//! the bin directory, each package's directory and the files the native plugins write. A package's files are not
//! read: an installed version is immutable, as Cargo and uv treat a registry package.

use crate::fetch::store;
use crate::php::sha1;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::fmt::Write as _;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// What a no-op install reports again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Recorded {
    pub kept: usize,
    pub classes: usize,
    pub warnings: Vec<String>,
}

/// A record to save once the whole command, scripts included, has succeeded.
#[derive(Debug)]
pub struct Pending {
    pub(crate) file: PathBuf,
    pub(crate) inputs: String,
    pub(crate) outputs: Vec<PathBuf>,
    pub(crate) recorded: Recorded,
}

impl Pending {
    /// Writes the record with the outputs as they are now. A failed write only costs a full install next time.
    pub fn save(&self) {
        let outputs: Vec<Value> = (self.outputs.iter())
            .map(|p| json!([p.to_string_lossy(), stamp(p, false)]))
            .collect();
        let record = json!({
            "inputs": self.inputs,
            "outputs": outputs,
            "kept": self.recorded.kept,
            "classes": self.recorded.classes,
            "warnings": self.recorded.warnings,
        });
        let _ = store(&self.file, record.to_string().as_bytes());
    }
}

/// The record's file for the project whose vendor directory is `vendor`.
pub fn file(cache: &Path, vendor: &str) -> PathBuf {
    cache
        .join("install")
        .join(format!("{}.json", sha1(vendor.as_bytes())))
}

/// The record in `file` when its inputs are `inputs` and none of its outputs changed since.
pub fn fresh(file: &Path, inputs: &str) -> Option<Recorded> {
    let record: Value = serde_json::from_slice(&std::fs::read(file).ok()?).ok()?;
    if record["inputs"].as_str() != Some(inputs) {
        return None;
    }
    for output in record["outputs"].as_array()? {
        if stamp(Path::new(output[0].as_str()?), false) != output[1] {
            return None;
        }
    }
    Some(Recorded {
        kept: usize::try_from(record["kept"].as_u64()?).ok()?,
        classes: usize::try_from(record["classes"].as_u64()?).ok()?,
        warnings: serde_json::from_value(record["warnings"].clone()).ok()?,
    })
}

/// Removes the record, so a run that stops half way leaves none behind.
pub fn forget(file: &Path) {
    let _ = std::fs::remove_file(file);
}

/// A path's identity: kind, inode, size, mode, and modification and change times in nanoseconds; `null` when it is
/// missing. The change time catches an edit whose modification time was set back.
pub fn stamp(path: &Path, follow: bool) -> Value {
    let meta = if follow {
        std::fs::metadata(path)
    } else {
        std::fs::symlink_metadata(path)
    };
    meta.map_or(Value::Null, |m| Value::String(identity(&m)))
}

#[cfg(windows)]
fn identity(m: &std::fs::Metadata) -> String {
    let nanos = |t: std::io::Result<std::time::SystemTime>| {
        t.ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos())
    };
    format!(
        "{}:{}:{}:{}",
        u8::from(m.is_dir()),
        m.len(),
        nanos(m.modified()),
        nanos(m.created())
    )
}

#[cfg(unix)]
fn identity(m: &std::fs::Metadata) -> String {
    format!(
        "{}:{}:{}:{:o}:{}.{}:{}.{}",
        u8::from(m.is_dir()),
        m.ino(),
        m.size(),
        m.mode(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec()
    )
}

/// Appends the stamp of `path` and of everything below it, in name order, following symlinks as the class map
/// scan follows them; a directory reached twice (by device and inode) is listed once.
pub fn tree<S: std::hash::BuildHasher>(
    path: &Path,
    out: &mut String,
    seen: &mut HashSet<(u64, u64), S>,
) {
    let Ok(m) = std::fs::metadata(path) else {
        let _ = writeln!(out, "{}\tnull", path.display());
        return;
    };
    let _ = writeln!(out, "{}\t{}", path.display(), identity(&m));
    #[cfg(unix)]
    let key = (m.dev(), m.ino());
    #[cfg(windows)]
    let key = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        pun_tokens::php_compat::canonicalize(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .hash(&mut hasher);
        (0, hasher.finish())
    };
    if !m.is_dir() || !seen.insert(key) {
        return;
    }
    let mut names: Vec<_> = (std::fs::read_dir(path).into_iter().flatten().flatten())
        .map(|e| e.file_name())
        .collect();
    names.sort();
    for name in names {
        tree(&path.join(name), out, seen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_is_fresh_until_an_input_or_output_changes() {
        let t = std::env::temp_dir().join(format!("pun-fingerprint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        std::fs::create_dir_all(t.join("src/sub")).unwrap();
        std::fs::write(t.join("src/sub/A.php"), "<?php class A {}").unwrap();
        std::fs::write(t.join("out.php"), "x").unwrap();
        let record = file(&t.join("cache"), "/p/vendor");
        assert_ne!(record, file(&t.join("cache"), "/q/vendor"));
        let recorded = Recorded {
            kept: 3,
            classes: 2,
            warnings: vec!["w".into()],
        };
        let pending = Pending {
            file: record.clone(),
            inputs: "in".into(),
            outputs: vec![t.join("out.php"), t.join("gone.php")],
            recorded: recorded.clone(),
        };
        assert_eq!(fresh(&record, "in"), None);
        pending.save();
        assert_eq!(fresh(&record, "in"), Some(recorded));
        assert_eq!(fresh(&record, "other"), None);
        std::fs::write(t.join("gone.php"), "").unwrap();
        assert_eq!(fresh(&record, "in"), None);
        std::fs::remove_file(t.join("gone.php")).unwrap();
        std::fs::write(t.join("out.php"), "y").unwrap();
        assert_eq!(fresh(&record, "in"), None);
        std::fs::write(&record, "{}").unwrap();
        assert_eq!(fresh(&record, "in"), None);
        forget(&record);
        assert!(!record.exists());

        let listing = |t: &Path| {
            let mut out = String::new();
            tree(&t.join("src"), &mut out, &mut HashSet::new());
            out
        };
        #[cfg(unix)]
        std::os::unix::fs::symlink(t.join("src"), t.join("src/sub/loop")).unwrap();
        let before = listing(&t);
        #[cfg(unix)]
        assert_eq!(before.lines().count(), 4, "{before}");
        #[cfg(windows)]
        assert_eq!(before.lines().count(), 3, "{before}");
        assert_eq!(before, listing(&t));
        std::fs::write(t.join("src/sub/B.php"), "<?php class B {}").unwrap();
        assert_ne!(before, listing(&t));
        std::fs::remove_dir_all(&t).unwrap();
    }
}
