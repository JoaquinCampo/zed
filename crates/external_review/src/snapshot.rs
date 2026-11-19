//! Snapshots of a folder's files, kept in a git object store that belongs to the
//! review, not to the folder.
//!
//! The folder does not have to be a git repository, and when it is one (or holds
//! some), their history, index and staged changes are never touched. Each git
//! repository inside the folder is snapshotted with its own ignore rules and then
//! placed at its path in the folder's snapshot, which is a single git tree.

use anyhow::{Context as _, Result};
use sha2::{Digest as _, Sha256};
use std::path::{Path, PathBuf};
use text::LineEnding;

/// Names that are never part of a snapshot, besides what `.gitignore` files exclude.
const ALWAYS_EXCLUDED: &[&str] = &[
    ".git",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".mypy_cache",
    ".pytest_cache",
    ".ruff_cache",
    ".next",
    ".turbo",
    ".DS_Store",
];

/// Files larger than this are left out of snapshots: their changes cannot be
/// reviewed as text, and hashing them (archives, logs, datasets) would make every
/// snapshot slow.
const MAX_FILE_SIZE: u64 = 2 * 1024 * 1024;

pub(crate) struct SnapshotStore {
    /// The folder under review.
    root: PathBuf,
    /// The store's git directory, which also holds the review's status file.
    dir: PathBuf,
}

impl SnapshotStore {
    /// The directory that holds the snapshots of `root`. Other programs (the bb
    /// plugin) find a review's status file there: it is named after the SHA-256 of
    /// the folder's canonical path.
    pub(crate) fn dir_for(root: &Path) -> PathBuf {
        let canonical = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
        let digest = Sha256::digest(canonical.to_string_lossy().as_bytes());
        let name = digest[..16]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        paths::data_dir().join("external_review").join(name)
    }

