#![cfg(unix)]
mod common;

use common::{clean_env, write_files};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

fn run(program: &Path, dir: &Path, home: &Path, args: &[&str]) -> (String, String, Option<i32>) {
    let cache = home.join("cache");
    run_with(program, dir, home, &[("PUN_CACHE_DIR", &cache)], args)
}

fn run_with(
    program: &Path,
    dir: &Path,
    home: &Path,
    env: &[(&str, &Path)],
    args: &[&str],
) -> (String, String, Option<i32>) {
    let out = clean_env(Command::new(program).args(args), &["PATH"])
        .current_dir(dir)
        .env("PUN_AGENT", "0")
        .env("COMPOSER_HOME", home.join("composer"))
        .env_remove("PUN_CACHE_DIR")
        .envs(env.iter().copied())
        .output()
        .unwrap();
    let s = |b: Vec<u8>| String::from_utf8(b).unwrap();
    (s(out.stdout), s(out.stderr), out.status.code())
}

/// An install as a run leaves it in `cache`, written by hand: a fresh composer.lock and Composer 2's
/// vendor/composer/installed.json with `name` at 1.0.0 and its `bin` entries (no files in vendor/bin).
fn installed(cache: &Path, name: &str, dir_name: &str, bins: &[&str]) -> PathBuf {
    let dir = cache.join("x").join(name).join(dir_name);
    let json = serde_json::json!({
        "packages": [{"name": name, "version": "1.0.0", "version_normalized": "1.0.0.0", "bin": bins}],
        "dev": false,
        "dev-package-names": [],
    });
    write_files(
        &dir,
        &[
            ("composer.lock", "{}"),
            ("vendor/composer/installed.json", &json.to_string()),
        ],
    );
    dir
}

#[test]
fn runs_a_packages_command() {
    let home = common::scratch("x", "home");
    let tool = home.join("tool");
    let config = format!(
        r#"{{"repositories": [{{"type": "path", "url": "{}"}}, {{"packagist.org": false}}]}}"#,
        tool.display()
    );
    write_files(
        &home,
        &[
            ("composer/config.json", config.as_str()),
            (
                "tool/composer.json",
                r#"{"name": "acme/tool", "version": "1.0.0", "bin": ["bin/tool", "bin/other"]}"#,
            ),
            (
                "tool/bin/tool",
                "#!/usr/bin/env php\n<?php\necho 'tool ', implode(' ', array_slice($argv, 1)), ' in ', basename(getcwd()), \"\\n\";\nexit(3);\n",
            ),
            (
                "tool/bin/other",
                "#!/usr/bin/env php\n<?php\necho 'other ', implode(' ', array_slice($argv, 1)), \"\\n\";\n",
            ),
            ("work/.keep", ""),
        ],
    );
    let work = home.join("work");

    let pun = Path::new(env!("CARGO_BIN_EXE_pun"));
    let (out, err, code) = run(pun, &work, &home, &["x", "acme/tool", "one", "--two"]);
    assert_eq!(
        (out.as_str(), code),
        ("tool one --two in work\n", Some(3)),
        "{err}"
    );
    assert!(err.contains("Installing acme/tool into "), "{err}");
    assert!(
        home.join("cache/x/acme/tool/latest/composer.lock")
            .is_file()
    );

    let punx = home.join("punx");
    std::os::unix::fs::symlink(pun, &punx).unwrap();
    let (out, err, code) = run(&punx, &work, &home, &["acme/tool:*", "other", "x"]);
    assert_eq!((out.as_str(), code), ("other x\n", Some(0)), "{err}");
    assert!(!err.contains("Installing"), "{err}");

    let (out, err, code) = run(&punx, &work, &home, &["acme/missing"]);
    assert_eq!((out.as_str(), code), ("", Some(2)), "{err}");
    assert!(!home.join("cache/x/acme/missing").exists());

    let (_, err, code) = run(&punx, &work, &home, &["acme/tool:^1.0 || >=9", "other"]);
    assert_eq!(code, Some(0), "{err}");
    let (out, err, code) = run(&punx, &work, &home, &["list"]);
    assert_eq!(
        (out.as_str(), code),
        (
            "acme/tool              1.0.0\nacme/tool:^1.0 || >=9  1.0.0\n",
            Some(0)
        ),
        "{err}"
    );
    let (out, err, code) = run(pun, &work, &home, &["x", "clean", "acme/tool:*"]);
    assert_eq!(
        (out.as_str(), code),
        ("Removed 1 install.\n", Some(0)),
        "{err}"
    );
    let (out, _, _) = run(&punx, &work, &home, &["list"]);
    assert_eq!(out, "acme/tool:^1.0 || >=9  1.0.0\n");
    let (out, err, code) = run(&punx, &work, &home, &["clean"]);
    assert_eq!(
        (out.as_str(), code),
        ("Removed 1 install.\n", Some(0)),
        "{err}"
    );
    assert!(!home.join("cache/x/acme").exists());
    let (out, _, _) = run(&punx, &work, &home, &["list"]);
    assert_eq!(out, "No packages are installed.\n");
    let (out, err, code) = run(&punx, &work, &home, &["clean", "not-a-package"]);
    assert_eq!((out.as_str(), code), ("", Some(2)), "{err}");

    // `..` as a name part is refused; as a constraint it gets a directory of its own, so the failed install it
    // removes is not the package's directory (here holding a sentinel).
    for spec in ["../..", "acme/..", "../tool"] {
        let (out, err, code) = run(&punx, &work, &home, &[spec]);
        assert_eq!((out.as_str(), code), ("", Some(2)), "{err}");
        assert!(err.contains("is not a package name"), "{err}");
    }
    write_files(&home, &[("cache/x/acme/missing/sentinel", "kept")]);
    let (_, err, code) = run(&punx, &work, &home, &["acme/missing:.."]);
    assert_eq!(code, Some(2), "{err}");
    assert_eq!(
        std::fs::read_to_string(home.join("cache/x/acme/missing/sentinel")).unwrap(),
        "kept"
    );
    assert!(!home.join("cache/x/acme/missing/composer.json").exists());
}

