#![cfg(unix)]
use pun_packages::autoload::{Apcu, Dumped, DumpedFile, Scan};
use pun_packages::fetch::{Auth, Fetcher};
use pun_packages::install::{Added, Options, Report, Updated, dump_autoload, install};
use pun_packages::lock::Lock;
use pun_packages::lock::LockedPackage;
use pun_packages::version::IgnorePlatform;
use serde_json::{Map, Value};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

fn copy(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        let kind = entry.file_type().unwrap();
        if kind.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(entry.path()).unwrap(), &target).unwrap();
        } else if kind.is_dir() {
            copy(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn try_run(dir: &Path, lock: &Lock, o: &Options) -> pun_packages::error::Result<Report> {
    let manifest: Map<String, Value> =
        serde_json::from_str(&std::fs::read_to_string(dir.join("composer.json")).unwrap()).unwrap();
    let fetcher = Fetcher::new(Some(dir.join("cache")), Auth::default(), true);
    let mut events = vec![];
    let report = install(
        dir,
        &manifest,
        lock,
        &fetcher,
        o,
        &|_| None,
        &mut |e: &str| {
            events.push(e.to_string());
            Ok(())
        },
    )?;
    let expected: &[&str] = if o.no_autoloader {
        &[]
    } else {
        &["pre-autoload-dump", "post-autoload-dump"]
    };
    assert_eq!(events, expected);
    Ok(report)
}

fn added(name: &str, version: &str) -> Added {
    Added {
        name: name.into(),
        version: version.into(),
    }
}

fn run(dir: &Path, lock: &Lock, o: &Options) -> Report {
    try_run(dir, lock, o).unwrap()
}

/// Composer's classmap walks directories in read order, so the ambiguous-class warnings and the alternatives after
/// the first file come in whatever order the filesystem returns them.
fn unordered(warning: &str) -> String {
    let Some((head, rest)) = warning.split_once("\" and ") else {
        return warning.to_string();
    };
    let (others, tail) = rest.split_once(", the first").unwrap();
    let mut others: Vec<&str> = others.split(", ").collect();
    others.sort_unstable();
    format!("{head}\" and {}, the first{tail}", others.join(", "))
}

fn same_as_composer(dir: &Path, expected: &str) {
    let expected = Path::new(FIXTURES).join(expected);
    let mut files = vec![("autoload.php".to_string(), expected.join("autoload.php"))];
    for sub in ["composer", "bin"] {
        for entry in std::fs::read_dir(expected.join(sub)).unwrap() {
            let entry = entry.unwrap();
            files.push((
                format!("{sub}/{}", entry.file_name().to_string_lossy()),
                entry.path(),
            ));
        }
    }
    // Bytes, not text: a class name can be any byte above 0x7f, as PHP reads it.
    let apcu = |mut bytes: Vec<u8>| {
        let needle = b"setApcuPrefix('";
        let Some(at) = bytes.windows(needle.len()).position(|w| w == needle) else {
            return bytes;
        };
        let prefix = at + 15..at + 35;
        assert!(
            bytes[prefix.clone()].iter().all(u8::is_ascii_hexdigit)
                && bytes[at + 35..].starts_with(b"')")
        );
        bytes.splice(prefix, b"<random>".iter().copied());
        bytes
    };
    for (name, want) in &files {
        let got = std::fs::read(dir.join("vendor").join(name))
            .unwrap_or_else(|_| panic!("vendor/{name} missing"));
        let (got, want) = (apcu(got), apcu(std::fs::read(want).unwrap()));
        assert!(
            got == want,
            "vendor/{name}:\n{}",
            String::from_utf8_lossy(&got)
        );
    }
    let bins = std::fs::read_dir(dir.join("vendor/bin")).unwrap().count();
    assert_eq!(
        bins,
        std::fs::read_dir(expected.join("bin")).unwrap().count()
    );
}

#[test]
fn install_matches_composer() {
    let tmp = std::env::temp_dir().join(format!("pun-install-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    copy(&Path::new(FIXTURES).join("project"), &tmp);
    let lock =
        Lock::from_composer_lock(&std::fs::read_to_string(tmp.join("composer.lock")).unwrap())
            .unwrap();

    let report = run(&tmp, &lock, &Options::default());
    assert!(!report.stale_lock);
    assert_eq!(
        report.installed,
        [added("acme/lib", "1.2.0"), added("acme/tools", "0.3.1")]
    );
    same_as_composer(&tmp, "project-expected");
    assert_eq!(
        std::fs::read_link(tmp.join("vendor/acme/lib")).unwrap(),
        Path::new("../../packages/lib/")
    );
    for bin in [
        "vendor/bin/lib",
        "vendor/bin/tool.sh",
        "packages/lib/bin/lib",
    ] {
        assert_eq!(
            std::fs::metadata(tmp.join(bin))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "{bin}"
        );
    }

    let again = run(&tmp, &lock, &Options::default());
    assert_eq!((again.kept, again.installed.len()), (2, 0));

    let no_dev = run(
        &tmp,
        &lock,
        &Options {
            no_dev: true,
            ..Options::default()
        },
    );
    assert_eq!(no_dev.removed, ["acme/tools"]);
    assert!(std::fs::symlink_metadata(tmp.join("vendor/acme/tools")).is_err());
    assert!(!tmp.join("vendor/bin/tool.sh").exists());
    assert!(tmp.join("packages/tools/bin/tool.sh").exists());
    let classmap =
        std::fs::read_to_string(tmp.join("vendor/composer/autoload_classmap.php")).unwrap();
    assert!(
        !classmap.contains("Legacy_Old")
            && !classmap.contains("KernelTest")
            && classmap.contains("App\\\\Kernel")
    );
    std::fs::remove_dir_all(&tmp).unwrap();
}

#[test]
fn autoloader_and_bin_corner_cases_match_composer() {
    let tmp = std::env::temp_dir().join(format!("pun-install-edge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    copy(&Path::new(FIXTURES).join("edge"), &tmp);
    // Messages name real paths, as Composer's realpath() does; macOS's temp dir is behind /var -> /private/var.
    let tmp = tmp.canonicalize().unwrap();
    let lock =
        Lock::from_composer_lock(&std::fs::read_to_string(tmp.join("composer.lock")).unwrap())
            .unwrap();
    let o = Options {
        ignore_platform: IgnorePlatform::Listed(vec!["php".into(), "ext-sodium-*".into()]),
        ..Options::default()
    };
    let report = run(&tmp, &lock, &o);
    same_as_composer(&tmp, "edge-expected");
    let prefixed = Options {
        apcu: Apcu::Prefix("mine".into()),
        ..o.clone()
    };
    run(&tmp, &lock, &prefixed);
    let real = std::fs::read_to_string(tmp.join("vendor/composer/autoload_real.php")).unwrap();
    assert!(
        real.contains("        $loader->setApcuPrefix('mine');\n"),
        "{real}"
    );
    run(&tmp, &lock, &o);
    let at = tmp.to_string_lossy();
    let mut warnings: Vec<String> = report
        .warnings
        .iter()
        .map(|w| unordered(&w.replace(&*at, "/tmp/edge")))
        .collect();
    warnings.sort();
    let mut want = [
            "Skipped installation of bin bin/nofile for package acme/legacy: file not found in package",
            "Skipped installation of bin bin/ for package acme/legacy: found a directory at that path",
            "Skipped installation of bin bin/escape for package acme/legacy: the bin resolves to a path outside of the package directory",
            "Skipped installation of bin bin/bad$name for package acme/legacy: the bin path contains invalid characters",
            "Skipped installation of bin bin/dup for package acme/legacy: name conflicts with an existing file",
            "Ambiguous class resolution, \"Dup\\Thing\" was found 3x: in \"/tmp/edge/classes/Thing.php\" and \"/tmp/edge/dup/Thing.php\", \"/tmp/edge/dup/Thing2.php\", the first will be used.",
            "Ambiguous class resolution, \"Dup\\Pair\" was found in both \"/tmp/edge/classes/Pair.php\" and \"/tmp/edge/dup/Pair.php\", the first will be used.",
        ]
        .map(unordered);
    want.sort();
    assert_eq!(warnings, want);

    let pair = tmp.join("classes/Pair.php");
    std::fs::set_permissions(&pair, std::fs::Permissions::from_mode(0o000)).unwrap();
    let error = try_run(&tmp, &lock, &o).unwrap_err().to_string();
    std::fs::set_permissions(&pair, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        error,
        format!(
            "File at \"{}\" is not readable, check its permissions",
            pair.display()
        )
    );
    let mut missing = lock;
    let legacy = (missing.packages.iter_mut())
        .find(|p| p["name"] == "acme/legacy")
        .unwrap();
    legacy.0["autoload"]["classmap"] = serde_json::json!(["code/", "gone/"]);
    assert_eq!(
        try_run(&tmp, &missing, &o).unwrap_err().to_string(),
        format!(
            "Could not scan for classes inside \"{}/vendor/acme/legacy/gone/\" which does not appear to be a file nor a folder",
            tmp.display()
        )
    );
    std::fs::remove_dir_all(&tmp).unwrap();
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one test walks every case of the behaviour it checks"
)]
fn lock_changes_plugins_and_failures() {
    let tmp = std::env::temp_dir().join(format!("pun-install-changes-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    copy(&Path::new(FIXTURES).join("project"), &tmp);
    let lock =
        Lock::from_composer_lock(&std::fs::read_to_string(tmp.join("composer.lock")).unwrap())
            .unwrap();
    run(&tmp, &lock, &Options::default());
    let package = |v: Value| LockedPackage(v.as_object().unwrap().clone());

    let mut changed = lock.clone();
    changed.packages[0].0["version"] = "1.2.1".into();
    changed.packages.push(package(
        serde_json::json!({"name": "acme/meta", "version": "1.0.0", "type": "metapackage"}),
    ));
    let report = run(&tmp, &changed, &Options::default());
    assert_eq!(
        report.updated,
        [Updated {
            name: "acme/lib".into(),
            from: "1.2.0".into(),
            to: "1.2.1".into()
        }]
    );
    assert_eq!(report.installed, [added("acme/meta", "1.0.0")]);
    assert!(!tmp.join("vendor/acme/meta").exists());

    let zip = {
        use std::io::Write;
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(vec![]));
        w.start_file(
            "acme-zipped-1/src/Z.php",
            zip::write::SimpleFileOptions::default(),
        )
        .unwrap();
        w.write_all(b"<?php class Z {}\n").unwrap();
        w.finish().unwrap().into_inner()
    };
    let url = "https://repo.acme.test/zipped.zip";
    let cache = tmp.join("cache/files/acme/zipped");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::write(
        cache.join(format!("{}.zip", pun_packages::php::sha1(url.as_bytes()))),
        &zip,
    )
    .unwrap();
    let mut zipped = lock.clone();
    zipped.packages.push(package(serde_json::json!({"name": "acme/zipped", "version": "1.0.0", "dist": {"type": "zip", "url": url, "shasum": pun_packages::php::sha1(&zip)}})));
    let report = run(&tmp, &zipped, &Options::default());
    assert_eq!(report.cached, 1);
    assert!(tmp.join("vendor/acme/zipped/src/Z.php").exists());

    let mut failing = lock.clone();
    failing.packages.push(package(serde_json::json!({"name": "acme/zip", "version": "1.0.0", "dist": {"type": "zip", "url": "http://example.com/zip.zip"}})));
    failing.packages.push(package(serde_json::json!({"name": "acme/src", "version": "1.0.0", "source": {"type": "git", "url": "https://example.com/src.git", "reference": "abc"}})));
    assert_eq!(
        try_run(&tmp, &failing, &Options::default())
            .unwrap_err()
            .to_string(),
        "installing acme/zip: Your configuration does not allow connections to http://example.com/zip.zip. See https://getcomposer.org/doc/06-config.md#secure-http for details.\n\
         installing acme/src: it has no dist archive, and installing from git sources is not supported yet"
    );
    let mut bad = lock.clone();
    bad.packages[0].0["name"] = "Acme/Not Valid".into();
    assert_eq!(
        try_run(&tmp, &bad, &Options::default())
            .unwrap_err()
            .to_string(),
        "composer.lock has a package named `acme/not valid`, which is not a valid package name"
    );

    std::fs::write(
        tmp.join("packages/tools/lib/Acme/Tools/Bad.php"),
        "<?php class Nope {}\n",
    )
    .unwrap();
    std::fs::write(
        tmp.join("src/Wrong.php"),
        "<?php\nnamespace App;\nclass Nope {}\n",
    )
    .unwrap();
    let manifest_path = tmp.join("composer.json");
    let original = std::fs::read_to_string(&manifest_path).unwrap();
    let mut manifest: Value = serde_json::from_str(&original).unwrap();
    manifest["autoload"]["files"] = serde_json::json!(["/dev/null"]);
    std::fs::write(&manifest_path, manifest.to_string()).unwrap();
    let report = run(&tmp, &lock, &Options::default());
    assert_eq!(
        report.warnings,
        [
            "Class App\\Nope located in ./src/Wrong.php does not comply with psr-4 autoloading standard (rule: App\\ => ./src). Skipping."
        ]
    );
    let files = std::fs::read_to_string(tmp.join("vendor/composer/autoload_files.php")).unwrap();
    assert!(
        files.contains("    '63a99e721f6076a94999e44885027d8b' => '/dev/null',\n"),
        "{files}"
    );
    std::fs::write(&manifest_path, original).unwrap();
    std::fs::remove_file(tmp.join("packages/tools/lib/Acme/Tools/Bad.php")).unwrap();
    std::fs::remove_file(tmp.join("src/Wrong.php")).unwrap();

    let ignore_php = Options {
        ignore_platform: IgnorePlatform::Listed(vec!["php".into()]),
        ..Options::default()
    };
    run(&tmp, &lock, &ignore_php);
    assert!(!tmp.join("vendor/composer/platform_check.php").exists());
    assert!(
        !std::fs::read_to_string(tmp.join("vendor/composer/autoload_real.php"))
            .unwrap()
            .contains("platform_check")
    );

    let installed = tmp.join("vendor/composer/installed.json");
    let mut json: Value =
        serde_json::from_str(&std::fs::read_to_string(&installed).unwrap()).unwrap();
    json["packages"].as_array_mut().unwrap().push(serde_json::json!({"name": "evil/escape", "version": "1.0.0", "install-path": "../../packages/lib"}));
    std::fs::write(&installed, json.to_string()).unwrap();
    let report = run(&tmp, &lock, &Options::default());
    assert_eq!(report.removed, ["evil/escape"]);
    assert!(tmp.join("packages/lib/composer.json").exists());

    let mut no_files = lock.clone();
    no_files.packages.clear();
    run(&tmp, &no_files, &Options::default());
    assert!(!tmp.join("vendor/composer/autoload_files.php").exists());
    assert!(
        !std::fs::read_to_string(tmp.join("vendor/composer/autoload_real.php"))
            .unwrap()
            .contains("$filesToLoad")
    );
    let no_autoloader = Options {
        no_autoloader: true,
        ..Options::default()
    };
    run(&tmp, &lock, &no_autoloader);
    assert!(!tmp.join("vendor/composer/autoload_files.php").exists());

    let mut manifest: Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    manifest["config"]["platform-check"] = false.into();
    manifest["config"]
        .as_object_mut()
        .unwrap()
        .remove("optimize-autoloader");
    manifest["config"]["allow-plugins"] =
        serde_json::json!({"acme/plugin": true, "pestphp/pest-plugin": true});
    std::fs::write(&manifest_path, manifest.to_string()).unwrap();
    let mut plugins = lock;
    for name in ["acme/plugin", "pestphp/pest-plugin"] {
        plugins.packages.push(package(serde_json::json!({"name": name, "version": "1.0.0", "type": "composer-plugin", "dist": {"type": "path", "url": "packages/lib", "reference": "x"}})));
    }
    run(&tmp, &plugins, &Options::default());
    assert!(!tmp.join("vendor/composer/platform_check.php").exists());
    assert!(tmp.join("vendor/pest-plugins.json").exists());
    let classmap =
        std::fs::read_to_string(tmp.join("vendor/composer/autoload_classmap.php")).unwrap();
    assert!(
        classmap.contains("KernelTest") && !classmap.contains("App\\\\Kernel'"),
        "{classmap}"
    );
    std::fs::remove_file(tmp.join("vendor/pest-plugins.json")).unwrap();
    let no_plugins = Options {
        no_plugins: true,
        ..Options::default()
    };
    let report = run(&tmp, &plugins, &no_plugins);
    assert!(!tmp.join("vendor/pest-plugins.json").exists() && report.warnings.is_empty());
    std::fs::remove_dir_all(&tmp).unwrap();
}

/// The autoloader kept in the cache gives back the bytes it was given: a class name from a Latin-1 file is the byte
/// 0xA9, not U+FFFD.
#[test]
fn a_kept_dump_keeps_bytes_that_are_not_utf8() {
    let dir = std::env::temp_dir().join(format!("pun-kept-dump-{}", std::process::id()));
    let file = dir.join("dump.txt");
    let dumped = Dumped {
        files: vec![DumpedFile {
            path: "vendor/composer/autoload_classmap.php".into(),
            bytes: Some(b"    '\xa9' => $baseDir . '/classes/Latin1.php',\n".to_vec()),
        }],
        classes: 1,
        warnings: vec![],
    };
    dumped.save(&file, "key");
    assert_eq!(Dumped::load(&file, "key").unwrap().files, dumped.files);
    std::fs::remove_dir_all(&dir).unwrap();
}

/// pest-plugins.json lists the plugins in the order Composer 2.11.2's local repository holds the packages: on a
/// fresh install the operation order (acme/zeta before acme/alpha, which requires it), after an update the kept
/// packages then the updated one, and with nothing to do installed.json's name order.
#[test]
fn pest_plugins_follow_composers_repository_order() {
    let tmp = std::env::temp_dir().join(format!("pun-pest-order-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    copy(&Path::new(FIXTURES).join("pest-order"), &tmp);
    let mut lock =
        Lock::from_composer_lock(&std::fs::read_to_string(tmp.join("composer.lock")).unwrap())
            .unwrap();
    let plugins = |lock: &Lock| {
        run(&tmp, lock, &Options::default());
        std::fs::read_to_string(tmp.join("vendor/pest-plugins.json")).unwrap()
    };
    let list = |names: [&str; 4]| {
        let items: Vec<String> = names
            .iter()
            .map(|n| format!("    \"Acme\\\\{n}\""))
            .collect();
        format!("[\n{}\n]", items.join(",\n"))
    };
    assert_eq!(plugins(&lock), list(["Zeta", "Alpha", "Mid", "Root"]));
    let mid = (lock.packages.iter_mut())
        .find(|p| p.name() == "acme/mid")
        .unwrap();
    mid.0.insert("version".into(), "1.1.0".into());
    mid.0["dist"]["reference"] = "cf5881339893fa536b9c4d9194d36162dd9e76f3".into();
    assert_eq!(plugins(&lock), list(["Alpha", "Zeta", "Mid", "Root"]));
    assert_eq!(plugins(&lock), list(["Alpha", "Mid", "Zeta", "Root"]));
    std::fs::remove_dir_all(&tmp).unwrap();
}

/// The recorder plugin logs each event it hears with its listener priority, beside the root package's scripts. The
/// expected logs are Composer 2.11.2's on the same fixture: a plugin installed by the command activates mid-command
/// and never hears `init`, and an upgraded one is re-activated before the autoload dump.
#[test]
fn composer_plugins_run_on_composers_classes() {
    use pun_packages::plugin_host::Dispatcher;
    let tmp = std::env::temp_dir().join(format!("pun-plugin-host-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    copy(&Path::new(FIXTURES).join("plugins"), &tmp);
    let manifest: Map<String, Value> =
        serde_json::from_str(&std::fs::read_to_string(tmp.join("composer.json")).unwrap()).unwrap();
    let mut lock =
        Lock::from_composer_lock(&std::fs::read_to_string(tmp.join("composer.lock")).unwrap())
            .unwrap();
    let cache = tmp.join("cache");
    std::fs::create_dir_all(cache.join("pun")).unwrap();
    std::fs::write(cache.join("pun/composer-2.11.2.phar"), "not composer").unwrap();
    let fetcher = Fetcher::new(Some(cache.clone()), Auth::default(), true);
    let events = |lock: &Lock, command: &str| {
        let mut d = Dispatcher::new(&tmp, &manifest, true, true, true, Some(&cache), &|_| None);
        d.dispatch(&format!("pre-{command}-cmd")).unwrap();
        let o = Options::default();
        install(&tmp, &manifest, lock, &fetcher, &o, &|_| None, &mut |e| {
            d.dispatch(e)
        })
        .unwrap();
        d.dispatch(&format!("post-{command}-cmd")).unwrap();
        let log = std::fs::read_to_string(tmp.join("events.log")).unwrap();
        std::fs::remove_file(tmp.join("events.log")).unwrap();
        log
    };
    let dumped = "pre-autoload-dump plugin 0 1\npost-autoload-dump plugin 10 1\npost-autoload-dump plugin 0 1\npost-autoload-dump script 1\npost-autoload-dump plugin -10 1\n";
    assert_eq!(
        events(&lock, "install"),
        format!(
            "activate acme/app\n{dumped}post-install-cmd plugin 0 1\npost-install-cmd script 1\n"
        )
    );
    let recorder = (lock.packages.iter_mut())
        .find(|p| p.name() == "acme/recorder")
        .unwrap();
    recorder.0.insert("version".into(), "1.1.0".into());
    assert_eq!(
        events(&lock, "update"),
        format!(
            "activate acme/app\ninit plugin 0 \npre-update-cmd plugin 0 1\nactivate acme/app\n{dumped}post-update-cmd plugin 0 1\n"
        )
    );

    let post_install = |manifest: &Map<String, Value>, scripts, plugins| {
        let mut d = Dispatcher::new(
            &tmp,
            manifest,
            true,
            scripts,
            plugins,
            Some(&cache),
            &|_| None,
        );
        d.dispatch("post-install-cmd").unwrap();
        let log = std::fs::read_to_string(tmp.join("events.log")).unwrap_or_default();
        let _ = std::fs::remove_file(tmp.join("events.log"));
        log
    };
    let started = "activate acme/app\ninit plugin 0 \n";
    assert_eq!(
        post_install(&manifest, false, true),
        format!("{started}post-install-cmd plugin 0 1\n")
    );
    assert_eq!(
        post_install(&manifest, true, false),
        "post-install-cmd script 1\n"
    );
    let mut denied = manifest.clone();
    denied["config"]["allow-plugins"]["acme/recorder"] = false.into();
    assert_eq!(post_install(&denied, false, true), "");

    let failure = |fail: &str, php: Option<&str>| {
        std::fs::write(tmp.join("fail"), fail).unwrap();
        let env = |k: &str| (k == "PHP_BINARY").then(|| php.map(String::from)).flatten();
        let mut d = Dispatcher::new(&tmp, &manifest, false, true, true, Some(&cache), &env);
        d.dispatch("post-install-cmd").map_err(|e| format!("{e:#}"))
    };
    assert_eq!(failure("post-install-cmd echo", None), Ok(()));
    assert_eq!(
        failure("post-install-cmd throw", None).unwrap_err(),
        "a Composer plugin failed on the post-install-cmd event: the recorder failed"
    );
    assert_eq!(
        failure("post-install-cmd exit", None).unwrap_err(),
        "a Composer plugin failed on the post-install-cmd event: the Composer plugin host stopped"
    );
    let missing = tmp.join("no-php");
    assert!(
        failure("", missing.to_str())
            .unwrap_err()
            .starts_with(&format!(
                "can't run {} for the Composer plugins: ",
                missing.display()
            ))
    );
    let script = format!("pun-plugin-host-{}.php", std::process::id());
    assert!(!std::env::temp_dir().join(script).exists());
    std::fs::remove_dir_all(&tmp).unwrap();
}

/// The no-op fast path: a recorded install with the same inputs and untouched outputs writes nothing, and any
/// change to what the autoloader reads or to what it wrote falls back to a full install.
#[test]
fn a_no_op_install_is_skipped_until_an_input_changes() {
    let tmp = std::env::temp_dir().join(format!("pun-install-noop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    copy(&Path::new(FIXTURES).join("project"), &tmp);
    let lock =
        Lock::from_composer_lock(&std::fs::read_to_string(tmp.join("composer.lock")).unwrap())
            .unwrap();
    let o = Options {
        php: Some("php".into()),
        cache: Some(tmp.join("pun-cache")),
        ..Options::default()
    };
    let full = run(&tmp, &lock, &o);
    assert!(!full.fresh);
    // Nothing is recorded until the command has succeeded, scripts included.
    assert!(!run(&tmp, &lock, &o).fresh);
    let saved = run(&tmp, &lock, &o);
    saved.fingerprint.as_ref().unwrap().save();
    let fresh = run(&tmp, &lock, &o);
    assert!(fresh.fresh && fresh.fingerprint.is_none());
    assert_eq!(
        (fresh.kept, fresh.classes, &fresh.warnings),
        (2, saved.classes, &saved.warnings)
    );
    same_as_composer(&tmp, "project-expected");

    // A new root class: the dump runs again and finds it.
    std::fs::write(
        tmp.join("src/Added.php"),
        "<?php\nnamespace App;\nclass Added {}\n",
    )
    .unwrap();
    let rescanned = run(&tmp, &lock, &o);
    assert!(!rescanned.fresh);
    assert_eq!(rescanned.classes, saved.classes + 1);
    rescanned.fingerprint.unwrap().save();
    assert!(run(&tmp, &lock, &o).fresh);

    // An edited output is written again.
    let classmap = tmp.join("vendor/composer/autoload_classmap.php");
    let want = std::fs::read(&classmap).unwrap();
    std::fs::write(&classmap, "<?php return [];\n").unwrap();
    let repaired = run(&tmp, &lock, &o);
    assert!(!repaired.fresh);
    assert_eq!(std::fs::read(&classmap).unwrap(), want);
    repaired.fingerprint.unwrap().save();

    // With the same inputs, the dump is the one kept from the last: what was kept is what gets written.
    let kept_dir = tmp.join("pun-cache/autoload/v1");
    let kept: Vec<PathBuf> = (std::fs::read_dir(&kept_dir).unwrap())
        .map(|e| e.unwrap().path())
        .collect();
    assert_eq!(kept.len(), 1);
    let text = std::fs::read_to_string(&kept[0]).unwrap();
    std::fs::write(&kept[0], text.replace("/src/Added.php", "/src/Kept.php")).unwrap();
    std::fs::remove_file(&classmap).unwrap();
    run(&tmp, &lock, &o).fingerprint.unwrap().save();
    assert!(
        String::from_utf8(std::fs::read(&classmap).unwrap())
            .unwrap()
            .contains("/src/Kept.php")
    );

    // A path repository's source is an input, so a class added there (seen through a symlinked package) is found.
    std::fs::write(
        tmp.join("packages/lib/src/Extra.php"),
        "<?php\nnamespace Acme\\Lib;\nclass Extra {}\n",
    )
    .unwrap();
    let edited = run(&tmp, &lock, &o);
    assert!(!edited.fresh);
    assert_eq!(edited.classes, rescanned.classes + 1);
    edited.fingerprint.unwrap().save();

    // Other options are other inputs; a random APCu prefix changes every dump, so it is never skipped.
    let no_dev = Options {
        no_dev: true,
        ..o.clone()
    };
    assert!(!run(&tmp, &lock, &no_dev).fresh);
    let apcu = Options {
        apcu: Apcu::RandomPrefix,
        ..o
    };
    assert!(run(&tmp, &lock, &apcu).fingerprint.is_none());
    std::fs::remove_dir_all(&tmp).unwrap();
}

/// `composer install` refuses a lock this PHP can't run (fixture `verify/A`, Composer exit 2) before it writes
/// anything.
#[test]
fn an_incompatible_lock_installs_nothing() {
    let tmp = std::env::temp_dir().join(format!("pun-install-platform-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    copy(&Path::new(FIXTURES).join("verify/A"), &tmp);
    let lock =
        Lock::from_composer_lock(&std::fs::read_to_string(tmp.join("composer.lock")).unwrap())
            .unwrap();
    let o = Options {
        php: Some("php".into()),
        cache: Some(tmp.join("pun-cache")),
        ..Options::default()
    };
    let manifest: Map<String, Value> =
        serde_json::from_str(&std::fs::read_to_string(tmp.join("composer.json")).unwrap()).unwrap();
    let fetcher = Fetcher::new(None, Auth::default(), true);
    let report = install(
        &tmp,
        &manifest,
        &lock,
        &fetcher,
        &o,
        &|_| None,
        &mut |e: &str| panic!("no event expected, got {e}"),
    )
    .unwrap();
    let problems = report.incompatible.unwrap();
    assert!(
        problems.contains("requires PHP extension ext-nope ^1.0 but it is missing"),
        "{problems}"
    );
    assert!(!tmp.join("vendor").exists());
    // Only a check that passed is remembered: the same lock is refused again.
    let again = install(
        &tmp,
        &manifest,
        &lock,
        &fetcher,
        &o,
        &|_| None,
        &mut |e: &str| panic!("no event expected, got {e}"),
    )
    .unwrap();
    assert_eq!(again.incompatible.as_deref(), Some(problems.as_str()));
    let ignored = Options {
        ignore_platform: IgnorePlatform::All,
        ..o
    };
    assert!(run(&tmp, &lock, &ignored).incompatible.is_none());
    std::fs::remove_dir_all(&tmp).unwrap();
}

/// `composer dump-autoload` on the fixture project after `composer install` (and then `composer install --no-dev`):
/// `dump-expected/<case>` holds the files that differ from `default`, recorded from Composer 2.11.2 with
/// `--no-scripts` and the flags each case is named after.
#[test]
fn dump_autoload_matches_composer() {
    let tmp = std::env::temp_dir().join(format!("pun-dump-autoload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    copy(&Path::new(FIXTURES).join("project"), &tmp);
    let lock =
        Lock::from_composer_lock(&std::fs::read_to_string(tmp.join("composer.lock")).unwrap())
            .unwrap();
    let manifest: Map<String, Value> =
        serde_json::from_str(&std::fs::read_to_string(tmp.join("composer.json")).unwrap()).unwrap();
    let expected = Path::new(FIXTURES).join("dump-expected");
    let check = |case: &str, o: &Options, dev: Option<bool>, want_dev: bool| {
        let mut events = vec![];
        let report = dump_autoload(&tmp, &manifest, Some(&lock), o, dev, &mut |e, dev| {
            events.push((e.to_string(), dev));
            Ok(())
        })
        .unwrap();
        assert_eq!(
            events,
            [
                ("pre-autoload-dump".to_string(), want_dev),
                ("post-autoload-dump".to_string(), want_dev)
            ],
            "{case}"
        );
        assert!(!report.missing);
        let mut files = vec!["autoload.php".to_string()];
        for e in std::fs::read_dir(expected.join("default/composer")).unwrap() {
            files.push(format!(
                "composer/{}",
                e.unwrap().file_name().to_string_lossy()
            ));
        }
        for f in files {
            let want = [
                expected.join(case).join(&f),
                expected.join("default").join(&f),
            ]
            .into_iter()
            .find(|p| p.exists())
            .unwrap();
            assert_eq!(
                std::fs::read_to_string(tmp.join("vendor").join(&f)).unwrap(),
                std::fs::read_to_string(want).unwrap(),
                "{case}: vendor/{f}"
            );
        }
        report.classes
    };
    run(&tmp, &lock, &Options::default());
    assert_eq!(check("default", &Options::default(), None, true), 7);
    let authoritative = Options {
        scan: Scan::Authoritative,
        ..Options::default()
    };
    assert_eq!(check("authoritative", &authoritative, None, true), 7);
    assert_eq!(check("no-dev", &Options::default(), Some(false), false), 3);
    let apcu = Options {
        apcu: Apcu::Prefix("mine".into()),
        ..Options::default()
    };
    check("apcu", &apcu, None, true);
    let no_dev = Options {
        no_dev: true,
        ..Options::default()
    };
    run(&tmp, &lock, &no_dev);
    assert_eq!(
        check("installed-no-dev", &Options::default(), None, false),
        3
    );
    assert_eq!(check("dev", &Options::default(), Some(true), true), 4);

    std::fs::remove_dir_all(tmp.join("packages/lib")).unwrap();
    let report = dump_autoload(
        &tmp,
        &manifest,
        None,
        &Options::default(),
        None,
        &mut |_, _| Ok(()),
    )
    .unwrap();
    assert!(report.missing && !report.dev_mode);
    std::fs::remove_dir_all(&tmp).unwrap();
}

/// A dist archive is extracted once into the store: a second project (here, the same one after `vendor/` is
/// deleted and the download cache emptied) is linked from it, and the classes scanned under it are read back from the
/// class cache instead of the files.
#[test]
fn packages_come_from_the_store_and_their_classes_from_the_class_cache() {
    use std::io::Write;
    let tmp = std::env::temp_dir().join(format!("pun-install-store-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    copy(&Path::new(FIXTURES).join("project"), &tmp);
    let lock =
        Lock::from_composer_lock(&std::fs::read_to_string(tmp.join("composer.lock")).unwrap())
            .unwrap();
    let zip = {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(vec![]));
        let options = zip::write::SimpleFileOptions::default();
        for (name, code) in [
            ("acme-zipped-1/src/Z.php", "<?php class Z {}\n"),
            ("acme-zipped-1/lib/One.php", "<?php class One {}\n"),
        ] {
            w.start_file(name, options).unwrap();
            w.write_all(code.as_bytes()).unwrap();
        }
        w.finish().unwrap().into_inner()
    };
    let url = "https://repo.acme.test/zipped.zip";
    let archive = tmp
        .join("cache/files/acme/zipped")
        .join(format!("{}.zip", pun_packages::php::sha1(url.as_bytes())));
    std::fs::create_dir_all(archive.parent().unwrap()).unwrap();
    std::fs::write(&archive, &zip).unwrap();
    let mut zipped = lock;
    zipped.packages.push(LockedPackage(
        serde_json::json!({"name": "acme/zipped", "version": "1.0.0", "dist": {"type": "zip", "url": url, "shasum": pun_packages::php::sha1(&zip)}, "autoload": {"classmap": ["src/", "lib/One.php"]}})
            .as_object()
            .unwrap()
            .clone(),
    ));
    let o = Options {
        cache: Some(tmp.join("pun-cache")),
        link: pun_packages::store::LinkMode::Hardlink,
        ..Options::default()
    };
    let classmap =
        || std::fs::read_to_string(tmp.join("vendor/composer/autoload_classmap.php")).unwrap();
    // The archive comes from Composer's download cache the first time, from the store (it is deleted) the second.
    let first = run(&tmp, &zipped, &o);
    assert_eq!(first.cached, 1);
    assert!(classmap().contains("'Z' => $vendorDir . '/acme/zipped/src/Z.php'"));
    let z = tmp.join("vendor/acme/zipped/src/Z.php");
    assert!(std::fs::metadata(&z).unwrap().nlink() > 1);

    std::fs::remove_dir_all(tmp.join("vendor")).unwrap();
    std::fs::remove_file(&archive).unwrap();
    // The cached classes are what the next dump reads (once the dump kept from this one is gone): change them, and
    // the class map follows.
    std::fs::remove_dir_all(tmp.join("pun-cache/autoload")).unwrap();
    let mut cached = 0;
    for dir in std::fs::read_dir(tmp.join("pun-cache/classes/v1")).unwrap() {
        for file in std::fs::read_dir(dir.unwrap().path()).unwrap() {
            let file = file.unwrap().path();
            let text = std::fs::read_to_string(&file).unwrap();
            std::fs::write(&file, text.replace("\"Z\"", "\"FromCache\"")).unwrap();
            cached += 1;
        }
    }
    assert_eq!(cached, 2);
    let second = run(&tmp, &zipped, &o);
    assert_eq!(second.cached, 1);
    assert_eq!(std::fs::read_to_string(&z).unwrap(), "<?php class Z {}\n");
    assert!(classmap().contains("'FromCache' => $vendorDir . '/acme/zipped/src/Z.php'"));
    assert!(classmap().contains("'One' => $vendorDir . '/acme/zipped/lib/One.php'"));

    // Switched to the copy mode, the kept package stops sharing its files with the store.
    let copied = Options {
        link: pun_packages::store::LinkMode::Copy,
        ..o.clone()
    };
    let third = run(&tmp, &zipped, &copied);
    assert!(!third.fresh);
    assert_eq!(std::fs::metadata(&z).unwrap().nlink(), 1);
    assert_eq!(std::fs::read_to_string(&z).unwrap(), "<?php class Z {}\n");

    // A source install is a checkout, which the class cache can't vouch for: it is scanned.
    let installed = tmp.join("vendor/composer/installed.json");
    let json = std::fs::read_to_string(&installed).unwrap().replace(
        "\"installation-source\": \"dist\"",
        "\"installation-source\": \"source\"",
    );
    std::fs::write(&installed, json).unwrap();
    let manifest: Map<String, Value> =
        serde_json::from_str(&std::fs::read_to_string(tmp.join("composer.json")).unwrap()).unwrap();
    dump_autoload(&tmp, &manifest, None, &o, None, &mut |_, _| Ok(())).unwrap();
    assert!(classmap().contains("'Z' => $vendorDir . '/acme/zipped/src/Z.php'"));
    assert!(!classmap().contains("FromCache"));
    std::fs::remove_dir_all(&tmp).unwrap();
}

/// A symlink planted in vendor/ never takes a write or a removal outside it: a stale package below a linked
/// directory is refused, and a link in place of a generated file or a bin proxy is replaced, not written through.
#[test]
fn vendor_symlinks_do_not_redirect_writes_or_removals() {
    let tmp = std::env::temp_dir().join(format!("pun-install-confined-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    copy(&Path::new(FIXTURES).join("project"), &tmp);
    let lock =
        Lock::from_composer_lock(&std::fs::read_to_string(tmp.join("composer.lock")).unwrap())
            .unwrap();
    run(&tmp, &lock, &Options::default());
    let outside = tmp.join("outside");
    std::fs::create_dir_all(outside.join("gone")).unwrap();
    std::fs::write(outside.join("gone/keep.php"), "keep").unwrap();
    std::fs::write(outside.join("file"), "keep").unwrap();
    std::os::unix::fs::symlink(&outside, tmp.join("vendor/evil")).unwrap();
    let installed = tmp.join("vendor/composer/installed.json");
    let original = std::fs::read_to_string(&installed).unwrap();
    let mut json: Value = serde_json::from_str(&original).unwrap();
    json["packages"].as_array_mut().unwrap().push(
        serde_json::json!({"name": "evil/gone", "version": "1.0.0", "install-path": "../evil/gone"}),
    );
    std::fs::write(&installed, json.to_string()).unwrap();
    let error = format!(
        "{:#}",
        try_run(&tmp, &lock, &Options::default()).unwrap_err()
    );
    assert!(error.contains("is a symlink that leads outside"), "{error}");
    assert!(outside.join("gone/keep.php").exists());

    std::fs::write(&installed, &original).unwrap();
    for file in ["vendor/composer/installed.php", "vendor/bin/lib"] {
        std::fs::remove_file(tmp.join(file)).unwrap();
        std::os::unix::fs::symlink(outside.join("file"), tmp.join(file)).unwrap();
    }
    run(&tmp, &lock, &Options::default());
    assert_eq!(
        std::fs::read_to_string(outside.join("file")).unwrap(),
        "keep"
    );
    assert_eq!(
        std::fs::metadata(outside.join("file")).unwrap().mode() & 0o777,
        std::fs::metadata(outside.join("gone/keep.php"))
            .unwrap()
            .mode()
            & 0o777
    );
    for file in ["vendor/composer/installed.php", "vendor/bin/lib"] {
        assert!(std::fs::symlink_metadata(tmp.join(file)).unwrap().is_file());
    }
    std::fs::remove_dir_all(&tmp).unwrap();
}

/// What else the no-op fingerprint covers: the git refs the root version comes from (a new commit is a new
/// `reference` in installed.php), no record at all when `.git` is a file (a worktree, whose refs live elsewhere), a
/// root with its own `version`, and `--no-autoloader`, whose no-op runs no dump events.
#[test]
fn the_fingerprint_follows_git_and_the_root_version() {
    let tmp = std::env::temp_dir().join(format!("pun-install-git-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    copy(&Path::new(FIXTURES).join("project"), &tmp);
    let lock =
        Lock::from_composer_lock(&std::fs::read_to_string(tmp.join("composer.lock")).unwrap())
            .unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(&tmp)
            .env_remove("GIT_DIR")
            .output()
            .unwrap();
        assert!(out.status.success(), "{out:?}");
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    };
    git(&["init", "-q", "-b", "main"]);
    git(&["commit", "-q", "--allow-empty", "-m", "one"]);
    let o = Options {
        cache: Some(tmp.join("pun-cache")),
        ..Options::default()
    };
    let installed_php =
        || std::fs::read_to_string(tmp.join("vendor/composer/installed.php")).unwrap();
    // The first install makes the path packages' bins executable in their sources, as Composer does: an input.
    run(&tmp, &lock, &o);
    run(&tmp, &lock, &o).fingerprint.unwrap().save();
    assert!(run(&tmp, &lock, &o).fresh);
    git(&["commit", "-q", "--allow-empty", "-m", "two"]);
    let head = git(&["rev-parse", "HEAD"]);
    assert!(!installed_php().contains(&head));
    let moved = run(&tmp, &lock, &o);
    assert!(!moved.fresh);
    assert!(installed_php().contains(&format!("'reference' => '{head}'")));

    std::fs::remove_dir_all(tmp.join(".git")).unwrap();
    std::fs::write(tmp.join(".git"), "gitdir: /elsewhere\n").unwrap();
    assert!(run(&tmp, &lock, &o).fingerprint.is_none());

    let json = std::fs::read_to_string(tmp.join("composer.json")).unwrap();
    let versioned = json.replacen('{', "{\n    \"version\": \"2.0.0\",", 1);
    std::fs::write(tmp.join("composer.json"), versioned).unwrap();
    run(&tmp, &lock, &o).fingerprint.unwrap().save();
    assert!(run(&tmp, &lock, &o).fresh);

    let no_autoloader = Options {
        no_autoloader: true,
        ..o
    };
    run(&tmp, &lock, &no_autoloader).fingerprint.unwrap().save();
    assert!(run(&tmp, &lock, &no_autoloader).fresh);
    std::fs::remove_dir_all(&tmp).unwrap();
}

/// `composer dump-autoload` without a lock and without `vendor/autoload.php` names the autoloader with
/// `bin2hex(random_bytes(16))`, 32 hex digits; a root classmap path that does not exist is Composer's error.
#[test]
fn dump_autoload_without_a_lock() {
    let tmp = std::env::temp_dir().join(format!("pun-dump-nolock-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    copy(&Path::new(FIXTURES).join("project"), &tmp);
    let lock =
        Lock::from_composer_lock(&std::fs::read_to_string(tmp.join("composer.lock")).unwrap())
            .unwrap();
    run(&tmp, &lock, &Options::default());
    std::fs::remove_file(tmp.join("vendor/autoload.php")).unwrap();
    let mut manifest: Map<String, Value> =
        serde_json::from_str(&std::fs::read_to_string(tmp.join("composer.json")).unwrap()).unwrap();
    dump_autoload(
        &tmp,
        &manifest,
        None,
        &Options::default(),
        None,
        &mut |_, _| Ok(()),
    )
    .unwrap();
    let autoload = std::fs::read_to_string(tmp.join("vendor/autoload.php")).unwrap();
    let suffix = autoload
        .split("ComposerAutoloaderInit")
        .nth(1)
        .unwrap()
        .split("::")
        .next()
        .unwrap();
    assert_eq!(suffix.len(), 32, "{suffix}");
    assert!(suffix.bytes().all(|b| b.is_ascii_hexdigit()) && suffix != lock.content_hash);

    manifest.insert(
        "autoload".into(),
        serde_json::json!({"classmap": ["missing/"]}),
    );
    let err = dump_autoload(
        &tmp,
        &manifest,
        None,
        &Options::default(),
        None,
        &mut |_, _| Ok(()),
    )
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "Could not scan for classes inside \"missing/\" which does not appear to be a file nor a folder"
    );
    std::fs::remove_dir_all(&tmp).unwrap();
}

/// A file and a class named with U+10FF27 and U+10FF5C, the code points that stand for the bytes `'` and `\` of a
/// name that isn't UTF-8, are written as their own UTF-8 bytes: the generated PHP still parses and names them.
#[test]
fn private_use_names_do_not_change_the_generated_php() {
    let tmp = std::env::temp_dir().join(format!("pun-dump-private-use-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    copy(&Path::new(FIXTURES).join("project"), &tmp);
    let lock =
        Lock::from_composer_lock(&std::fs::read_to_string(tmp.join("composer.lock")).unwrap())
            .unwrap();
    run(&tmp, &lock, &Options::default());
    let (file, class) = ("x\u{10FF27}.php", "Odd\u{10FF5C}\u{10FF27}");
    std::fs::create_dir_all(tmp.join("odd")).unwrap();
    std::fs::write(
        tmp.join("odd").join(file),
        format!("<?php\nclass {class} {{}}\n"),
    )
    .unwrap();
    let mut manifest: Map<String, Value> =
        serde_json::from_str(&std::fs::read_to_string(tmp.join("composer.json")).unwrap()).unwrap();
    manifest.insert("autoload".into(), serde_json::json!({"classmap": ["odd/"]}));
    dump_autoload(
        &tmp,
        &manifest,
        None,
        &Options::default(),
        None,
        &mut |_, _| Ok(()),
    )
    .unwrap();
    for name in ["autoload_classmap.php", "autoload_static.php"] {
        let path = tmp.join("vendor/composer").join(name);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(&format!("'{class}' => ")), "{text}");
        assert!(text.contains(&format!("/odd/{file}'")), "{text}");
        if std::process::Command::new("php").arg("-v").output().is_ok() {
            let lint = std::process::Command::new("php")
                .arg("-l")
                .arg(&path)
                .output()
                .unwrap();
            assert!(
                lint.status.success(),
                "{}",
                String::from_utf8_lossy(&lint.stdout)
            );
        }
    }
    std::fs::remove_dir_all(&tmp).unwrap();
}

/// The autoloader's class names end in a suffix: a lock hash or an old autoload.php that isn't one is passed over (the
/// next source picks it), and a configured one is an error, so a hand-edited lock can't write PHP that doesn't parse.
#[test]
fn a_suffix_that_isnt_an_identifier_is_not_written() {
    let tmp = std::env::temp_dir().join(format!("pun-dump-suffix-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    copy(&Path::new(FIXTURES).join("project"), &tmp);
    let mut lock =
        Lock::from_composer_lock(&std::fs::read_to_string(tmp.join("composer.lock")).unwrap())
            .unwrap();
    lock.content_hash = "x{}; system('id'); //".into();
    run(&tmp, &lock, &Options::default());
    let suffix = |tmp: &Path| {
        let autoload = std::fs::read_to_string(tmp.join("vendor/autoload.php")).unwrap();
        let s = autoload.split("ComposerAutoloaderInit").nth(1).unwrap();
        s.split("::").next().unwrap().to_string()
    };
    let random = suffix(&tmp);
    assert!(
        random.len() == 32 && random.bytes().all(|b| b.is_ascii_hexdigit()),
        "{random}"
    );

    std::fs::write(
        tmp.join("vendor/autoload.php"),
        "<?php\nreturn ComposerAutoloaderInitbad-one::getLoader();\n",
    )
    .unwrap();
    let mut manifest: Map<String, Value> =
        serde_json::from_str(&std::fs::read_to_string(tmp.join("composer.json")).unwrap()).unwrap();
    let dump = |manifest: &Map<String, Value>| {
        dump_autoload(
            &tmp,
            manifest,
            Some(&lock),
            &Options::default(),
            None,
            &mut |_, _| Ok(()),
        )
    };
    dump(&manifest).unwrap();
    let again = suffix(&tmp);
    assert!(again.len() == 32 && again != "bad-one", "{again}");

    manifest.insert(
        "config".into(),
        serde_json::json!({"autoloader-suffix": ""}),
    );
    dump(&manifest).unwrap();
    assert_eq!(suffix(&tmp), again, "an empty suffix is none");
    manifest.insert(
        "config".into(),
        serde_json::json!({"autoloader-suffix": "Mine_2"}),
    );
    dump(&manifest).unwrap();
    assert_eq!(suffix(&tmp), "Mine_2");
    manifest.insert(
        "config".into(),
        serde_json::json!({"autoloader-suffix": "a::b"}),
    );
    assert_eq!(
        dump(&manifest).unwrap_err().to_string(),
        "config.autoloader-suffix \"a::b\" can't end a PHP class name: use letters, digits and _"
    );
    assert_eq!(suffix(&tmp), "Mine_2", "the autoloader is left as it was");
    std::fs::remove_dir_all(&tmp).unwrap();
}