    pub(crate) async fn open(root: PathBuf) -> Result<Self> {
        let dir = Self::dir_for(&root);
        let store = Self { root, dir };
        if !store.dir.join("HEAD").exists() {
            std::fs::create_dir_all(&store.dir)?;
            let output = util::command::new_command("git")
                .args(["init", "--quiet", "--bare"])
                .arg(&store.dir)
                .output()
                .await
                .context("running git init")?;
            anyhow::ensure!(
                output.status.success(),
                "git init failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
            store
                .git(&store.root, None, &["config", "core.bare", "false"])
                .await?;
            std::fs::create_dir_all(store.dir.join("info"))?;
            std::fs::write(
                store.dir.join("info").join("exclude"),
                ALWAYS_EXCLUDED
                    .iter()
                    .map(|name| format!("{name}\n"))
                    .collect::<String>(),
            )?;
        }
        std::fs::create_dir_all(store.dir.join("indexes"))?;
        Ok(store)
    }

    pub(crate) fn status_file(&self) -> PathBuf {
        self.dir.join("status.json")
    }

    async fn git(&self, work_tree: &Path, index: Option<&Path>, args: &[&str]) -> Result<Vec<u8>> {
        let mut command = util::command::new_command("git");
        command
            .current_dir(work_tree)
            .args(args)
            .env("GIT_DIR", &self.dir)
            .env("GIT_WORK_TREE", work_tree);
        if let Some(index) = index {
            command.env("GIT_INDEX_FILE", index);
        }
        let output = command
            .output()
            .await
            .with_context(|| format!("running git {}", args.join(" ")))?;
        anyhow::ensure!(
            output.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(output.stdout)
    }

    async fn git_line(&self, args: &[&str]) -> Result<String> {
        let stdout = self.git(&self.root, None, args).await?;
        Ok(String::from_utf8(stdout)?.trim().to_string())
    }

    /// Writes the folder's files, as they are on disk now, as a git tree.
    pub(crate) async fn snapshot(&self) -> Result<String> {
        let root = self.root.clone();
        let FolderScan {
            repositories,
            large_files,
        } = smol::unblock(move || scan_folder(&root)).await;

        // Each part (the folder, and every repository in it) is added with its own
        // ignore rules, leaving out the repositories inside it. Its index is kept
        // between snapshots, so unchanged files are not hashed again.
        let mut parts = Vec::new();
        for part in std::iter::once(PathBuf::new()).chain(repositories.iter().cloned()) {
            let work_tree = self.root.join(&part);
            let index = self.dir.join("indexes").join(index_name(&part));
            let excludes_file = index.with_extension("exclude");
            let excludes = large_files
                .iter()
                .filter(|file| owning_part(file, &repositories) == part.as_path())
                .filter_map(|file| file.strip_prefix(&part).ok())
                .map(|file| format!("{}\n", ignore_pattern(file)))
                .collect::<String>();
            std::fs::write(&excludes_file, excludes)?;
            let mut args = vec![
                "-c".to_string(),
                format!("core.excludesFile={}", excludes_file.display()),
                "add".into(),
                "--all".into(),
                "--".into(),
                ".".into(),
            ];
            for inner in &repositories {
                if let Ok(inside) = inner.strip_prefix(&part)
                    && !inside.as_os_str().is_empty()
                {
                    args.push(format!(":(exclude,literal){}", to_git_path(inside)));
                }
            }
            let args = args.iter().map(String::as_str).collect::<Vec<_>>();
            self.git(&work_tree, Some(&index), &args).await?;
            let tree = String::from_utf8(self.git(&work_tree, Some(&index), &["write-tree"]).await?)?
                .trim()
                .to_string();
            parts.push((part, tree));
        }

        // Places every repository's tree at its path in the folder's tree.
        let index = self.dir.join("indexes").join("combined.index");
        std::fs::remove_file(&index).ok();
        let mut parts = parts.into_iter();
        let (_, root_tree) = parts.next().context("no snapshot of the folder")?;
        self.git(&self.root, Some(&index), &["read-tree", &root_tree])
            .await?;
        for (part, tree) in parts {
            let prefix = format!("--prefix={}/", to_git_path(&part));
            self.git(&self.root, Some(&index), &["read-tree", &prefix, &tree])
                .await?;
        }
        let tree = String::from_utf8(self.git(&self.root, Some(&index), &["write-tree"]).await?)?;
        std::fs::remove_file(&index).ok();
        Ok(tree.trim().to_string())
    }

    /// The tree of a snapshot taken earlier, if the store still has it.
    pub(crate) async fn resolve(&self, base: &str) -> Result<String> {
        self.git_line(&["rev-parse", "--verify", "--quiet", &format!("{base}^{{tree}}")])
            .await
    }

    /// Paths, relative to the folder, of files added or modified since `base`.
    pub(crate) async fn changed_paths(&self, base: &str) -> Result<Vec<String>> {
        let current = self.snapshot().await?;
        if current == base {
            return Ok(Vec::new());
        }
        let output = self
            .git(
                &self.root,
                None,
                &["diff", "--name-status", "-z", "--no-renames", base, &current],
            )
            .await?;
        let output = String::from_utf8(output)?;
        let mut fields = output.split('\0').filter(|field| !field.is_empty());
        let mut paths = Vec::new();
        while let (Some(status), Some(path)) = (fields.next(), fields.next()) {
            if status != "D" {
                paths.push(path.to_string());
            }
        }
        Ok(paths)
    }

    /// The text of `path` in `base`, or `None` if the file did not exist there.
    pub(crate) async fn base_text(&self, base: &str, path: &str) -> Result<Option<String>> {
        let object = format!("{base}:{path}");
        if self
            .git(&self.root, None, &["cat-file", "-e", &object])
            .await
            .is_err()
        {
            return Ok(None);
        }
        let blob = self
            .git(&self.root, None, &["cat-file", "blob", &object])
            .await?;
        let mut text = String::from_utf8(blob).context("file in the review base is not UTF-8")?;
        LineEnding::normalize(&mut text);
        Ok(Some(text))
    }
}

struct FolderScan {
    /// Folders inside the folder, relative to it, that are git repositories (or
    /// linked worktrees), outermost first.
    repositories: Vec<PathBuf>,
    /// Files, relative to the folder, too large to snapshot.
    large_files: Vec<PathBuf>,
}

/// Walks the folder, leaving out what `.gitignore` files and `ALWAYS_EXCLUDED`
/// exclude.
fn scan_folder(root: &Path) -> FolderScan {
    let mut repositories = Vec::new();
    let mut large_files = Vec::new();
    let walk = ignore::WalkBuilder::new(root)
        .hidden(false)
        .git_global(false)
        .require_git(false)
        .filter_entry(|entry| {
            let name = entry.file_name().to_string_lossy();
            !ALWAYS_EXCLUDED.contains(&name.as_ref())
        })
        .build();
    for entry in walk.filter_map(|entry| entry.ok()) {
        let Ok(relative) = entry.path().strip_prefix(root) else {
            continue;
        };
        let Some(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if entry.depth() > 0 && entry.path().join(".git").exists() {
                repositories.push(relative.to_path_buf());
            }
        } else if kind.is_file()
            && entry
                .metadata()
                .is_ok_and(|metadata| metadata.len() > MAX_FILE_SIZE)
        {
            large_files.push(relative.to_path_buf());
        }
    }
    repositories.sort_by_key(|path| path.components().count());
    FolderScan {
        repositories,
        large_files,
    }
}

/// The innermost repository holding `file`, or the folder itself (an empty path).
fn owning_part<'a>(file: &Path, repositories: &'a [PathBuf]) -> &'a Path {
    repositories
        .iter()
        .rev()
        .find(|repository| file.starts_with(repository))
        .map_or(Path::new(""), PathBuf::as_path)
}