#[test]
fn refreshes_a_day_old_install() {
    let home = common::scratch("x", "refresh");
    let nobin = home.join("nobin");
    let config = format!(
        r#"{{"repositories": [{{"type": "path", "url": "{}"}}, {{"packagist.org": false}}]}}"#,
        nobin.display()
    );
    write_files(
        &home,
        &[
            ("composer/config.json", config.as_str()),
            (
                "nobin/composer.json",
                r#"{"name": "acme/nobin", "version": "1.0.0"}"#,
            ),
            ("work/.keep", ""),
        ],
    );
    let work = home.join("work");
    let punx = home.join("punx");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_pun"), &punx).unwrap();

    let dir = home.join("cache/x/acme/nobin/latest");
    let lock = dir.join("composer.lock");
    let age = || {
        let modified = std::fs::metadata(&lock).unwrap().modified().unwrap();
        SystemTime::now().duration_since(modified).unwrap()
    };
    let (out, err, code) = run(&punx, &work, &home, &["acme/nobin"]);
    assert_eq!((out.as_str(), code), ("", Some(2)), "{err}");
    assert!(
        err.contains(&format!("Installing acme/nobin into {}", dir.display())),
        "{err}"
    );
    assert!(
        err.contains("acme/nobin has no commands to run: its composer.json lists no bin"),
        "{err}"
    );
    // A day-old install looks for a newer release; when that fails, the run keeps the install, and the next
    // run looks again.
    let day = Duration::from_hours(24);
    let stale = SystemTime::now() - 3 * day;
    let set_age = |t: SystemTime| {
        let file = std::fs::File::options().append(true).open(&lock).unwrap();
        file.set_modified(t).unwrap();
    };
    set_age(stale);
    let (_, err, code) = run(&punx, &work, &home, &["acme/nobin"]);
    assert_eq!(code, Some(2), "{err}");
    assert!(!err.contains("Installing acme/nobin"), "{err}");
    assert!(age() < day, "the refreshed install's lock is fresh");
    set_age(stale);
    std::fs::rename(&nobin, home.join("nobin.gone")).unwrap();
    let (_, err, code) = run(&punx, &work, &home, &["acme/nobin"]);
    assert_eq!(code, Some(2), "{err}");
    assert!(
        err.contains(" ! couldn't look for a newer acme/nobin, so this run uses the one installed"),
        "{err}"
    );
    assert!(err.contains("acme/nobin has no commands to run"), "{err}");
    assert!(age() > 2 * day && dir.join("vendor/composer/installed.json").is_file());

    // A refresh the solver refuses, here because the installed release is now on a malware list, doesn't fall back
    // on the install: that release is the one refused.
    let listed = r#"{"repositories": [{"type": "package", "package": [{"name": "acme/nobin", "version": "1.0.0"}],
        "filter": {"malware": [{"package": "acme/nobin", "constraint": "*"}]}}, {"packagist.org": false}]}"#;
    write_files(&home, &[("composer/config.json", listed)]);
    let (_, err, code) = run(&punx, &work, &home, &["acme/nobin"]);
    assert_eq!(code, Some(2), "{err}");
    assert!(
        err.contains("acme/nobin can't be refreshed (exit 2), so the one installed isn't run"),
        "{err}"
    );
    assert!(!err.contains("has no commands to run"), "{err}");

    // A broken install that the run can't repair is removed, so the next run starts over.
    write_files(
        &home,
        &[("cache/x/acme/garbage/latest/composer.lock", "nope")],
    );
    let (_, err, code) = run(&punx, &work, &home, &["acme/garbage"]);
    assert_eq!(code, Some(2), "{err}");
    let lock = home.join("cache/x/acme/garbage/latest/composer.lock");
    assert!(
        err.contains(&format!("invalid {}", lock.display())),
        "{err}"
    );
    assert!(!home.join("cache/x/acme/garbage").exists());
}

