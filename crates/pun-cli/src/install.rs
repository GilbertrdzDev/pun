use crate::Exit;
use crate::render::{ColorChoice, Look, Role};
use anyhow::{Context, Result, bail};
use pun_packages::autoload::{Apcu, Scan};
use pun_packages::fetch::{Auth, Fetcher, composer_dirs};
use pun_packages::install::{Options, install};
use pun_packages::lock::Lock;
use pun_packages::plugin_host::Dispatcher;
use pun_packages::store::LinkMode;
use pun_packages::verify::missing_requirements;
use pun_packages::version::IgnorePlatform;
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(clap::Args, Clone, Default)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "one bool per command-line flag"
)]
pub(crate) struct InstallArgs {
    /// Leave out the dev dependencies (`require-dev`).
    #[arg(long)]
    pub(crate) no_dev: bool,
    /// Put the PSR-0 and PSR-4 classes in the class map too.
    #[arg(short = 'o', long)]
    pub(crate) optimize_autoloader: bool,
    /// Load classes from the class map only (implies --optimize-autoloader).
    #[arg(short = 'a', long)]
    pub(crate) classmap_authoritative: bool,
    /// Cache found and missing classes in APCu.
    #[arg(long)]
    pub(crate) apcu_autoloader: bool,
    /// The APCu cache prefix (implies --apcu-autoloader); without it each dump picks a random one.
    #[arg(long, value_name = "PREFIX")]
    pub(crate) apcu_autoloader_prefix: Option<String>,
    /// Skip writing the autoloader.
    #[arg(long)]
    pub(crate) no_autoloader: bool,
    /// Skip the root package's scripts.
    #[arg(long)]
    pub(crate) no_scripts: bool,
    /// Skip the Composer plugins.
    #[arg(long)]
    pub(crate) no_plugins: bool,
    /// Leave this platform requirement out of the platform check (`php`, `ext-*`, globs with `*`); repeatable.
    #[arg(long, value_name = "REQ")]
    pub(crate) ignore_platform_req: Vec<String>,
    /// Leave every platform requirement out of the platform check.
    #[arg(long)]
    pub(crate) ignore_platform_reqs: bool,
    /// How package files reach vendor/ from Pun's store: hardlink, clone (copy-on-write, else copy) or copy.
    /// Default: `PUN_LINK_MODE`, else hardlink.
    #[arg(long, value_name = "MODE")]
    pub(crate) link_mode: Option<String>,
    /// Run in this directory instead of the current one.
    #[arg(short = 'd', long, value_name = "DIR")]
    pub(crate) working_dir: Option<PathBuf>,
}

/// A project's dependencies: its composer.json, and composer.lock when it exists.
pub(crate) struct Project {
    pub(crate) root: PathBuf,
    pub(crate) manifest: Map<String, Value>,
    pub(crate) lock: Option<Lock>,
    /// The workspace the project is the root of; `manifest` then holds the members too.
    pub(crate) workspace: Option<crate::workspace::Workspace>,
    /// The directory the command started in: the root, or a directory below it.
    pub(crate) here: PathBuf,
    /// The command's events, with the plugin host they start; made at the first event.
    events: std::cell::RefCell<Option<Dispatcher>>,
    /// The time spent in the events: Composer's plugins and the scripts, which run on PHP.
    composer: std::cell::Cell<Duration>,
}

impl Project {
    /// Dispatches `event` to the plugins and the scripts, as `a` allows them.
    pub(crate) fn dispatch(&self, a: &InstallArgs, event: &str) -> pun_packages::error::Result<()> {
        let started = Instant::now();
        let mut events = self.events.borrow_mut();
        let events = events.get_or_insert_with(|| {
            Dispatcher::new(
                &self.root,
                &self.manifest,
                !a.no_dev,
                !a.no_scripts,
                !a.no_plugins,
                composer_dirs().map(|(_, cache)| cache).as_deref(),
                &|k| std::env::var(k).ok(),
            )
        });
        let result = events.dispatch(event);
        self.composer.set(self.composer.get() + started.elapsed());
        result
    }
}

/// The PHP Composer runs on: `PHP_BINARY`, else the `php` on the PATH.
pub(crate) fn php_binary() -> String {
    std::env::var("PHP_BINARY")
        .ok()
        .filter(|p| !p.is_empty())
        .unwrap_or_else(|| "php".into())
}

