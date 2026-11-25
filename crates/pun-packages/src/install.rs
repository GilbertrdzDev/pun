use crate::Env;
use crate::autoload::{self, Apcu, PlatformCheck, Scan};
use crate::bins;
use crate::error::{Context, Error, Result, bail};
use crate::fetch::{Fetcher, link_path, remove, unpack};
use crate::fingerprint::{self, Pending, Recorded};
use crate::installed::{Installed, installed_json, installed_php, read_installed, root};
use crate::lock::{Lock, LockedPackage, content_hash};
use crate::paths::{is_absolute, normalize};
use crate::php::{md5, sha1};
use crate::plugin_host;
use crate::plugins;
use crate::resolver::platform::probe_cached;
use crate::store::{LinkMode, Store};
use crate::verify::verify;
use crate::version::{IgnorePlatform, is_platform};
use indexmap::IndexMap;
use pcre2::bytes::Regex;
use rayon::prelude::*;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub no_dev: bool,
    pub scan: Scan,
    pub apcu: Apcu,
    pub no_autoloader: bool,
    pub no_plugins: bool,
    pub ignore_platform: IgnorePlatform,
    /// The PHP the lock is checked against before anything is installed, as `composer install` checks it; `None`
    /// skips the check (an update has just solved the lock on this platform).
    pub php: Option<String>,
    /// Pun's cache directory: the platform probe, the no-op fingerprints and the package store live here.
    pub cache: Option<PathBuf>,
    /// How files reach `vendor/` from the store.
    pub link: LinkMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Added {
    pub name: String,
    pub version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Updated {
    pub name: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub installed: Vec<Added>,
    pub updated: Vec<Updated>,
    pub removed: Vec<String>,
    pub kept: usize,
    pub cached: usize,
    pub classes: usize,
    pub stale_lock: bool,
    pub warnings: Vec<String>,
    /// The fingerprint matched: nothing was written.
    pub fresh: bool,
    /// Composer's problem report when the lock can't be installed on this platform; nothing was installed.
    pub incompatible: Option<String>,
    /// The record to save once the command's scripts have run.
    pub fingerprint: Option<Pending>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    pub vendor: String,
    pub bin: String,
}

fn config_str<'a>(config: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    config.get(key).and_then(Value::as_str)
}

fn config_bool(config: &Map<String, Value>, key: &str) -> bool {
    config.get(key) == Some(&Value::Bool(true))
}

fn config_path(base: &str, value: &str, vendor: &str) -> String {
    let value = value.replace("{$vendor-dir}", vendor);
    normalize(&if is_absolute(&value) {
        value
    } else {
        format!("{base}/{value}")
    })
}

pub fn dirs(base: &str, manifest: &Map<String, Value>) -> Dirs {
    let config = manifest.get("config").and_then(Value::as_object);
    let get = |k: &str| config.and_then(|c| config_str(c, k));
    let vendor = config_path(base, get("vendor-dir").unwrap_or("vendor"), "");
    let bin = config_path(base, get("bin-dir").unwrap_or("{$vendor-dir}/bin"), &vendor);
    Dirs { vendor, bin }
}

struct Project<'a> {
    dir: PathBuf,
    /// `dir` as the generated PHP files spell it.
    dir_str: String,
    manifest: &'a Map<String, Value>,
    dirs: Dirs,
}