/// `punx` linked to the test's pun in `home`, and an empty work directory to run it in.
fn punx(home: &Path) -> (PathBuf, PathBuf) {
    let work = home.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let punx = home.join("punx");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_pun"), &punx).unwrap();
    (punx, work)
}

/// The stderr of a `punx ARGS` that must fail with exit 2 and print nothing.
fn fails(home: &Path, args: &[&str]) -> String {
    let (out, err, code) = run(&home.join("punx"), &home.join("work"), home, args);
    assert_eq!((out.as_str(), code), ("", Some(2)), "{err}");
    err
}

/// Each run ends where `exec` fails (the installs have no vendor/bin files), so the error names the command picked.
#[test]
fn picks_the_command_to_run() {
    let home = common::scratch("x", "pick");
    let cache = home.join("cache");
    let (punx, work) = punx(&home);
    let error = |args: &[&str]| fails(&home, args);

    let multi = installed(
        &cache,
        "acme/multi",
        "latest",
        &["bin/alpha", "bin/multi", "bin/beta"],
    );
    let cant_run =
        |dir: &Path, command: &str| format!("can't run {}/vendor/bin/{command}", dir.display());
    let err = error(&["acme/multi", "beta", "--x"]);
    assert!(err.contains(&cant_run(&multi, "beta")), "{err}");
    let err = error(&["acme/multi:", "gamma"]);
    assert!(err.contains(&cant_run(&multi, "multi")), "{err}");
    // `^1.0 || >=9` percent-encoded byte by byte: ^ 5E, space 20, | 7C, > 3E, = 3D.
    let pinned = installed(
        &cache,
        "acme/multi",
        "%5E1.0%20%7C%7C%20%3E%3D9",
        &["bin/alpha", "bin/beta"],
    );
    let err = error(&["acme/multi:^1.0 || >=9", "alpha"]);
    assert!(err.contains(&cant_run(&pinned, "alpha")), "{err}");
    let err = error(&["acme/multi@^1.0 || >=9"]);
    assert!(
        err.contains("acme/multi has 2 commands (alpha, beta): name one after the package, as in `punx acme/multi alpha`"),
        "{err}"
    );

    // A dependency declaring the same command has vendor/bin/<command>: it is not run in the package's place.
    let shadowed = installed(&cache, "acme/shadowed", "latest", &["bin/tool"]);
    let json = serde_json::json!({"packages": [
        {"name": "evil/dep", "version": "1.0.0", "bin": ["bin/tool"]},
        {"name": "acme/shadowed", "version": "1.0.0", "bin": ["bin/tool"]},
    ]});
    std::fs::write(
        shadowed.join("vendor/composer/installed.json"),
        json.to_string(),
    )
    .unwrap();
    let err = error(&["acme/shadowed"]);
    assert!(
        err.contains("evil/dep also declares the command tool, so vendor/bin/tool may not be acme/shadowed's; not running it"),
        "{err}"
    );

    let solo = installed(&cache, "acme/solo", "latest", &["bin/only-one"]);
    assert!(error(&["acme/solo", "x"]).contains(&cant_run(&solo, "only-one")));
    // squizlabs/php_codesniffer's composer.json: "bin": ["bin/phpcbf", "bin/phpcs"].
    let phpcs = installed(
        &cache,
        "squizlabs/php_codesniffer",
        "latest",
        &["bin/phpcbf", "bin/phpcs"],
    );
    assert!(error(&["phpcs"]).contains(&cant_run(&phpcs, "phpcs")));
    assert!(error(&["PHPCBF", "-q"]).contains(&cant_run(&phpcs, "phpcbf")));

    let (out, err, code) = run(
        &punx,
        &work,
        &home,
        &["clean", "pint", "acme/multi:^1.0 || >=9"],
    );
    assert_eq!(
        (out.as_str(), code),
        ("Removed 1 install.\n", Some(0)),
        "{err}"
    );
    assert!(!pinned.exists() && multi.exists());
}