/// The downloader for a project: Composer's cache, the project's and the global auth.json, and `secure-http`.
pub(crate) fn fetcher(root: &Path, manifest: &Map<String, Value>) -> Fetcher {
    let secure_http =
        manifest.get("config").and_then(|c| c.get("secure-http")) != Some(&Value::Bool(false));
    let (home, cache) = composer_dirs().unzip();
    Fetcher::new(
        cache,
        Auth::load(root, home.as_deref(), std::env::var("COMPOSER_AUTH").ok()),
        secure_http,
    )
}

/// The project in `working_dir` (or the current directory). With `create`, a directory without dependencies is a
/// new project whose dependencies go in a new composer.json. In a workspace member, the project is the workspace's root.
pub(crate) fn project(working_dir: Option<&PathBuf>, create: bool) -> Result<Project> {
    let here = match working_dir {
        Some(d) => crate::canonical(d).with_context(|| format!("can't open {}", d.display()))?,
        None => crate::root()?,
    };
    let workspace = crate::workspace::find(&here)?;
    let root = workspace
        .as_ref()
        .map_or_else(|| here.clone(), |w| w.root.clone());
    let manifest = match pun_config::manifest::read(&root)? {
        Some(m) => m.json,
        None if workspace.is_some() || create => Map::new(),
        None => bail!(
            "no composer.json in {}: add a dependency with `pun require <package>`",
            root.display()
        ),
    };
    let lock = Lock::read(&root)?;
    let manifest = match &workspace {
        Some(w) => w.manifest(manifest),
        None => manifest,
    };
    Ok(Project {
        root,
        manifest,
        lock,
        workspace,
        here,
        events: std::cell::RefCell::new(None),
        composer: std::cell::Cell::default(),
    })
}

pub(crate) fn run(a: &InstallArgs) -> Result<Exit> {
    let started = Instant::now();
    let mut project = project(a.working_dir.as_ref(), false)?;
    if project.lock.is_none() {
        eprintln!("No composer.lock yet: resolving the dependencies to their latest versions");
        return crate::update::update_and_install(
            &mut project,
            &crate::update::Plan::default(),
            a,
            started,
        );
    }
    let lock = project.lock.take().expect("a lock");
    install_locked(&project, &lock, a, started, false)
}

/// `--link-mode`, else `PUN_LINK_MODE`, else hardlink.
fn link_mode(flag: Option<&str>) -> Result<LinkMode> {
    let env = std::env::var("PUN_LINK_MODE")
        .ok()
        .filter(|v| !v.is_empty());
    Ok(match flag.map(String::from).or(env) {
        Some(name) => LinkMode::parse(&name)?,
        None => LinkMode::default(),
    })
}

/// The class map the autoloader flags ask for.
pub(crate) fn scan(optimize: bool, authoritative: bool) -> Scan {
    if authoritative {
        Scan::Authoritative
    } else if optimize {
        Scan::Optimized
    } else {
        Scan::Classmap
    }
}

/// The APCu cache the flags ask for: a prefix implies the cache, as in Composer.
pub(crate) fn apcu(enabled: bool, prefix: Option<&String>) -> Apcu {
    match prefix {
        Some(prefix) => Apcu::Prefix(prefix.clone()),
        None if enabled => Apcu::RandomPrefix,
        None => Apcu::Off,
    }
}

/// Prints Composer's lines for each requirement of composer.json the lock doesn't meet; false when the install
/// stops there, as it does without `config.allow-missing-requirements`.
fn meets_requirements(manifest: &Map<String, Value>, lock: &Lock, no_dev: bool) -> Result<bool> {
    let missing = missing_requirements(manifest, lock, !no_dev)?;
    for line in &missing {
        eprintln!("{line}");
    }
    let allowed = (manifest
        .get("config")
        .and_then(|c| c.get("allow-missing-requirements")))
        == Some(&Value::Bool(true));
    Ok(missing.is_empty() || allowed)
}

