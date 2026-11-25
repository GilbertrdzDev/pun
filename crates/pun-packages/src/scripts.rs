#![expect(
    clippy::print_stderr,
    reason = "each script is echoed before it runs, as Composer echoes it"
)]

use crate::Env;
use crate::error::{Context, Result, bail};
use serde_json::{Map, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Runner {
    pub base: PathBuf,
    pub vendor: String,
    pub bin_dir: String,
    pub manifest: Map<String, Value>,
    pub dev_mode: bool,
    pub php: String,
    path: String,
    env: Vec<(String, Option<String>)>,
    stack: Vec<String>,
}

impl Runner {
    pub fn new(
        base: &Path,
        vendor: &str,
        bin_dir: &str,
        manifest: &Map<String, Value>,
        dev_mode: bool,
        env: Env,
    ) -> Self {
        Runner {
            base: base.to_path_buf(),
            vendor: vendor.to_string(),
            bin_dir: bin_dir.to_string(),
            manifest: manifest.clone(),
            dev_mode,
            php: which(
                &php_binary(env("PHP_BINARY")),
                &env("PATH").unwrap_or_default(),
            ),
            path: env("PATH").unwrap_or_default(),
            env: vec![],
            stack: vec![],
        }
    }

    fn listeners(&self, event: &str) -> Vec<String> {
        match self.manifest.get("scripts").and_then(|s| s.get(event)) {
            Some(Value::String(s)) => vec![s.clone()],
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            _ => vec![],
        }
    }

    pub fn dispatch(&mut self, event: &str, args: &[String]) -> Result<()> {
        if self.stack.iter().any(|e| e == event) {
            bail!("Circular call to script handler '{event}' detected");
        }
        self.stack.push(event.to_string());
        let result = (|| {
            for callable in self.listeners(event) {
                self.run(event, &callable, args)?;
            }
            Ok(())
        })();
        self.stack.pop();
        result
    }

    fn run(&mut self, event: &str, callable: &str, args: &[String]) -> Result<()> {
        let (callable, args) = if callable.contains("@no_additional_args") {
            (
                callable
                    .replace(" @no_additional_args", "")
                    .replace("@no_additional_args", ""),
                &[][..],
            )
        } else {
            (callable.to_string(), args)
        };
        if callable.is_empty() {
            return Ok(());
        }
        let quoted = args.iter().map(|a| quote(a)).collect::<Vec<_>>().join(" ");
        if let Some(rest) = callable.strip_prefix("@putenv ") {
            match rest.split_once('=') {
                Some((k, v)) => self.env.push((k.to_string(), Some(v.to_string()))),
                None => self.env.push((rest.to_string(), None)),
            }
            return Ok(());
        }
        if callable.starts_with('@')
            && !callable.starts_with("@php ")
            && !callable.starts_with("@composer ")
        {
            let mut words = split_args(&callable[1..]);
            let name = if words.is_empty() {
                String::new()
            } else {
                words.remove(0)
            };
            let mut forwarded = vec![];
            let mut had = false;
            for w in words {
                if w == "@additional_args" {
                    forwarded.extend(args.iter().cloned());
                    had = true;
                } else {
                    forwarded.push(w);
                }
            }
            if !had {
                forwarded.extend(args.iter().cloned());
            }
            if self.listeners(&name).is_empty() {
                eprintln!("You made a reference to a non-existent script {callable}");
            }
            return self
                .dispatch(&name, &forwarded)
                .with_context(|| format!("Script {callable} was called via {event}"));
        }
        if !callable.contains(' ') && callable.contains("::") {
            return self.callback(event, &callable, args);
        }
        let with_args = |c: &str| match (c.contains("@additional_args"), quoted.is_empty()) {
            (true, _) => c.replace("@additional_args", &quoted),
            (false, true) => c.to_string(),
            (false, false) => format!("{c} {quoted}"),
        };
        let exec = if let Some(rest) = callable.strip_prefix("@composer ") {
            format!("composer {}", with_args(rest))
        } else if let Some(rest) = callable.strip_prefix("@php ") {
            format!("{} {}", quote(&self.php), with_args(rest))
        } else {
            with_args(&callable)
        };
        eprintln!("> {}", with_args(&callable));
        // Symfony's Process starts Composer scripts with `/bin/sh -c` on Unix and `cmd /D /C` on Windows.
        // The child's PATH puts the bin dir first; the shell itself is not taken from that PATH.
        #[cfg(unix)]
        let status = self.command("/bin/sh").arg("-c").arg(&exec).status()?;
        #[cfg(windows)]
        let status = self
            .command("cmd")
            .arg("/D")
            .arg("/C")
            .arg(&exec)
            .status()?;
        if !status.success() {
            bail!(
                "Script {callable} handling the {event} event returned with error code {}",
                status.code().unwrap_or(1)
            );
        }
        Ok(())
    }

    fn callback(&mut self, event: &str, callable: &str, args: &[String]) -> Result<()> {
        eprintln!("> {callable}");
        let runner = Helper::new(
            "script-callback.php",
            include_str!("../assets/script-callback.php"),
        )?;
        let status = (self.command(&self.php.clone()))
            .arg(&runner.path)
            .arg(&self.vendor)
            .arg(event)
            .arg(if self.dev_mode { "1" } else { "0" })
            .arg(callable)
            .args(args)
            .env(
                "PUN_ROOT_PACKAGE",
                Value::Object(self.manifest.clone()).to_string(),
            )
            .status();
        drop(runner);
        let status = status.with_context(|| format!("can't run {} for {callable}", self.php))?;
        if !status.success() {
            bail!(
                "Script {callable} handling the {event} event returned with error code {}",
                status.code().unwrap_or(1)
            );
        }
        Ok(())
    }

    fn command(&self, program: &str) -> Command {
        let mut c = Command::new(program);
        c.current_dir(&self.base);
        if Path::new(&self.bin_dir).is_dir() {
            let existing: Vec<PathBuf> = std::env::split_paths(&self.path).collect();
            if !existing.iter().any(|p| p == Path::new(&self.bin_dir)) {
                let mut paths = vec![PathBuf::from(&self.bin_dir)];
                paths.extend(existing);
                if let Ok(path) = std::env::join_paths(&paths) {
                    c.env("PATH", path);
                }
            }
        }
        c.env("COMPOSER_DEV_MODE", if self.dev_mode { "1" } else { "0" });
        c.env("PHP_BINARY", &self.php);
        for (k, v) in &self.env {
            match v {
                Some(v) => c.env(k, v),
                None => c.env_remove(k),
            };
        }
        c
    }
}

/// A PHP helper pun writes for one run, in a directory made for it alone and readable only by this user, so no other
/// account can put its own code at the path PHP opens. The directory goes when the helper is dropped.
pub(crate) struct Helper {
    dir: PathBuf,
    pub path: PathBuf,
}

impl Helper {
    pub(crate) fn new(name: &str, source: &str) -> std::io::Result<Helper> {
        use std::io::Write;
        let nanos = (std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH))
            .map_or(0, |d| d.as_nanos());
        let mut attempt = 0;
        loop {
            let dir = std::env::temp_dir()
                .join(format!("pun-{}-{nanos:x}-{attempt}", std::process::id()));
            // `mkdir` fails on any existing entry, a link included, so the directory is new and ours.
            // On Unix it is mode 0700; Windows has no Unix mode bits.
            #[cfg(unix)]
            let mut builder = std::fs::DirBuilder::new();
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            #[cfg(windows)]
            let builder = std::fs::DirBuilder::new();
            match builder.create(&dir) {
                Ok(()) => {
                    let helper = Helper {
                        path: dir.join(name),
                        dir,
                    };
                    (std::fs::OpenOptions::new().write(true).create_new(true))
                        .open(&helper.path)?
                        .write_all(source.as_bytes())?;
                    return Ok(helper);
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && attempt < 100 => {
                    attempt += 1;
                }
                Err(e) => return Err(e),
            }
        }
    }

    pub(crate) fn dir(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `program`'s path in `path`, the PATH pun started with, so a bin the scripts' PATH puts first can't stand in for
/// PHP (Composer runs its own `PHP_BINARY`); `program` itself when it has a slash or isn't found.
fn which(program: &str, path: &str) -> String {
    if program.contains('/') || program.contains('\\') {
        return program.to_string();
    }
    // A name that already has an extension (`php.exe`) is looked up as written. Otherwise Windows also tries
    // `PATHEXT`, so `php` finds `php.exe`.
    let exts: Vec<String> = if program.contains('.') {
        vec![String::new()]
    } else {
        path_extensions()
    };
    for dir in std::env::split_paths(path) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for ext in &exts {
            let candidate = dir.join(format!("{program}{ext}"));
            if is_runnable(&candidate) {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    program.to_string()
}

#[cfg(unix)]
fn path_extensions() -> Vec<String> {
    vec![String::new()]
}

#[cfg(windows)]
fn path_extensions() -> Vec<String> {
    let pathext = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
    let mut exts: Vec<String> = pathext
        .split(';')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    exts.push(String::new());
    exts
}

#[cfg(unix)]
fn is_runnable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(windows)]
fn is_runnable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file())
}

fn php_binary(env: Option<String>) -> String {
    env.filter(|p| !p.is_empty())
        .unwrap_or_else(|| "php".into())
}

/// Shell-quotes one argument. Unix scripts run under `/bin/sh` (single quotes). Windows scripts run under
/// `cmd.exe`, which treats only double quotes as quoting.
#[cfg(unix)]
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(windows)]
fn quote(s: &str) -> String {
    if !s
        .chars()
        .any(|c| matches!(c, ' ' | '"' | '&' | '|' | '<' | '>' | '^' | '%' | '(' | ')'))
    {
        return s.to_string();
    }
    let mut out = String::from("\"");
    let mut slashes = 0usize;
    for c in s.chars() {
        if c == '\\' {
            slashes += 1;
            continue;
        }
        if c == '"' {
            out.push_str(&"\\".repeat(slashes * 2 + 1));
            out.push('"');
        } else {
            out.push_str(&"\\".repeat(slashes));
            out.push(c);
        }
        slashes = 0;
    }
    out.push_str(&"\\".repeat(slashes * 2));
    out.push('"');
    out
}

fn split_args(command: &str) -> Vec<String> {
    let (mut out, mut word, mut quote, mut started) = (vec![], String::new(), None::<char>, false);
    for ch in command.chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), c) => word.push(c),
            (None, '"' | '\'') => {
                quote = Some(ch);
                started = true;
            }
            (None, c) if c.is_whitespace() => {
                if started {
                    out.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            (None, c) => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(word);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn project(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("pun-scripts-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("vendor/bin")).unwrap();
        d
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "a test helper called with temporaries"
    )]
    fn run(dir: &Path, scripts: Value, event: &str) -> Result<()> {
        let manifest = json!({"scripts": scripts}).as_object().unwrap().clone();
        let vendor = dir.join("vendor").to_string_lossy().into_owned();
        let bin = dir.join("vendor/bin").to_string_lossy().into_owned();
        Runner::new(dir, &vendor, &bin, &manifest, true, &|k| {
            std::env::var(k).ok()
        })
        .dispatch(event, &[])
    }

    #[cfg(unix)]
    #[test]
    fn commands_references_and_environment() {
        let d = project("run");
        for (bin, line) in [("tool", "tool"), ("composer", "composer $*")] {
            std::fs::write(
                d.join("vendor/bin").join(bin),
                format!("#!/bin/sh\necho {line} >> out.txt\n"),
            )
            .unwrap();
            std::fs::set_permissions(
                d.join("vendor/bin").join(bin),
                std::os::unix::fs::PermissionsExt::from_mode(0o755),
            )
            .unwrap();
        }
        let scripts = json!({
            "a": ["@putenv FOO=bar", "echo \"$FOO $COMPOSER_DEV_MODE\" > out.txt", "@b 'x y'", "tool", "@composer show x", "@putenv FOO", "echo \"[$FOO]\" >> out.txt"],
            "b": "echo b >> out.txt",
        });
        run(&d, scripts, "a").unwrap();
        assert_eq!(
            std::fs::read_to_string(d.join("out.txt")).unwrap(),
            "bar 1\nb x y\ntool\ncomposer show x\n[]\n"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn arguments_like_composer() {
        let d = project("args");
        std::fs::remove_dir_all(d.join("vendor/bin")).unwrap();
        let manifest = json!({"scripts": {
            "a": ["@b @additional_args  extra ", "echo c @additional_args >> out.txt", "echo n @no_additional_args >> out.txt", "@php -r 'echo \"php\\n\";' >> out.txt", "@missing", "@"],
            "b": "echo b >> out.txt",
        }});
        let manifest = manifest.as_object().unwrap().clone();
        let vendor = d.join("vendor").to_string_lossy().into_owned();
        let bin = d.join("vendor/bin").to_string_lossy().into_owned();
        let args = ["one".to_string(), "two words".to_string()];
        Runner::new(&d, &vendor, &bin, &manifest, true, &|k| {
            std::env::var(k).ok()
        })
        .dispatch("a", &args)
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(d.join("out.txt")).unwrap(),
            "b one two words extra\nc one two words\nn\nphp\n"
        );
        assert_eq!(quote("it's"), "'it'\\''s'");
        assert_eq!(
            (php_binary(None), php_binary(Some(String::new()))),
            ("php".into(), "php".into())
        );
        assert_eq!(
            php_binary(Some("/usr/bin/php8.4".into())),
            "/usr/bin/php8.4"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn failures_use_composers_messages() {
        let d = project("fail");
        let error =
            |scripts: Value, event: &str| format!("{:#}", run(&d, scripts, event).unwrap_err());
        assert_eq!(
            error(json!({"a": "exit 3"}), "a"),
            "Script exit 3 handling the a event returned with error code 3"
        );
        assert_eq!(
            error(json!({"a": "@a"}), "a"),
            "Script @a was called via a: Circular call to script handler 'a' detected"
        );
        assert_eq!(
            error(json!({"a": "@b", "b": "exit 4"}), "a"),
            "Script @b was called via a: Script exit 4 handling the b event returned with error code 4"
        );
        run(&d, json!({"a": ""}), "a").unwrap();
        run(&d, json!({}), "post-install-cmd").unwrap();
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// Composer runs scripts with `/bin/sh` and its own PHP; a dependency's `sh` or `php` in the bin dir, which the
    /// scripts' PATH puts first, runs neither.
    #[cfg(unix)]
    #[test]
    fn the_bin_dir_does_not_supply_the_interpreters() {
        let d = project("interpreters");
        for bin in ["sh", "php"] {
            let path = d.join("vendor/bin").join(bin);
            std::fs::write(&path, "#!/bin/sh\necho impostor >> out.txt\n").unwrap();
            std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755))
                .unwrap();
        }
        let scripts =
            json!({"a": ["echo shell >> out.txt", "@php -r 'echo \"php\\n\";' >> out.txt"]});
        run(&d, scripts, "a").unwrap();
        assert_eq!(
            std::fs::read_to_string(d.join("out.txt")).unwrap(),
            "shell\nphp\n"
        );
        assert_eq!(which("sh", "/nowhere"), "sh");
        assert_eq!(which("./php", "/bin"), "./php");
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// The callback runner goes in a new private directory: a link at a guessable name in the shared temporary
    /// directory is neither written through nor run.
    #[cfg(unix)]
    #[test]
    fn helpers_are_private_and_new() {
        use std::os::unix::fs::PermissionsExt;
        let d = project("helper");
        let sentinel = d.join("sentinel");
        std::fs::write(&sentinel, "kept").unwrap();
        let old = std::env::temp_dir().join(format!("pun-script-{}.php", std::process::id()));
        let _ = std::fs::remove_file(&old);
        std::os::unix::fs::symlink(&sentinel, &old).unwrap();
        std::fs::write(d.join("vendor/autoload.php"), "<?php\nfinal class Hook { public static function run() { file_put_contents('out.txt', 'ran'); } }\n").unwrap();
        run(&d, json!({"x": "Hook::run"}), "x").unwrap();
        std::fs::remove_file(&old).unwrap();
        assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), "kept");
        assert_eq!(std::fs::read_to_string(d.join("out.txt")).unwrap(), "ran");

        let (a, b) = (
            Helper::new("a.php", "<?php").unwrap(),
            Helper::new("a.php", "<?php").unwrap(),
        );
        assert_ne!(a.dir(), b.dir());
        let mode = std::fs::metadata(a.dir()).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
        assert_eq!(std::fs::read_to_string(&a.path).unwrap(), "<?php");
        let dir = a.dir().to_path_buf();
        drop(a);
        assert!(!dir.exists());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn php_callbacks() {
        let d = project("callback");
        std::fs::write(
            d.join("vendor/autoload.php"),
            "<?php\nnamespace Acme;\nfinal class Hooks {\n    public static function dump($event) {\n        file_put_contents('out.txt', $event->getName().' '.(int) $event->isDevMode().' '.basename($event->getComposer()->getConfig()->get('vendor-dir')).' '.$event->getComposer()->getPackage()->getName());\n    }\n    public static function fail($event) { return false; }\n}\n",
        )
        .unwrap();
        run(
            &d,
            json!({"post-autoload-dump": "Acme\\Hooks::dump"}),
            "post-autoload-dump",
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(d.join("out.txt")).unwrap(),
            "post-autoload-dump 1 vendor __root__"
        );
        let error = format!(
            "{:#}",
            run(&d, json!({"x": "Acme\\Hooks::fail"}), "x").unwrap_err()
        );
        assert_eq!(
            error,
            "Script Acme\\Hooks::fail handling the x event returned with error code 1"
        );
        run(&d, json!({"x": "Acme\\Missing::run"}), "x").unwrap();
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// Composer scripts on Windows are a `cmd.exe` command line, so arguments with spaces are double-quoted and
    /// `cmd /D /C` runs the line.
    #[cfg(windows)]
    #[test]
    fn windows_scripts_run_through_cmd() {
        let d = project("cmd");
        assert_eq!(quote("two words"), "\"two words\"");
        assert_eq!(quote("plain"), "plain");
        assert_eq!(quote("say \"hi\""), "\"say \\\"hi\\\"\"");
        let scripts = json!({"a": "echo hello> out.txt"});
        run(&d, scripts, "a").unwrap();
        let text = std::fs::read_to_string(d.join("out.txt")).unwrap();
        assert!(text.contains("hello"), "{text}");
        assert_eq!(which(r"C:\php\php.exe", ""), r"C:\php\php.exe");
        std::fs::remove_dir_all(&d).unwrap();
    }
}