pub fn install(
    base: &Path,
    manifest: &Map<String, Value>,
    lock: &Lock,
    fetcher: &Fetcher,
    o: &Options,
    env: Env,
    events: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<Report> {
    let dir = pun_tokens::php_compat::canonicalize(base)
        .with_context(|| format!("can't open {}", base.display()))?;
    let dir_str = dir.to_string_lossy().replace('\\', "/");
    let project = Project {
        dirs: dirs(&dir_str, manifest),
        dir,
        dir_str,
        manifest,
    };
    let mut report = Report {
        stale_lock: content_hash(manifest) != lock.content_hash,
        ..Report::default()
    };
    let mut packages = locked_packages(lock, &project.dirs.vendor, o.no_dev)?;
    let probe = match &o.php {
        Some(php) => Some(probe_cached(php, o.cache.as_deref())?),
        None => None,
    };
    let inputs = fingerprint_inputs(&project, &packages, o, probe.as_ref(), env);
    let record = (o.cache.as_deref()).map(|c| fingerprint::file(c, &project.dirs.vendor));
    if let (Some(file), Some(inputs)) = (&record, &inputs)
        && let Some(recorded) = fingerprint::fresh(file, inputs)
    {
        if !o.no_autoloader {
            events("pre-autoload-dump")?;
            events("post-autoload-dump")?;
        }
        report.kept = recorded.kept;
        report.classes = recorded.classes;
        report.warnings = recorded.warnings;
        report.fresh = true;
        return Ok(report);
    }
    if let Some(file) = &record {
        fingerprint::forget(file);
    }
    if let Some(probe) = &probe
        && let Some(problems) = verify_once(o, manifest, lock, probe)?
    {
        report.incompatible = Some(problems);
        return Ok(report);
    }
    let before = previous(Path::new(&project.dirs.vendor))?;
    let isolate = o.cache.is_some() && o.link != LinkMode::Hardlink;
    let todo = compare(&packages, &before, isolate, &mut report);
    remove_stale(&before, &packages, &todo, &project.dirs, &mut report)?;
    for p in todo.iter().map(|&i| &packages[i]) {
        if let Some(path) = &p.path {
            confined(&project.dirs.vendor, Path::new(path))
                .with_context(|| format!("installing {}", p.name()))?;
        }
    }
    let store = (o.cache.as_deref()).map(|c| Store::new(c, o.link));
    download(
        fetcher,
        store.as_ref(),
        &packages,
        &todo,
        base,
        env,
        &mut report,
    )?;
    write_repository(&project, &packages, !o.no_dev, env)?;
    link_bins(&packages, &project.dirs, &mut report)?;
    let repository: Vec<String> = (repository_order(&packages, &before, &todo).into_iter())
        .map(|i| packages[i].name())
        .collect();
    packages.sort_by_key(Installed::name);
    let mut written = vec![];
    if !o.no_autoloader {
        let repository: Vec<&Installed> = (repository.iter())
            .filter_map(|n| {
                let i = packages.binary_search_by(|p| p.name().cmp(n)).ok()?;
                Some(&packages[i])
            })
            .collect();
        written = dump_autoloader(
            &project,
            Some(&lock.content_hash),
            &packages,
            &repository,
            o,
            events,
            &mut report,
        )?;
    }
    if let (Some(file), Some(inputs)) = (record, inputs) {
        report.fingerprint = Some(Pending {
            file,
            inputs,
            outputs: outputs(&project.dirs, &packages, written),
            recorded: Recorded {
                kept: packages.len(),
                classes: report.classes,
                warnings: report.warnings.clone(),
            },
        });
    }
    Ok(report)
}

/// What `dump-autoload` reports.
#[derive(Debug, Default)]
pub struct DumpReport {
    pub classes: usize,
    pub warnings: Vec<String>,
    pub dev_mode: bool,
    /// A package installed.json lists is missing from `vendor/` (Composer then exits 1).
    pub missing: bool,
}

/// `composer dump-autoload`: the autoloader of the packages installed.json lists, in dev mode when `dev` says so or,
/// without it, when the last install was (installed.json's `dev`), between the dump events.
pub fn dump_autoload(
    base: &Path,
    manifest: &Map<String, Value>,
    lock: Option<&Lock>,
    o: &Options,
    dev: Option<bool>,
    events: &mut dyn FnMut(&str, bool) -> Result<()>,
) -> Result<DumpReport> {
    let dir = pun_tokens::php_compat::canonicalize(base)
        .with_context(|| format!("can't open {}", base.display()))?;
    let dir_str = dir.to_string_lossy().replace('\\', "/");
    let project = Project {
        dirs: dirs(&dir_str, manifest),
        dir,
        dir_str,
        manifest,
    };
    let vendor = &project.dirs.vendor;
    let text = std::fs::read(format!("{vendor}/composer/installed.json")).unwrap_or_default();
    let json: Value = serde_json::from_slice(&text).unwrap_or(Value::Null);
    let dev_names: Vec<&str> = (json.get("dev-package-names").and_then(Value::as_array))
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let dev_mode = dev.unwrap_or(json.get("dev") == Some(&Value::Bool(true)));
    let mut report = DumpReport {
        dev_mode,
        ..DumpReport::default()
    };
    let mut repository: Vec<Installed> = vec![];
    for p in read_installed(Path::new(vendor))? {
        let path = (p.get("install-path").and_then(Value::as_str))
            .map(|ip| normalize(&format!("{vendor}/composer/{ip}")));
        report.missing |= path.as_ref().is_some_and(|p| !Path::new(p).exists());
        repository.push(Installed {
            dev: dev_names.contains(&p.name().as_str()),
            package: p,
            path,
        });
    }
    let mut packages = repository.clone();
    packages.sort_by_key(Installed::name);
    let order: Vec<&Installed> = repository.iter().collect();
    let o = Options {
        no_dev: !dev_mode,
        ..o.clone()
    };
    let mut install = Report::default();
    let mut events = |event: &str| events(event, dev_mode);
    dump_autoloader(
        &project,
        lock.map(|l| l.content_hash.as_str()),
        &packages,
        &order,
        &o,
        &mut events,
        &mut install,
    )?;
    report.classes = install.classes;
    report.warnings = install.warnings;
    Ok(report)
}

/// The no-op fingerprint's inputs (see `fingerprint`), or `None` when the next install can't be skipped: a random
/// APCu prefix changes on every dump, a `pre-autoload-dump` script could change what the dump reads after the
/// check, a plugin the host runs could listen to anything, and a `.git` file (a worktree or submodule) keeps its
/// refs elsewhere.
fn fingerprint_inputs(
    project: &Project,
    packages: &[Installed],
    o: &Options,
    probe: Option<&Value>,
    env: Env,
) -> Option<String> {
    let manifest = project.manifest;
    let empty = Map::new();
    let config = (manifest.get("config").and_then(Value::as_object)).unwrap_or(&empty);
    let random_apcu = match o.apcu {
        Apcu::RandomPrefix => true,
        Apcu::Off => config_bool(config, "apcu-autoloader"),
        Apcu::Prefix(_) => false,
    };
    let pre_dump = (manifest.get("scripts")).is_some_and(|s| s.get("pre-autoload-dump").is_some());
    let hosted = !o.no_plugins
        && (packages.iter()).any(|p| plugin_host::hosted(&p.name(), p.package.kind(), config));
    if random_apcu || pre_dump || hosted {
        return None;
    }
    let mut s = format!("pun {}\n", env!("CARGO_PKG_VERSION"));
    let exe = std::env::current_exe().unwrap_or_default();
    let _ = writeln!(s, "{}", fingerprint::stamp(&exe, true));
    let _ = writeln!(
        s,
        "{}\n{}\n{}",
        project.dir_str,
        Value::Object(manifest.clone()),
        fingerprint::stamp(&project.dir.join("composer.lock"), true)
    );
    let _ = writeln!(
        s,
        "{} {:?} {:?} {} {} {:?} {:?}",
        o.no_dev, o.scan, o.apcu, o.no_autoloader, o.no_plugins, o.ignore_platform, o.link
    );
    let _ = writeln!(s, "{}", probe.map_or_else(String::new, Value::to_string));
    let root_version = env("COMPOSER_ROOT_VERSION");
    let _ = writeln!(s, "{root_version:?}");
    let mut seen = HashSet::new();
    if manifest.get("version").is_none() && root_version.is_none_or(|v| v.is_empty()) {
        let git = project
            .dir
            .ancestors()
            .map(|d| d.join(".git"))
            .find(|g| g.exists());
        if let Some(git) = git {
            if !git.is_dir() {
                return None;
            }
            for name in ["HEAD", "packed-refs", "refs"] {
                fingerprint::tree(&git.join(name), &mut s, &mut seen);
            }
        }
    }
    let mut roots: Vec<PathBuf> = vec![];
    let kinds = ["autoload", "autoload-dev"];
    for kind in &kinds[..if o.no_dev { 1 } else { 2 }] {
        autoload_paths(manifest.get(*kind), &mut |path| {
            roots.push(project.dir.join(path));
        });
    }
    // A path repository's source: what a symlinked package's classes are read from. The directory in vendor/ is an
    // output, and doesn't exist yet before the first install.
    for p in packages {
        let dist = p.package.dist();
        let get = |k: &str| dist.and_then(|d| d.get(k)).and_then(Value::as_str);
        if let (Some("path"), Some(url)) = (get("type"), get("url")) {
            roots.push(project.dir.join(url));
        }
    }
    for root in roots {
        fingerprint::tree(&root, &mut s, &mut seen);
    }
    Some(sha1(s.as_bytes()))
}

/// Calls `f` with each path an `autoload` section reads. `exclude-from-classmap` holds patterns, not paths (the
/// manifest, hashed whole, covers them), so a pattern such as `/` doesn't send the walk through the whole disk.
fn autoload_paths(section: Option<&Value>, f: &mut dyn FnMut(&str)) {
    for (kind, value) in section.and_then(Value::as_object).into_iter().flatten() {
        if kind != "exclude-from-classmap" {
            leaves(Some(value), f);
        }
    }
}

/// Calls `f` with every string in `value`, however deep.
fn leaves(value: Option<&Value>, f: &mut dyn FnMut(&str)) {
    match value {
        Some(Value::String(s)) => f(s),
        Some(Value::Array(items)) => items.iter().for_each(|v| leaves(Some(v), f)),
        Some(Value::Object(m)) => m.values().for_each(|v| leaves(Some(v), f)),
        _ => {}
    }
}

/// What an install writes, for the fingerprint: vendor/autoload.php, vendor/composer and the bin directory with
/// each of their entries, each package's directory, and the files in `written`.
fn outputs(dirs: &Dirs, packages: &[Installed], written: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from(format!("{}/autoload.php", dirs.vendor))];
    for dir in [format!("{}/composer", dirs.vendor), dirs.bin.clone()] {
        let dir = PathBuf::from(dir);
        let mut entries: Vec<PathBuf> = (std::fs::read_dir(&dir).into_iter().flatten().flatten())
            .map(|e| e.path())
            .collect();
        entries.sort();
        out.push(dir);
        out.extend(entries);
    }
    out.extend(
        packages
            .iter()
            .filter_map(|p| p.path.as_ref().map(PathBuf::from)),
    );
    out.extend(written);
    out
}

