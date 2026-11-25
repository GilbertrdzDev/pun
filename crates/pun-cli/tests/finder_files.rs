#![cfg(unix)]
mod common;

use punphp::finder::{Command, Exclusions, FinderError, files, is_path_excluded};
use std::path::{Path, PathBuf};

fn tree(files: &[&str]) -> PathBuf {
    common::tree("files", files, "<?php\n")
}

fn list(
    root: &Path,
    toml: &str,
    command: Option<Command>,
    paths: &[&str],
) -> Result<Vec<String>, String> {
    let cfg = pun_config::from_toml(root, toml).map_err(|e| e.to_string())?;
    let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    let out = files(&cfg, command, &paths).map_err(|e| e.to_string())?;
    Ok(out
        .iter()
        .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
        .collect())
}

const TREE: &[&str] = &[
    "app/A.php",
    "app/Models/User.php",
    "app/storage/Kept.php",
    "app/view.blade.php",
    "app/_ide_helper.php",
    "app/vendor/V.php",
    "app/.hidden/H.php",
    "app/notes.txt",
    "tests/Feature/ExampleTest.php",
    "tests/Pest.php",
    "storage/framework/S.php",
    "src/Other.php",
    "vendor/x/V.php",
    "Root.php",
];

#[test]
fn plan_example_gives_the_listed_sets() {
    let root = tree(TREE);
    let toml = r#"
[files]
paths = ["app", "tests"]
exclude = ["storage/**"]

[analyse]
exclude = ["tests/**"]
"#;
    let app = [
        "app/A.php",
        "app/Models/User.php",
        "app/_ide_helper.php",
        "app/storage/Kept.php",
    ];
    let tests = ["tests/Feature/ExampleTest.php", "tests/Pest.php"];
    assert_eq!(
        list(&root, toml, None, &[]).unwrap(),
        [
            &app[..2],
            &[
                "app/_ide_helper.php",
                "app/storage/Kept.php",
                "app/view.blade.php"
            ],
            &tests
        ]
        .concat()
    );
    assert_eq!(list(&root, toml, Some(Command::Analyse), &[]).unwrap(), app);
    assert_eq!(
        list(&root, toml, Some(Command::Lint), &[]).unwrap(),
        [&app[..], &tests].concat()
    );
    assert_eq!(
        list(&root, toml, Some(Command::Format), &[]).unwrap(),
        [
            "app/A.php",
            "app/Models/User.php",
            "tests/Feature/ExampleTest.php",
            "tests/Pest.php"
        ]
    );
}

#[test]
fn each_command_adds_its_own_exclude() {
    let root = tree(TREE);
    let toml = r#"
[files]
exclude = ["tests"]

[format]
exclude = ["app/Models/**"]

[lint]
exclude = ["app/*.php", "Root.php"]

[analyse]
paths = ["app"]
exclude = ["app/Models"]
"#;
    let all = [
        "Root.php",
        "app/A.php",
        "app/Models/User.php",
        "app/_ide_helper.php",
        "app/storage/Kept.php",
        "app/view.blade.php",
        "src/Other.php",
        "storage/framework/S.php",
    ];
    assert_eq!(list(&root, toml, None, &[]).unwrap(), all);
    assert_eq!(
        list(&root, toml, Some(Command::Lint), &[]).unwrap(),
        ["src/Other.php"]
    );
    assert_eq!(
        list(&root, toml, Some(Command::Analyse), &[]).unwrap(),
        ["app/A.php", "app/_ide_helper.php", "app/storage/Kept.php"]
    );
    assert_eq!(
        list(&root, toml, Some(Command::Format), &[]).unwrap(),
        ["Root.php", "app/A.php", "src/Other.php"]
    );
}