#[test]
fn reports_a_broken_install() {
    let home = common::scratch("x", "broken");
    let cache = home.join("cache");
    punx(&home);
    let error = |args: &[&str]| fails(&home, args);

    let none = installed(&cache, "acme/none", "latest", &[]);
    let err = error(&["acme/none"]);
    assert!(
        err.contains("acme/none has no commands to run: its composer.json lists no bin"),
        "{err}"
    );
    std::fs::write(
        none.join("vendor/composer/installed.json"),
        "{\"packages\": [",
    )
    .unwrap();
    assert!(error(&["acme/none"]).contains("invalid installed.json"));
    std::fs::write(
        none.join("vendor/composer/installed.json"),
        "{\"packages\": []}",
    )
    .unwrap();
    let err = error(&["acme/none"]);
    assert!(
        err.contains(&format!(
            "acme/none is not in {}/vendor/composer/installed.json",
            none.display()
        )),
        "{err}"
    );

    // A file where the run needs a directory, and a directory where it writes a file.
    write_files(&cache, &[("x/acme/blocked", "")]);
    let err = error(&["acme/blocked"]);
    assert!(
        err.contains(&format!(
            "can't create {}",
            cache.join("x/acme/blocked/latest").display()
        )),
        "{err}"
    );
    std::fs::create_dir_all(cache.join("x/acme/locked/latest/.lock")).unwrap();
    let err = error(&["acme/locked"]);
    assert!(
        err.contains(&format!(
            "can't write in {}",
            cache.join("x/acme/locked/latest").display()
        )),
        "{err}"
    );
    std::fs::create_dir_all(cache.join("x/acme/unwritten/latest/composer.json")).unwrap();
    let err = error(&["acme/unwritten"]);
    assert!(
        err.contains(&format!(
            "can't write {}",
            cache.join("x/acme/unwritten/latest").display()
        )),
        "{err}"
    );
}

/// Without `PUN_CACHE_DIR`, the cache is the platform's: `$XDG_CACHE_HOME/pun`, else `~/.cache/pun` on Linux.
#[test]
fn finds_the_cache_directory() {
    let home = common::scratch("x", "cache");
    let work = home.join("work");
    std::fs::create_dir_all(&work).unwrap();
    let xdg = home.join("xdg");
    let pun = Path::new(env!("CARGO_BIN_EXE_pun"));
    let list = |env: &[(&str, &Path)]| run_with(pun, &work, &home, env, &["x", "list"]);
    installed(&xdg.join("pun"), "acme/in-xdg", "latest", &[]);
    if cfg!(target_os = "macos") {
        installed(
            &home.join("Library/Caches/pun"),
            "acme/in-home",
            "latest",
            &[],
        );
    } else {
        installed(&home.join(".cache/pun"), "acme/in-home", "latest", &[]);
    }

    let (out, err, code) = list(&[("HOME", &home)]);
    assert_eq!(
        (out.as_str(), code),
        ("acme/in-home  1.0.0\n", Some(0)),
        "{err}"
    );
    let (out, err, code) = list(&[
        ("HOME", &home),
        ("XDG_CACHE_HOME", &xdg),
        ("PUN_CACHE_DIR", Path::new("")),
    ]);
    let expected = if cfg!(target_os = "macos") {
        "acme/in-home  1.0.0\n"
    } else {
        "acme/in-xdg  1.0.0\n"
    };
    assert_eq!((out.as_str(), code), (expected, Some(0)), "{err}");
    let (out, err, code) = list(&[]);
    assert_eq!((out.as_str(), code), ("", Some(2)), "{err}");
    assert!(err.contains("HOME is not set; set PUN_CACHE_DIR"), "{err}");

    // An install no run has locked (it has no .lock) is removed all the same.
    let home_env: &[(&str, &Path)] = &[("HOME", &home)];
    let (out, err, code) = run_with(pun, &work, &home, home_env, &["x", "clean"]);
    assert_eq!(
        (out.as_str(), code),
        ("Removed 1 install.\n", Some(0)),
        "{err}"
    );
    assert_eq!(list(home_env).0, "No packages are installed.\n");
}