/// The lock's packages, and the dev ones unless `no_dev`, each with the directory it installs in.
fn locked_packages(lock: &Lock, vendor: &str, no_dev: bool) -> Result<Vec<Installed>> {
    static NAME: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"^[a-z0-9](?:[_.-]?[a-z0-9]++)*+/[a-z0-9](?:(?:[_.]|-{1,2})?[a-z0-9]++)*+$")
            .unwrap()
    });
    let packages: Vec<Installed> = (lock.packages.iter().map(|p| (p, false)))
        .chain(
            lock.packages_dev
                .iter()
                .filter(|_| !no_dev)
                .map(|p| (p, true)),
        )
        .map(|(p, dev)| Installed {
            package: p.clone(),
            path: (p.kind() != Some("metapackage")).then(|| format!("{vendor}/{}", p.name())),
            dev,
        })
        .collect();
    for p in &packages {
        let name = p.name();
        if !NAME.is_match(name.as_bytes())? {
            bail!("composer.lock has a package named `{name}`, which is not a valid package name");
        }
    }
    Ok(packages)
}

/// installed.json's packages, in its order.
fn previous(vendor: &Path) -> Result<IndexMap<String, LockedPackage>> {
    Ok((read_installed(vendor)?.into_iter())
        .filter(|p| p.get("name").is_some_and(Value::is_string))
        .map(|p| (p.name(), p))
        .collect())
}