/// Installs `lock` into the project's `vendor/` and reports it; `update` dispatches the update events instead of
/// the install ones.
pub(crate) fn install_locked(
    project: &Project,
    lock: &Lock,
    a: &InstallArgs,
    started: Instant,
    update: bool,
) -> Result<Exit> {
    let (root, manifest) = (&project.root, &project.manifest);
    let look = Look::stderr(ColorChoice::Auto);
    let (pre, post) = if update {
        ("pre-update-cmd", "post-update-cmd")
    } else {
        ("pre-install-cmd", "post-install-cmd")
    };
    let options = Options {
        no_dev: a.no_dev,
        scan: scan(a.optimize_autoloader, a.classmap_authoritative),
        apcu: apcu(a.apcu_autoloader, a.apcu_autoloader_prefix.as_ref()),
        no_autoloader: a.no_autoloader,
        no_plugins: a.no_plugins,
        ignore_platform: IgnorePlatform::new(a.ignore_platform_req.clone(), a.ignore_platform_reqs),
        php: (!update).then(php_binary),
        cache: pun_config::cache_dir(),
        link: link_mode(a.link_mode.as_deref())?,
    };
    let fetcher = fetcher(root, manifest);
    let env = |k: &str| std::env::var(k).ok();
    let total = lock.packages.len() + if a.no_dev { 0 } else { lock.packages_dev.len() };
    eprintln!(
        "Installing {} from composer.lock",
        crate::agent::count(total, "package")
    );
    if !update {
        project.dispatch(a, pre)?;
        // Composer checks this on every install from a lock, after `pre-install-cmd`; an update has just solved it.
        if !meets_requirements(manifest, lock, a.no_dev)? {
            return Ok(Exit::LockInvalid);
        }
    }
    let mut events = |event: &str| project.dispatch(a, event);
    let report = install(root, manifest, lock, &fetcher, &options, &env, &mut events)?;
    if let Some(problems) = &report.incompatible {
        let cross = look.paint(Role::Error, look.glyph("✖", "x"));
        eprintln!(
            "{cross} Your lock file does not contain a compatible set of packages. Please run pun update.\n{problems}"
        );
        return Ok(Exit::RunError);
    }
    project.dispatch(a, post)?;
    if let Some(pending) = &report.fingerprint {
        pending.save();
    }

    let ok = look.paint(Role::Ok, look.glyph("✔", "+"));
    let warn = look.paint(Role::Warning, look.glyph("⚠", "!"));
    if report.stale_lock {
        eprintln!(
            " {warn} composer.lock is out of date with composer.json: the dependencies changed since it was written, so run `pun update` to refresh it"
        );
    }
    let mut changes = vec![];
    for (what, n) in [
        ("installed", report.installed.len()),
        ("updated", report.updated.len()),
        ("removed", report.removed.len()),
    ] {
        if n > 0 {
            changes.push(format!("{n} {what}"));
        }
    }
    if report.kept > 0 {
        changes.push(format!("{} up to date", report.kept));
    }
    let cached = if report.cached > 0 {
        format!(" ({} from the cache)", report.cached)
    } else {
        String::new()
    };
    eprintln!(
        " {ok} packages: {}{cached}",
        if changes.is_empty() {
            "none".into()
        } else {
            changes.join(", ")
        }
    );
    if !a.no_autoloader {
        let noun = if report.classes == 1 {
            "class"
        } else {
            "classes"
        };
        eprintln!(" {ok} autoloader: {} {noun}", report.classes);
    }
    for w in &report.warnings {
        eprintln!(" {warn} {w}");
    }
    eprintln!(
        "{}",
        look.paint(
            Role::Summary,
            &format!(
                "Installed in {}",
                took(started.elapsed(), project.composer.get())
            )
        )
    );
    Ok(Exit::Success)
}

/// The time a command took: the total, and when Composer's plugins and scripts ran, how it splits between Pun and
/// them, such as `3.2s (Pun 0.4s, Composer scripts and plugins 2.8s)`. Under 0.05s of Composer time shows the total
/// alone, as it rounds to nothing.
fn took(total: Duration, composer: Duration) -> String {
    let secs = total.as_secs_f64();
    let composer = composer.as_secs_f64();
    if composer < 0.05 {
        return format!("{secs:.1}s");
    }
    format!(
        "{secs:.1}s (Pun {:.1}s, Composer scripts and plugins {composer:.1}s)",
        (secs - composer).max(0.0)
    )
}

#[cfg(test)]
mod tests {
    use super::took;
    use std::time::Duration;

    #[test]
    fn took_splits_the_total_between_pun_and_composer() {
        let ms = Duration::from_millis;
        assert_eq!(took(ms(3240), ms(0)), "3.2s");
        assert_eq!(took(ms(3240), ms(40)), "3.2s");
        assert_eq!(
            took(ms(3240), ms(2810)),
            "3.2s (Pun 0.4s, Composer scripts and plugins 2.8s)"
        );
    }
}