/// A `.gitignore` line matching exactly `path`, relative to the work tree.
fn ignore_pattern(path: &Path) -> String {
    let mut pattern = String::from("/");
    for character in to_git_path(path).chars() {
        if matches!(character, '\\' | '*' | '?' | '[' | '!' | '#' | ' ') {
            pattern.push('\\');
        }
        pattern.push(character);
    }
    pattern
}

fn to_git_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// A file name for the index of one part of the folder.
fn index_name(part: &Path) -> String {
    if part.as_os_str().is_empty() {
        return "root.index".to_string();
    }
    let digest = Sha256::digest(to_git_path(part).as_bytes());
    let name = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{name}.index")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(dir: &Path, args: &[&str]) {
        let output = smol::block_on(
            util::command::new_command("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output(),
        )
        .unwrap();
        assert!(output.status.success(), "git {args:?} failed");
    }

    /// A folder that is not a repository, holding plain folders, a repository with
    /// commits and its own ignore rules, and a repository without any commit.
    #[test]
    fn test_snapshot_folder_with_repositories() {
        static DATA_DIR: std::sync::Once = std::sync::Once::new();
        let data = tempfile::tempdir().unwrap().keep();
        DATA_DIR.call_once(|| {
            paths::set_custom_data_dir(data.to_str().unwrap());
        });

        let folder = tempfile::tempdir().unwrap().keep();
        let write = |path: &str, text: &str| {
            let path = folder.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("notes/plan.md", "plan\n");
        write("notes/node_modules/dep.js", "dep\n");
        write("app/main.py", "print(1)\n");
        write("app/.gitignore", "out/\n");
        write("app/out/build.log", "log\n");
        let app = folder.join("app");
        run(&app, &["init", "--quiet"]);
        run(&app, &["add", "."]);
        run(
            &app,
            &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--quiet", "-m", "init"],
        );
        write("app/staged.py", "staged\n");
        run(&app, &["add", "staged.py"]);
        write("fresh/readme.md", "fresh\n");
        run(&folder.join("fresh"), &["init", "--quiet"]);
        let app_index_before = std::fs::read(app.join(".git/index")).unwrap();

        smol::block_on(async {
            let store = SnapshotStore::open(folder.clone()).await.unwrap();
            let base = store.snapshot().await.unwrap();
            for (path, expected) in [
                ("notes/plan.md", Some("plan\n")),
                ("app/main.py", Some("print(1)\n")),
                ("app/staged.py", Some("staged\n")),
                ("fresh/readme.md", Some("fresh\n")),
                ("notes/node_modules/dep.js", None),
                ("app/out/build.log", None),
            ] {
                assert_eq!(
                    store.base_text(&base, path).await.unwrap().as_deref(),
                    expected,
                    "{path}"
                );
            }

            write("notes/plan.md", "plan v2\n");
            write("app/main.py", "print(2)\n");
            write("fresh/new.py", "new\n");
            write("app/out/build.log", "log v2\n");
            write("loose.txt", "loose\n");
            write("app/huge.bin", &"x".repeat(MAX_FILE_SIZE as usize + 1));
            let mut changed = store.changed_paths(&base).await.unwrap();
            changed.sort();
            assert_eq!(
                changed,
                ["app/main.py", "fresh/new.py", "loose.txt", "notes/plan.md"]
            );

            // A store opened again later still has the snapshot.
            let reopened = SnapshotStore::open(folder.clone()).await.unwrap();
            assert_eq!(reopened.resolve(&base).await.unwrap(), base);
        });

        assert_eq!(
            std::fs::read(app.join(".git/index")).unwrap(),
            app_index_before,
            "the repository's own index changed"
        );
    }
}