/// Counts each package as kept, updated or added; the indexes of the ones to place. With `isolate` (the clone and
/// copy link modes), a kept package whose files are still hardlinked, from an install in the hardlink mode, is
/// placed again so that its files stop being shared.
fn compare(
    packages: &[Installed],
    before: &IndexMap<String, LockedPackage>,
    isolate: bool,
    report: &mut Report,
) -> Vec<usize> {
    let mut todo = vec![];
    for (i, p) in packages.iter().enumerate() {
        let name = p.name();
        let present = p.path.as_ref().is_none_or(|path| Path::new(path).exists());
        match before.get(&name) {
            Some(old)
                if present
                    && old.reference() == p.package.reference()
                    && old.version() == p.package.version() =>
            {
                report.kept += 1;
                if isolate
                    && p.path
                        .as_ref()
                        .is_some_and(|path| hardlinked(Path::new(path)))
                {
                    todo.push(i);
                }
            }
            Some(old) => {
                report.updated.push(Updated {
                    name,
                    from: old.version().to_string(),
                    to: p.package.version().to_string(),
                });
                todo.push(i);
            }
            None => {
                report.installed.push(Added {
                    name,
                    version: p.package.version().to_string(),
                });
                todo.push(i);
            }
        }
    }
    todo
}

/// Whether the first file found below `dir` has another hardlink: the store links every file of a package the same
/// way. A symlinked `dir` (a path repository's) is not a store's.
/// `Metadata::number_of_links` is unstable on Windows, so a store install is not recognized by its link count.
/// Files are still hardlinked when the filesystem allows it (`std::fs::hard_link` in the store).
#[cfg(windows)]
fn hardlinked(_dir: &Path) -> bool {
    false
}