#[test]
fn path_arguments_replace_paths_but_keep_excludes() {
    let root = tree(TREE);
    let toml = "[files]\npaths = [\"app\"]\nexclude = [\"src/**\", \"app/Models\"]\n";
    assert_eq!(
        list(
            &root,
            toml,
            None,
            &[
                "src",
                "vendor/x/V.php",
                "app/Models/User.php",
                "tests/Pest.php"
            ]
        )
        .unwrap(),
        ["tests/Pest.php", "vendor/x/V.php"]
    );
    assert_eq!(
        list(
            &root,
            toml,
            Some(Command::Format),
            &["src", "app/Models/User.php"]
        )
        .unwrap(),
        ["app/Models/User.php", "src/Other.php"]
    );
    assert_eq!(
        list(&root, toml, None, &["nope"]).unwrap_err(),
        "The path \"nope\" is not readable."
    );
    assert_eq!(
        list(&root, "[files]\npaths = [\"missing\"]\n", None, &[]).unwrap_err(),
        "The \"missing\" directory does not exist."
    );
    assert_eq!(
        list(
            &root,
            "[files]\npaths = [\"Root.php\", \"tests\"]\n",
            Some(Command::Lint),
            &[]
        )
        .unwrap(),
        [
            "Root.php",
            "tests/Feature/ExampleTest.php",
            "tests/Pest.php"
        ]
    );
    assert!(
        list(&root, "[files]\nexclude = [\"a/{b\"]\n", None, &[])
            .unwrap_err()
            .starts_with("invalid exclude: ")
    );
}

#[test]
fn stdin_filename_honours_glob_excludes() {
    let root = tree(&[]);
    let cfg = pun_config::from_toml(&root, "[format]\nexclude = [\"app/Gen*/**\"]\n").unwrap();
    assert!(is_path_excluded(&cfg, &root.join("app/Generated/X.php")));
    assert!(!is_path_excluded(&cfg, &root.join("app/Models/X.php")));
}

#[test]
fn errors_are_typed() {
    let root = tree(&["src/A.php"]);
    let kind = |toml: &str, command: Option<Command>, paths: &[&str]| {
        let cfg = pun_config::from_toml(&root, toml).unwrap();
        let paths: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        let e = files(&cfg, command, &paths).unwrap_err();
        e.downcast_ref::<FinderError>().cloned()
    };
    let missing = "[files]\npaths = [\"missing\"]\n";
    for command in [None, Some(Command::Format)] {
        assert_eq!(
            kind("", command, &["nope"]),
            Some(FinderError::NotReadable("nope".into()))
        );
        assert_eq!(
            kind(missing, command, &[]),
            Some(FinderError::DirectoryNotFound("missing".into()))
        );
    }
}

#[test]
fn exclusions_test_absolute_paths() {
    let root = tree(&[]);
    let cfg = pun_config::from_toml(
        &root,
        "[files]\nexclude = [\"src/Gen*\"]\n[analyse]\nexclude = [\"tests/**\"]\n",
    )
    .unwrap();
    let e = Exclusions::new(&cfg, Command::Analyse).unwrap();
    for excluded in [
        "tests/Unit/A.php",
        "src/Generated.php",
        "storage/framework/views/x.php",
        "node_modules/a/b.php",
    ] {
        assert!(e.is_excluded(&root.join(excluded)), "{excluded}");
    }
    for kept in [
        "app/storage/A.php",
        "vendor/laravel/framework/src/A.php",
        "src/Models/A.php",
    ] {
        assert!(!e.is_excluded(&root.join(kept)), "{kept}");
    }
    let e = Exclusions::new(&cfg, Command::Lint).unwrap();
    assert!(!e.is_excluded(&root.join("tests/Unit/A.php")));
    assert!(e.is_excluded(&root.join("src/Generated.php")));
}

#[test]
fn an_unreadable_directory_fails_the_walk() {
    use std::os::unix::fs::PermissionsExt;
    let root = tree(&["a/A.php", "b/locked/L.php", "b/B.php", "c/C.php"]);
    let locked = root.join("b/locked");
    let mode = |m| std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(m)).unwrap();
    mode(0o000);
    // Root reads the directory anyway, so the walk has nothing to report.
    let enforced = std::fs::read_dir(&locked).is_err();
    let got = list(&root, "", Some(Command::Format), &[]);
    mode(0o755);
    std::fs::remove_dir_all(&root).unwrap();
    if enforced {
        assert_eq!(
            got,
            Err(format!(
                "IO error for operation on {}: Permission denied (os error 13)",
                locked.display()
            ))
        );
    }
}