#[cfg(unix)]
fn hardlinked(dir: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if !std::fs::symlink_metadata(dir).is_ok_and(|m| m.is_dir()) {
        return false;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    let mut dirs = vec![];
    for e in entries.flatten() {
        match e.file_type() {
            Ok(t) if t.is_file() => return e.metadata().is_ok_and(|m| m.nlink() > 1),
            Ok(t) if t.is_dir() => dirs.push(e.path()),
            _ => {}
        }
    }
    dirs.iter().any(|d| hardlinked(d))
}

/// Refuses `dir`, below `vendor`, when an existing directory on the way to it is a symlink that leads outside
/// `vendor`, so that placing or removing a package never reaches another directory. `dir` itself may be a symlink
/// (a path repository's): it is replaced or unlinked, not followed.
fn confined(vendor: &str, dir: &Path) -> Result<()> {
    let Ok(root) = pun_tokens::php_compat::canonicalize(vendor) else {
        return Ok(());
    };
    for at in dir.ancestors().skip(1) {
        if at == Path::new(vendor) || !at.starts_with(vendor) {
            break;
        }
        if std::fs::symlink_metadata(at).is_ok() {
            if !pun_tokens::php_compat::canonicalize(at).is_ok_and(|real| real.starts_with(&root)) {
                bail!(
                    "{} is a symlink that leads outside {vendor}, so Pun won't write or remove {} through it",
                    at.display(),
                    dir.display()
                );
            }
            break;
        }
    }
    Ok(())
}

/// Removes the binaries of the packages that change or go, and the directories of the ones that go.
fn remove_stale(
    before: &IndexMap<String, LockedPackage>,
    packages: &[Installed],
    todo: &[usize],
    dirs: &Dirs,
    report: &mut Report,
) -> Result<()> {
    let locked: Vec<String> = packages.iter().map(Installed::name).collect();
    for (name, old) in before {
        let gone = !locked.contains(name);
        let changed = todo.iter().any(|&i| packages[i].name() == *name);
        if gone || changed {
            for bin in old.bins() {
                remove_file(&Path::new(&dirs.bin).join(bin.rsplit('/').next().unwrap_or(bin)))?;
            }
        }
        if gone {
            if let Some(dir) = old.get("install-path").and_then(Value::as_str) {
                let dir = normalize(&format!("{}/composer/{dir}", dirs.vendor));
                if dir.starts_with(&format!("{}/", dirs.vendor)) {
                    confined(&dirs.vendor, Path::new(&dir))?;
                    remove(Path::new(&dir))?;
                }
            }
            report.removed.push(name.clone());
        }
    }
    Ok(())
}

fn download(
    fetcher: &Fetcher,
    store: Option<&Store>,
    packages: &[Installed],
    todo: &[usize],
    base: &Path,
    env: Env,
    report: &mut Report,
) -> Result<()> {
    let cores = std::thread::available_parallelism().map_or(8, usize::from);
    let threads = download_threads(env("COMPOSER_MAX_PARALLEL_HTTP"), cores);
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(Error::msg)?;
    let results: Vec<Result<bool>> = pool.install(|| {
        (todo.par_iter())
            .map(|&i| {
                let p = &packages[i];
                let Some(path) = &p.path else {
                    return Ok(false);
                };
                place(fetcher, store, &p.package, base, Path::new(path))
                    .with_context(|| format!("installing {}", p.name()))
            })
            .collect()
    });
    let mut errors = vec![];
    for r in results {
        match r {
            Ok(cached) => report.cached += usize::from(cached),
            Err(e) => errors.push(format!("{e:#}")),
        }
    }
    if !errors.is_empty() {
        bail!("{}", errors.join("\n"));
    }
    Ok(())
}

fn write_repository(
    project: &Project,
    packages: &[Installed],
    dev_mode: bool,
    env: Env,
) -> Result<()> {
    let vendor = &project.dirs.vendor;
    let repo = format!("{vendor}/composer");
    confined(vendor, Path::new(&format!("{repo}/installed.json")))?;
    std::fs::create_dir_all(&repo)?;
    write_if_changed(
        Path::new(&format!("{repo}/installed.json")),
        installed_json(packages, vendor, dev_mode).as_bytes(),
    )?;
    let root_pkg = root(project.manifest, &project.dir, env);
    write_if_changed(
        Path::new(&format!("{repo}/installed.php")),
        installed_php(&root_pkg, packages, vendor, &project.dir_str, dev_mode).as_bytes(),
    )?;
    write_if_changed(
        Path::new(&format!("{repo}/InstalledVersions.php")),
        include_bytes!("../assets/InstalledVersions.php"),
    )
}

/// Writes the bin proxies in Composer's install order; the first package to claim a name keeps it.
fn link_bins(packages: &[Installed], dirs: &Dirs, report: &mut Report) -> Result<()> {
    let mut claimed: HashSet<String> = HashSet::new();
    for i in install_order(packages) {
        let p = &packages[i];
        let Some(path) = &p.path else { continue };
        let names: Vec<String> = p.package.bins().map(String::from).collect();
        let (proxies, warnings) = bins::proxies(&p.name(), path, &names, &dirs.bin, &dirs.vendor);
        report.warnings.extend(warnings);
        for proxy in proxies {
            if !claimed.insert(proxy.name.clone()) {
                report.warnings.push(format!(
                    "Skipped installation of bin {} for package {}: name conflicts with an existing file",
                    proxy.bin,
                    p.name()
                ));
                continue;
            }
            bins::write(&proxy, Path::new(&dirs.bin))?;
        }
    }
    Ok(())
}

/// The autoloader's options: the command's, raised by the root's `config`.
fn autoload_options(
    config: &Map<String, Value>,
    vendor: &str,
    content_hash: Option<&str>,
    o: &Options,
    dev_mode: bool,
) -> Result<autoload::Options> {
    static SUFFIX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"ComposerAutoloaderInit([^:\s]+)::").unwrap());
    let platform_check = match config.get("platform-check") {
        Some(Value::Bool(false)) => PlatformCheck::Off,
        Some(Value::Bool(true)) => PlatformCheck::Full,
        _ => PlatformCheck::PhpOnly,
    };
    let existing = match std::fs::read_to_string(format!("{vendor}/autoload.php")) {
        Ok(text) => (SUFFIX.captures(text.as_bytes())?)
            .map(|caps| String::from_utf8_lossy(&caps[1]).into_owned()),
        Err(_) => None,
    };
    // The suffix ends PHP class names, so only identifier characters (ASCII letters, digits, `_`, and bytes from
    // 0x80, as PHP reads them) can go in; Composer pastes any text in and writes an autoloader PHP can't parse.
    let identifier = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || !c.is_ascii())
    };
    // Composer reads an empty suffix as none (`Config::get`).
    let configured = config_str(config, "autoloader-suffix").filter(|s| !s.is_empty());
    if let Some(s) = configured.filter(|s| !identifier(s)) {
        bail!(
            "config.autoloader-suffix \"{s}\" can't end a PHP class name: use letters, digits and _"
        );
    }
    let suffix = configured
        .map(String::from)
        .or(existing.filter(|s| identifier(s)))
        .or_else(|| content_hash.filter(|s| identifier(s)).map(String::from))
        .unwrap_or_else(|| {
            // Composer's `bin2hex(random_bytes(16))` for a project without a lock.
            md5(format!("{:?}{}", std::time::SystemTime::now(), std::process::id()).as_bytes())
        });
    let configured = if config_bool(config, "classmap-authoritative") {
        Scan::Authoritative
    } else if config_bool(config, "optimize-autoloader") {
        Scan::Optimized
    } else {
        Scan::Classmap
    };
    let apcu = match &o.apcu {
        Apcu::Off if config_bool(config, "apcu-autoloader") => Apcu::RandomPrefix,
        apcu => apcu.clone(),
    };
    Ok(autoload::Options {
        scan: o.scan.max(configured),
        apcu,
        dev_mode,
        platform_check,
        ignore_platform: o.ignore_platform.clone(),
        use_include_path: config_bool(config, "use-include-path"),
        prepend: config.get("prepend-autoloader") != Some(&Value::Bool(false)),
        suffix,
        class_cache: o.cache.as_ref().map(|c| c.join("classes").join("v1")),
    })
}

/// Writes the autoloader between the `pre-autoload-dump` and `post-autoload-dump` events, then the files of
/// the plugins Pun runs, which see the packages in `repository`'s order.
fn dump_autoloader(
    project: &Project,
    content_hash: Option<&str>,
    packages: &[Installed],
    repository: &[&Installed],
    o: &Options,
    events: &mut dyn FnMut(&str) -> Result<()>,
    report: &mut Report,
) -> Result<Vec<PathBuf>> {
    let empty = Map::new();
    let config = (project.manifest.get("config"))
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let vendor = &project.dirs.vendor;
    let options = autoload_options(config, vendor, content_hash, o, !o.no_dev)?;
    events("pre-autoload-dump")?;
    let kept = (o.cache.as_deref()).zip(dump_key(project, packages, &options));
    let kept = kept.map(|(cache, key)| {
        let file =
            (cache.join("autoload").join("v1")).join(format!("{}.txt", sha1(vendor.as_bytes())));
        (file, key)
    });
    let dumped = if let Some(dumped) =
        (kept.as_ref()).and_then(|(file, key)| autoload::Dumped::load(file, key))
    {
        dumped
    } else {
        let dumped = autoload::dump(
            &project.dir_str,
            vendor,
            project.manifest,
            packages,
            &options,
        )?;
        if let Some((file, key)) = &kept {
            dumped.save(file, key);
        }
        dumped
    };
    report.classes = dumped.classes;
    report.warnings.extend(dumped.warnings);
    for file in dumped.files {
        let path = format!("{}/{}", project.dir_str, file.path);
        match file.bytes {
            Some(bytes) => write_if_changed(Path::new(&path), &bytes)?,
            None => remove_file(Path::new(&path))?,
        }
    }
    let mut written = vec![];
    if !o.no_plugins {
        for (rel, bytes) in plugins::after_dump(project.manifest, repository) {
            let path = PathBuf::from(format!("{vendor}/{rel}"));
            write_if_changed(&path, &bytes)?;
            written.push(path);
        }
    }
    events("post-autoload-dump")?;
    Ok(written)
}

/// `verify`, skipped when a check of the same composer.json, lock, platform and options passed before (a marker in
/// Pun's cache): its answer depends on nothing else.
fn verify_once(
    o: &Options,
    manifest: &Map<String, Value>,
    lock: &Lock,
    probe: &Value,
) -> Result<Option<String>> {
    let marker = o.cache.as_deref().map(|cache| {
        let exe = std::env::current_exe().unwrap_or_default();
        let inputs = (
            env!("CARGO_PKG_VERSION"),
            fingerprint::stamp(&exe, true),
            manifest,
            (&lock.settings, &lock.packages, &lock.packages_dev),
            probe,
            (o.no_dev, format!("{:?}", o.ignore_platform)),
        );
        let text = serde_json::to_vec(&inputs).expect("JSON values serialize");
        cache.join("verified").join("v1").join(sha1(&text))
    });
    if marker.as_ref().is_some_and(|m| m.exists()) {
        return Ok(None);
    }
    let problems = verify(manifest, lock, probe, !o.no_dev, &o.ignore_platform)?;
    if let (None, Some(marker)) = (&problems, &marker) {
        // Best effort: a failed write only costs the check next time.
        let _ = crate::fetch::store(marker, b"");
    }
    Ok(problems)
}

/// What the autoloader's files are made of, after the no-op fingerprint, so a dump with the same inputs is the one
/// kept from the last dump of this vendor directory: the options, composer.json, every package's metadata and
/// install path, and the files under the root's autoload paths. A package from the store is taken not to change at
/// one version (as the class map cache takes it); the directory of any other is stamped like the root's paths.
/// `None` when the dump is not the same twice: an APCu prefix picked at random.
fn dump_key(
    project: &Project,
    packages: &[Installed],
    options: &autoload::Options,
) -> Option<String> {
    if options.apcu == Apcu::RandomPrefix {
        return None;
    }
    let mut s = format!("pun {}\n", env!("CARGO_PKG_VERSION"));
    let exe = std::env::current_exe().unwrap_or_default();
    let _ = writeln!(s, "{}", fingerprint::stamp(&exe, true));
    let _ = writeln!(
        s,
        "{}\n{}\n{options:?}",
        project.dir_str, project.dirs.vendor
    );
    let json = |v: &Map<String, Value>| serde_json::to_string(v).expect("JSON values serialize");
    let _ = writeln!(s, "{}", json(project.manifest));
    let mut seen = HashSet::new();
    let mut trees: Vec<PathBuf> = vec![];
    for p in packages {
        let _ = writeln!(s, "{:?} {} {}", p.path, p.dev, json(&p.package.0));
        let from_store = Store::key(&p.package).is_some()
            && (p.package.0.get("installation-source")).is_none_or(|s| s == "dist");
        if let (Some(path), false) = (&p.path, from_store) {
            trees.push(PathBuf::from(path));
        }
    }
    let kinds = ["autoload", "autoload-dev"];
    for kind in &kinds[..if options.dev_mode { 2 } else { 1 }] {
        autoload_paths(project.manifest.get(*kind), &mut |path| {
            trees.push(project.dir.join(path));
        });
    }
    for tree in trees {
        fingerprint::tree(&tree, &mut s, &mut seen);
    }
    Some(sha1(s.as_bytes()))
}

fn download_threads(env: Option<String>, cores: usize) -> usize {
    let http = env.and_then(|v| v.parse::<usize>().ok()).filter(|n| *n > 0);
    http.unwrap_or(48).max(cores)
}

fn names(p: &Installed) -> Vec<String> {
    let mut out = vec![p.name()];
    for kind in ["provide", "replace"] {
        out.extend(p.package.link_targets(kind));
    }
    out
}

/// Composer's operation order (`Transaction::calculateOperations`): dependencies first, plugins moved to the front.
fn install_order(packages: &[Installed]) -> Vec<usize> {
    plugins_first(packages, &dependency_order(packages))
}

/// The order Composer's local repository holds the packages in after an install, which is the order plugins read
/// them in: the ones kept in installed.json's order, then the installed and updated ones in operation order, since
/// each is added (an update removed first) as its operation runs. Composer moves plugins to the front among the
/// operations only.
fn repository_order(
    packages: &[Installed],
    before: &IndexMap<String, LockedPackage>,
    todo: &[usize],
) -> Vec<usize> {
    let mut kept: Vec<usize> = (0..packages.len()).filter(|i| !todo.contains(i)).collect();
    kept.sort_by_key(|&i| before.get_index_of(&packages[i].name()));
    let operations: Vec<usize> = (dependency_order(packages).into_iter())
        .filter(|i| todo.contains(i))
        .collect();
    kept.extend(plugins_first(packages, &operations));
    kept
}

/// The packages in post-order of a depth-first walk from the root packages, as Composer's transaction visits them.
fn dependency_order(packages: &[Installed]) -> Vec<usize> {
    let mut by_name: Vec<usize> = (0..packages.len()).collect();
    by_name.sort_by_key(|&i| std::cmp::Reverse(packages[i].name()));
    let mut providers: HashMap<String, Vec<usize>> = HashMap::new();
    for &i in &by_name {
        for name in names(&packages[i]) {
            providers.entry(name).or_default().push(i);
        }
    }
    let required = |i: usize| -> Vec<usize> {
        (packages[i].package.link_targets("require"))
            .flat_map(|t| providers.get(&t).into_iter().flatten().copied())
            .collect()
    };
    let mut roots: Vec<usize> = by_name.clone();
    for &i in &by_name {
        if roots.contains(&i) {
            let required = required(i);
            roots.retain(|r| *r == i || !required.contains(r));
        }
    }
    let (mut visited, mut done) = (vec![false; packages.len()], vec![false; packages.len()]);
    let mut order = vec![];
    let mut stack = roots;
    while let Some(i) = stack.pop() {
        if done[i] {
            continue;
        }
        if visited[i] {
            done[i] = true;
            order.push(i);
        } else {
            visited[i] = true;
            stack.push(i);
            stack.extend(required(i));
        }
    }
    order
}

/// `Transaction::movePluginsToFront` on `order`.
fn plugins_first(packages: &[Installed], order: &[usize]) -> Vec<usize> {
    let (mut front, mut rest): ([Vec<usize>; 4], Vec<usize>) = (Default::default(), vec![]);
    let mut wanted: [Vec<String>; 2] = Default::default();
    for &i in order.iter().rev() {
        let p = &packages[i];
        let kind = p.package.kind();
        let modifies = kind == Some("composer-plugin")
            && (p.package.get("extra")).and_then(|e| e.get("plugin-modifies-downloads"))
                == Some(&Value::Bool(true));
        let plugin = matches!(kind, Some("composer-plugin" | "composer-installer"));
        let names = names(p);
        let group = [modifies, plugin]
            .iter()
            .zip(&wanted)
            .position(|(is, want)| *is || names.iter().any(|n| want.contains(n)));
        let Some(g) = group else {
            rest.insert(0, i);
            continue;
        };
        let requires: Vec<String> = (p.package.link_targets("require"))
            .filter(|r| !is_platform(r))
            .collect();
        if [modifies, plugin][g] && requires.is_empty() {
            front[2 * g].insert(0, i);
        } else {
            wanted[g].extend(requires);
            front[2 * g + 1].insert(0, i);
        }
    }
    front.into_iter().flatten().chain(rest).collect()
}

fn place(
    fetcher: &Fetcher,
    store: Option<&Store>,
    package: &LockedPackage,
    base: &Path,
    target: &Path,
) -> Result<bool> {
    let dist = package.dist();
    match dist.and_then(|d| d.get("type")).and_then(Value::as_str) {
        Some("path") => {
            let url = dist
                .and_then(|d| d.get("url"))
                .and_then(Value::as_str)
                .context("path dist without a url")?;
            let mirror = (package.get("transport-options")).and_then(|t| t.get("symlink"))
                == Some(&Value::Bool(false));
            link_path(url, base, target, mirror)?;
            Ok(false)
        }
        Some(kind) => {
            if let (Some(store), Some(key)) = (store, Store::key(package)) {
                let mut cached = false;
                let stored = store.place(&key, target, |dir| {
                    let bytes;
                    (bytes, cached) = fetcher.archive(package)?;
                    unpack(kind, &bytes, dir)
                })?;
                return Ok(stored || cached);
            }
            let (bytes, cached) = fetcher.archive(package)?;
            unpack(kind, &bytes, target)?;
            Ok(cached)
        }
        None => {
            let kind = package
                .get("source")
                .and_then(|s| s.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("none");
            bail!("it has no dist archive, and installing from {kind} sources is not supported yet")
        }
    }
}

fn remove_file(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

fn write_if_changed(path: &Path, bytes: &[u8]) -> Result<()> {
    if std::fs::read(path).ok().as_deref() == Some(bytes) {
        return Ok(());
    }
    // Through a temporary file, so a symlink in the file's place is replaced, not written through.
    crate::fetch::store(path, bytes).with_context(|| format!("can't write {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The fingerprint walks the paths autoload reads, not the exclusion patterns: `/` there excludes nothing outside
    /// the project and must not be walked as the filesystem root.
    #[test]
    fn fingerprints_walk_autoload_paths_not_exclusions() {
        let section = json!({
            "psr-4": {"App\\": "src/", "Lib\\": ["lib/", "more/"]},
            "classmap": ["classes/"],
            "files": ["helpers.php"],
            "exclude-from-classmap": ["/", "/Tests/"],
        });
        let mut paths = vec![];
        autoload_paths(Some(&section), &mut |p| paths.push(p.to_string()));
        paths.sort();
        assert_eq!(paths, ["classes/", "helpers.php", "lib/", "more/", "src/"]);
    }

    #[test]
    fn download_threads_follow_composer_max_parallel_http() {
        assert_eq!(download_threads(None, 8), 48);
        assert_eq!(download_threads(Some("12".into()), 8), 12);
        assert_eq!(download_threads(Some("2".into()), 8), 8);
        assert_eq!(download_threads(Some("0".into()), 8), 48);
        assert_eq!(download_threads(Some("many".into()), 8), 48);
    }

    #[test]
    fn install_order_matches_composer() {
        let packages: Vec<Installed> = [
            json!({"name": "a/dep", "require": {"php": ">=8"}}),
            json!({"name": "b/dl", "type": "composer-plugin", "require": {"composer-plugin-api": "^2"}, "extra": {"plugin-modifies-downloads": true}}),
            json!({"name": "c/lib", "require": {"d/base": "*", "virtual/thing": "*"}}),
            json!({"name": "d/base"}),
            json!({"name": "e/prov", "provide": {"virtual/thing": "1.0"}}),
            json!({"name": "m/app", "require": {"c/lib": "*"}}),
            json!({"name": "n/cyc", "require": {"o/cyc": "*"}}),
            json!({"name": "o/cyc", "require": {"n/cyc": "*"}}),
            json!({"name": "x/helper"}),
            json!({"name": "y/inst", "type": "composer-installer", "require": {"x/helper": "*"}}),
            json!({"name": "z/plugin", "type": "composer-plugin", "require": {"composer-plugin-api": "^2", "a/dep": "*"}}),
        ]
        .into_iter()
        .map(|p| Installed {
            package: LockedPackage(p.as_object().unwrap().clone()),
            path: None,
            dev: false,
        })
        .collect();
        let order: Vec<String> = (install_order(&packages).into_iter())
            .map(|i| packages[i].name())
            .collect();
        assert_eq!(
            order,
            [
                "b/dl", "x/helper", "y/inst", "a/dep", "z/plugin", "d/base", "e/prov", "c/lib",
                "m/app", "o/cyc", "n/cyc"
            ]
        );
    }
}
