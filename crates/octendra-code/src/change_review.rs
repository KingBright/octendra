//! Complete bounded review: tracked diffs plus explicitly labelled untracked files.
use crate::{
    files::{self, Workspace},
    hash,
};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::io::Read;
#[cfg(unix)]
use std::time::Duration;
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};
const CAP: usize = 16 * 1024 * 1024;
#[cfg(unix)]
const SNAPSHOT_CAP: usize = 64 * 1024 * 1024;
/// Git's repository discovery is not a workspace boundary. Only an ordinary
/// in-root .git directory is accepted; linked/external worktrees fail closed.
#[cfg(unix)]
fn repository(
    ws: &Workspace,
) -> Result<(
    std::sync::Arc<cap_std::fs::Dir>,
    std::sync::Arc<cap_std::fs::Dir>,
)> {
    let root = std::sync::Arc::new(files::directory(ws)?);
    let meta = root
        .symlink_metadata(".git")
        .context("workspace_review_denied: workspace must be the repository root")?;
    ensure!(
        meta.is_dir() && !meta.is_symlink(),
        "workspace_review_denied: external gitdir and linked worktrees are unsupported"
    );
    let git = std::sync::Arc::new(files::open_beneath(&root, std::path::Path::new(".git"))?);
    // Git may follow object alternates, common directories and config includes.
    // These are additional authorities and cannot be inferred from a cwd.
    for path in [
        "commondir",
        "objects/info/alternates",
        "objects/info/http-alternates",
    ] {
        ensure!(
            git.symlink_metadata(path)
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound),
            "workspace_review_denied: external Git metadata is unsupported"
        );
    }
    let config = files::read_text(&git, "config")?;
    let normalized = config.to_ascii_lowercase();
    ensure!(
        ![
            "include",
            "worktree",
            "alternat",
            "fsmonitor",
            "attributesfile"
        ]
        .iter()
        .any(|key| normalized.contains(key)),
        "workspace_review_denied: Git config may reference external metadata"
    );
    let mut stack = vec![git.try_clone()?];
    let start = std::time::Instant::now();
    let mut count = 0usize;
    while let Some(dir) = stack.pop() {
        for entry in dir.entries()? {
            let entry = entry?;
            let kind = entry.file_type()?;
            ensure!(
                !kind.is_symlink(),
                "workspace_review_denied: Git metadata symlink"
            );
            if kind.is_dir() {
                stack.push(files::open_beneath(
                    &dir,
                    std::path::Path::new(&entry.file_name()),
                )?);
            } else {
                ensure!(
                    kind.is_file(),
                    "workspace_review_denied: special Git metadata"
                );
            }
            count += 1;
            ensure!(
                count <= 100000 && start.elapsed().as_secs() < 5,
                "workspace_review_denied: Git metadata exceeds bounded review; narrow repository"
            );
        }
    }
    Ok((root, git))
}
#[cfg(unix)]
async fn git_output(
    ws: &Workspace,
    args: &[&str],
    paths: Option<&[String]>,
    input: Option<Vec<u8>>,
    limit: usize,
) -> Result<Vec<u8>> {
    let (root, git) = repository(ws)?;
    let mut cmd = tokio::process::Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key.to_string_lossy().starts_with("GIT_") {
            cmd.env_remove(key);
        }
    }
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_ATTR_NOSYSTEM", "1");
    #[cfg(unix)]
    {
        use std::os::{fd::AsRawFd, unix::process::CommandExt};
        cmd.args([
            "--no-pager",
            "--no-replace-objects",
            "--git-dir=.git",
            "--work-tree=.",
        ]);
        // fchdir pins cwd even if its path/ancestors are exchanged after
        // validation. Verify .git still names the checked directory before exec.
        unsafe {
            cmd.as_std_mut().pre_exec(move || {
                if nix::libc::fchdir(root.as_raw_fd()) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                let mut held: nix::libc::stat = std::mem::zeroed();
                let mut named: nix::libc::stat = std::mem::zeroed();
                if nix::libc::fstat(git.as_raw_fd(), &mut held) != 0
                    || nix::libc::fstatat(
                        root.as_raw_fd(),
                        c".git".as_ptr(),
                        &mut named,
                        nix::libc::AT_SYMLINK_NOFOLLOW,
                    ) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                if held.st_dev != named.st_dev || held.st_ino != named.st_ino {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "workspace_review_denied: Git directory changed",
                    ));
                }
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (root, git);
        return Err(anyhow::anyhow!(
            "workspace_review_denied: handle-pinned Git review is unavailable on this platform"
        ));
    }
    cmd.args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.attributesFile=/dev/null",
    ])
    .args(args);
    if let Some(paths) = paths {
        cmd.arg("--").args(paths);
    }
    cmd.stdin(if input.is_some() {
        std::process::Stdio::piped()
    } else {
        std::process::Stdio::null()
    })
    .stdout(std::process::Stdio::piped())
    .stderr(std::process::Stdio::null())
    .kill_on_drop(true);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW, preserve piped Git output.
    let mut child = cmd.spawn()?;
    let stdout = child.stdout.take().context("missing git output")?;
    let stdin = child.stdin.take();
    let mut bytes = Vec::new();
    let send = async move {
        if let (Some(mut stdin), Some(input)) = (stdin, input) {
            stdin.write_all(&input).await?;
            stdin.shutdown().await?;
        }
        Ok::<_, std::io::Error>(())
    };
    let receive = async {
        stdout
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)
            .await?;
        if bytes.len() > limit {
            return Err(std::io::Error::other("review capacity: narrow paths"));
        }
        Ok::<_, std::io::Error>(())
    };
    tokio::time::timeout(Duration::from_secs(20), async {
        tokio::try_join!(send, receive)
    })
    .await
    .context("git review timed out")??;
    if bytes.len() > limit {
        child.kill().await?;
        anyhow::bail!("review capacity: narrow paths");
    }
    let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .context("git review exit timed out")??;
    ensure!(status.success(), "git review requires a valid worktree");
    Ok(bytes)
}
#[cfg(unix)]
async fn git(ws: &Workspace, args: &[&str], paths: &[String]) -> Result<Vec<u8>> {
    git_output(ws, args, Some(paths), None, CAP).await
}
#[cfg(not(unix))]
async fn git(_ws: &Workspace, _args: &[&str], _paths: &[String]) -> Result<Vec<u8>> {
    anyhow::bail!(
        "workspace_review_denied: handle-pinned Git review is unavailable on this platform"
    )
}
#[cfg(unix)]
fn worktree_bytes(dir: &cap_std::fs::Dir, path: &str) -> Result<(Option<Vec<u8>>, &'static str)> {
    use cap_std::fs::{OpenOptions, OpenOptionsExt, PermissionsExt};
    use std::os::unix::ffi::OsStrExt;
    let meta = match dir.symlink_metadata(files::relative(path)?) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok((None, "000000")),
        Err(error) => return Err(error.into()),
    };
    if meta.is_symlink() {
        return Ok((
            Some(
                dir.read_link_contents(path)?
                    .as_os_str()
                    .as_bytes()
                    .to_vec(),
            ),
            "120000",
        ));
    }
    if meta.is_dir() {
        return Ok((None, "000000"));
    }
    ensure!(
        meta.is_file(),
        "workspace_review_denied: tracked special file"
    );
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    let file = dir.open_with(path, &options)?;
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.len() <= CAP as u64,
        "review capacity: narrow paths"
    );
    let mode = if meta.permissions().mode() & 0o111 != 0 {
        "100755"
    } else {
        "100644"
    };
    let mut bytes = Vec::new();
    file.take((CAP + 1) as u64).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= CAP, "review capacity: narrow paths");
    Ok((Some(bytes), mode))
}

/// Never ask Git to convert worktree contents. Both clean and process filters
/// can execute even with --no-ext-diff/--no-textconv. Plumbing returns raw index
/// objects; held directory capabilities supply the current bytes.
#[cfg(unix)]
async fn raw_worktree_diff(ws: &Workspace, paths: &[String]) -> Result<Vec<u8>> {
    let index = git(ws, &["ls-files", "--stage", "-z", "--full-name"], paths).await?;
    let mut entries = Vec::new();
    let mut input = Vec::new();
    for record in index.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let (header, path) = record.split_at(
            record
                .iter()
                .position(|b| *b == b'\t')
                .context("workspace_review_denied: invalid index record")?,
        );
        let fields: Vec<_> = std::str::from_utf8(header)?.split(' ').collect();
        ensure!(
            fields.len() == 3 && fields[2] == "0",
            "workspace_review_denied: unmerged index; use bounded file reads"
        );
        let mode = fields[0];
        if mode == "160000" {
            continue;
        } // Preserve ignore-submodules=all.
        ensure!(
            matches!(mode, "100644" | "100755" | "120000"),
            "workspace_review_denied: unsupported index mode"
        );
        let oid = fields[1];
        ensure!(
            [40, 64].contains(&oid.len()) && oid.bytes().all(|b| b.is_ascii_hexdigit()),
            "invalid index object"
        );
        let path = std::str::from_utf8(&path[1..])?;
        files::relative(path)?;
        entries.push((mode.to_owned(), path.to_owned(), oid.to_owned()));
        input.extend_from_slice(oid.as_bytes());
        input.push(b'\n');
        ensure!(entries.len() <= 100000, "review capacity: narrow paths");
    }
    if entries.is_empty() {
        return Ok(Vec::new());
    }
    let objects = git_output(
        ws,
        &["cat-file", "--batch"],
        None,
        Some(input),
        SNAPSHOT_CAP,
    )
    .await?;
    let dir = files::directory(ws)?;
    tokio::task::spawn_blocking(move || -> Result<Vec<u8>> {
        let mut offset = 0usize;
        let mut text = String::new();
        let mut read_bytes = objects.len();
        let started = std::time::Instant::now();
        for (mode, path, oid) in entries {
            ensure!(
                started.elapsed().as_secs() < 120,
                "review capacity: narrow paths"
            );
            let newline = objects[offset..]
                .iter()
                .position(|b| *b == b'\n')
                .context("invalid blob header")?
                + offset;
            let header: Vec<_> = std::str::from_utf8(&objects[offset..newline])?
                .split(' ')
                .collect();
            ensure!(
                header.len() == 3 && header[0] == oid && header[1] == "blob",
                "workspace_review_denied: index blob unavailable"
            );
            let length: usize = header[2].parse()?;
            let begin = newline + 1;
            let end = begin.checked_add(length).context("blob length overflow")?;
            ensure!(
                end < objects.len() && objects[end] == b'\n',
                "invalid blob length"
            );
            let before = &objects[begin..end];
            offset = end + 1;
            let (after, new_mode) = worktree_bytes(&dir, &path)?;
            read_bytes = read_bytes.saturating_add(after.as_ref().map_or(0, Vec::len));
            ensure!(read_bytes <= SNAPSHOT_CAP, "review capacity: narrow paths");
            if after.as_deref() == Some(before) && mode == new_mode {
                continue;
            }
            text.push_str(&format!("diff --git a/{path} b/{path}\n"));
            if after.is_none() {
                text.push_str(&format!("deleted file mode {mode}\n"));
            } else if mode != new_mode {
                text.push_str(&format!("old mode {mode}\nnew mode {new_mode}\n"));
            }
            let old = std::str::from_utf8(before)
                .ok()
                .filter(|s| !s.contains('\0'));
            let new = after
                .as_ref()
                .and_then(|b| std::str::from_utf8(b).ok())
                .filter(|s| !s.contains('\0'));
            if let (Some(old), Some(new)) = (old, if after.is_none() { Some("") } else { new }) {
                let added = if after.is_none() {
                    "/dev/null".to_owned()
                } else {
                    format!("b/{path}")
                };
                text.push_str(
                    &similar::TextDiff::configure()
                        .timeout(Duration::from_millis(500))
                        .diff_lines(old, new)
                        .unified_diff()
                        .context_radius(3)
                        .header(&format!("a/{path}"), &added)
                        .to_string(),
                );
            } else {
                text.push_str(&format!(
                    "Binary files a/{path} and b/{path} differ (sha256 {} -> {})\n",
                    hash(before),
                    after.as_ref().map(hash).unwrap_or_else(|| "absent".into())
                ));
            }
            ensure!(text.len() <= CAP, "review capacity: narrow paths");
        }
        ensure!(offset == objects.len(), "unexpected blob output");
        Ok(text.into_bytes())
    })
    .await?
}
#[cfg(not(unix))]
async fn raw_worktree_diff(_ws: &Workspace, _paths: &[String]) -> Result<Vec<u8>> {
    anyhow::bail!(
        "workspace_review_denied: handle-pinned Git review is unavailable on this platform"
    )
}
pub(crate) async fn read(ws: &Workspace, v: &Value) -> Result<Value> {
    let paths: Vec<String> = v
        .get("paths")
        .map(|p| serde_json::from_value(p.clone()))
        .transpose()?
        .unwrap_or_default();
    ensure!(paths.len() <= 20, "too many diff paths");
    for p in &paths {
        files::relative(p)?;
    }
    let staged = v["staged"] == true;
    let args = if staged {
        vec![
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--cached",
            "--ignore-submodules=all",
        ]
    } else {
        vec![
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            "--ignore-submodules=all",
        ]
    };
    let bytes = if staged {
        git(ws, &args, &paths).await?
    } else {
        raw_worktree_diff(ws, &paths).await?
    };
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    let names = if staged || v["include_untracked"] == false {
        Vec::new()
    } else {
        git(
            ws,
            &["ls-files", "--others", "--exclude-standard", "-z"],
            &paths,
        )
        .await?
    };
    let entries: Vec<&[u8]> = names.split(|b| *b == 0).filter(|s| !s.is_empty()).collect();
    let dir = files::directory(ws)?;
    let mut details = Vec::new();
    let mut skipped = 0;
    for name in &entries {
        if details.len() >= 100 || text.len() >= CAP || serde_json::to_vec(&details)?.len() > 24000
        {
            skipped += 1;
            continue;
        }
        let Ok(path) = std::str::from_utf8(name) else {
            skipped += 1;
            continue;
        };
        files::relative(path)?;
        let meta = dir.symlink_metadata(path)?;
        if meta.is_symlink() || !meta.is_file() {
            text.push_str(&format!(
                "\nUntracked special entry (not followed): {path}\n"
            ));
            details.push(json!({"path":path,"kind":"special_not_followed","expanded":false}));
            continue;
        }
        if meta.len() > 8 * 1024 * 1024 {
            text.push_str(&format!(
                "\nUntracked large file (content omitted): {path}\n"
            ));
            details.push(json!({"path":path,"kind":"large","size":meta.len(),"expanded":false}));
            continue;
        }
        let mut data = Vec::new();
        dir.open(path)?
            .take(8 * 1024 * 1024 + 1)
            .read_to_end(&mut data)?;
        ensure!(
            data.len() <= 8 * 1024 * 1024,
            "file changed during review; narrow paths"
        );
        let version = hash(&data);
        let content = std::str::from_utf8(&data)
            .ok()
            .filter(|s| !s.contains('\0'));
        let expanded = content
            .is_some_and(|s| text.len() + s.len() + s.lines().count() + path.len() * 4 + 512 < CAP);
        if expanded {
            text.push_str(&format!("\ndiff --git a/{path} b/{path}\nnew file (untracked)\n--- /dev/null\n+++ b/{path}\n"));
            for line in content.unwrap().split_inclusive('\n') {
                text.push('+');
                text.push_str(line);
            }
            if !data.ends_with(b"\n") {
                text.push_str("\n\\ No newline at end of file\n");
            }
        } else {
            text.push_str(&format!(
                "\nUntracked binary or budget-limited file: {path} ({version})\n"
            ));
        }
        details.push(json!({"path":path,"kind":if content.is_some(){"text"}else{"binary"},"size":data.len(),"version":version,"expanded":expanded}));
    }
    let cursor = files::number(v, "cursor", 0, 0, 100000000)?;
    let max = files::number(v, "max_bytes", 16000, 1024, 65536)?;
    ensure!(
        cursor <= text.len() && text.is_char_boundary(cursor),
        "invalid diff cursor"
    );
    let version = hash(text.as_bytes());
    if let Some(expected) = v["expected_version"].as_str() {
        ensure!(
            expected == version,
            "version_conflict: diff changed; restart review"
        );
    }
    let (chunk, truncated) = files::bounded(&text[cursor..], max);
    Ok(
        json!({"diff":chunk,"truncated":truncated,"next_cursor":truncated.then_some(cursor+chunk.len()),"version":version,
        "untracked":details,"untracked_count":entries.len(),"untracked_omitted":skipped,"staged":staged,
        "coverage":"staged Git diff or raw index-to-worktree bytes without conversion filters, plus nonignored untracked files unless staged/include_untracked=false; special entries never followed","review_protocol":1}),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    fn init(root: &std::path::Path) {
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
    }
    fn workspace(root: &std::path::Path) -> Workspace {
        Workspace {
            directory: None,
            id: "fixture".into(),
            device_id: "fixture".into(),
            root: root.canonicalize().unwrap(),
        }
    }
    #[tokio::test]
    async fn authorization_review_rejects_parent_repo_external_gitdir_and_worktree() {
        let fixture = tempfile::tempdir().unwrap();
        let repo = fixture.path().join("repo");
        let child = repo.join("allowed-child");
        let external = fixture.path().join("external");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::create_dir(&external).unwrap();
        init(&repo);
        std::fs::write(repo.join("outside-allowed-child"), "OUTSIDE-FIXTURE").unwrap();
        assert!(
            read(&workspace(&child), &json!({}))
                .await
                .unwrap_err()
                .to_string()
                .contains("workspace_review_denied")
        );
        std::fs::write(
            child.join(".git"),
            format!("gitdir: {}\n", repo.join(".git").display()),
        )
        .unwrap();
        assert!(
            read(
                &workspace(&child),
                &json!({"paths":["outside-allowed-child"]})
            )
            .await
            .is_err()
        );
        std::fs::remove_file(child.join(".git")).unwrap();
        // Real linked-worktree fixture; Git stores a .git indirection file.
        assert!(
            std::process::Command::new("git")
                .args([
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.invalid",
                    "commit",
                    "--allow-empty",
                    "-qm",
                    "fixture"
                ])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            std::process::Command::new("git")
                .args(["worktree", "add", "--detach", "-q"])
                .arg(&external)
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
        assert!(read(&workspace(&external), &json!({})).await.is_err());
        std::fs::write(
            repo.join(".git/objects/info/alternates"),
            external.display().to_string(),
        )
        .unwrap();
        assert!(read(&workspace(&repo), &json!({})).await.is_err());
        std::fs::remove_file(repo.join(".git/objects/info/alternates")).unwrap();
        let config = repo.join(".git/config");
        let original = std::fs::read_to_string(&config).unwrap();
        std::fs::write(
            config,
            format!("{original}\n[core]\nworktree = {}\n", external.display()),
        )
        .unwrap();
        assert!(read(&workspace(&repo), &json!({})).await.is_err());
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn authorization_review_pins_root_and_refuses_outside_folder_symlinks() {
        let fixture = tempfile::tempdir().unwrap();
        let repo = fixture.path().join("repo");
        let outside = fixture.path().join("outside");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(&outside).unwrap();
        init(&repo);
        std::fs::write(repo.join("inside"), "INSIDE").unwrap();
        std::fs::write(outside.join("outside"), "OUTSIDE-FIXTURE").unwrap();
        std::os::unix::fs::symlink(&outside, repo.join("escape")).unwrap();
        let mut ws = workspace(&repo);
        ws.directory = Some(std::sync::Arc::new(files::directory(&ws).unwrap()));
        std::fs::rename(&repo, fixture.path().join("moved")).unwrap();
        std::os::unix::fs::symlink(&outside, &repo).unwrap();
        let result = read(&ws, &json!({})).await.unwrap();
        assert!(result["diff"].as_str().unwrap().contains("INSIDE"));
        assert!(!result.to_string().contains("OUTSIDE-FIXTURE"));
    }
    #[cfg(unix)]
    async fn general_git_filter_fixture(mode: &str) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        init(root);
        std::fs::write(root.join("state.txt"), "before\n").unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap()
        };
        assert!(git(&["add", "state.txt"]).status.success());
        assert!(
            git(&[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-qm",
                "fixture"
            ])
            .status
            .success()
        );
        std::fs::write(root.join("state.txt"), "staged\n").unwrap();
        let oid = git(&["hash-object", "-w", "--no-filters", "state.txt"]);
        assert!(oid.status.success());
        let oid = std::str::from_utf8(&oid.stdout).unwrap().trim();
        assert!(
            git(&[
                "update-index",
                "--cacheinfo",
                &format!("100644,{oid},state.txt")
            ])
            .status
            .success()
        );
        std::fs::write(root.join(".gitattributes"), "state.txt filter=fixture\n").unwrap();
        std::fs::write(root.join("state.txt"), "after\n").unwrap();
        let marker = root.join("filter-executed");
        let quoted = format!(
            "'{}'",
            marker.display().to_string().replace('\'', "'\"'\"'")
        );
        let command = format!("printf fixture > {quoted}; exit 1");
        assert!(
            git(&["config", &format!("filter.fixture.{mode}"), &command])
                .status
                .success()
        );
        assert!(!marker.exists());
        let result = read(
            &workspace(root),
            &json!({"paths":["state.txt"],"include_untracked":false}),
        )
        .await
        .unwrap();
        let text = result["diff"].as_str().unwrap();
        assert!(
            text.contains("-staged") && text.contains("+after"),
            "{text}"
        );
        let staged = read(
            &workspace(root),
            &json!({"paths":["state.txt"],"include_untracked":false,"staged":true}),
        )
        .await
        .unwrap();
        let text = staged["diff"].as_str().unwrap();
        assert!(
            text.contains("-before") && text.contains("+staged"),
            "{text}"
        );
        assert!(
            !marker.exists(),
            "known general-mode Git filter execution risk: {mode}"
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn known_general_git_clean_filter_read_must_not_execute() {
        general_git_filter_fixture("clean").await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn known_general_git_process_filter_read_must_not_execute() {
        general_git_filter_fixture("process").await;
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn raw_worktree_review_covers_binary_deletion_mode_and_symlink_without_following() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        init(root);
        std::fs::write(root.join("binary"), b"before\0").unwrap();
        std::fs::write(root.join("deleted"), "removed\n").unwrap();
        std::fs::write(root.join("mode"), "unchanged\n").unwrap();
        std::fs::write(root.join("link"), "before\n").unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["add", "."])
                .current_dir(root)
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(root.join("binary"), b"after\0").unwrap();
        std::fs::remove_file(root.join("deleted")).unwrap();
        std::fs::set_permissions(root.join("mode"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("outside"), "OUTSIDE-NOT-READ").unwrap();
        std::fs::remove_file(root.join("link")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("outside"), root.join("link")).unwrap();
        let ws = workspace(root);
        let first = read(&ws, &json!({"include_untracked":false}))
            .await
            .unwrap();
        let diff = first["diff"].as_str().unwrap();
        assert!(diff.contains("Binary files a/binary") && diff.contains("deleted file mode"));
        assert!(diff.contains("new mode 100755") && diff.contains("new mode 120000"));
        assert!(!diff.contains("OUTSIDE-NOT-READ"));
        std::fs::write(root.join("binary"), b"different\0").unwrap();
        assert!(
            read(
                &ws,
                &json!({"include_untracked":false,"expected_version":first["version"]})
            )
            .await
            .is_err()
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn raw_worktree_review_empty_index_and_unmerged_index_fail_clearly() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        init(root);
        let ws = workspace(root);
        assert_eq!(
            read(&ws, &json!({"include_untracked":false}))
                .await
                .unwrap()["diff"],
            ""
        );
        std::fs::write(root.join("state.txt"), "fixture\n").unwrap();
        let output = std::process::Command::new("git")
            .args(["hash-object", "-w", "--no-filters", "state.txt"])
            .current_dir(root)
            .output()
            .unwrap();
        assert!(output.status.success());
        let oid = std::str::from_utf8(&output.stdout).unwrap().trim();
        let mut child = std::process::Command::new("git")
            .args(["update-index", "--index-info"])
            .current_dir(root)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        {
            use std::io::Write;
            child
                .stdin
                .take()
                .unwrap()
                .write_all(format!("100644 {oid} 1\tstate.txt\n").as_bytes())
                .unwrap();
        }
        assert!(child.wait().unwrap().success());
        let error = read(&ws, &json!({"include_untracked":false}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unmerged index"));
    }
    #[tokio::test]
    async fn untracked_text_and_binary_are_not_empty_diff() {
        let d = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(d.path())
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(d.path().join("new.rs"), "fn feature() {}\n").unwrap();
        std::fs::write(d.path().join("binary"), [0, 1, 2]).unwrap();
        let ws = Workspace {
            directory: None,
            id: "w".into(),
            device_id: "d".into(),
            root: d.path().canonicalize().unwrap(),
        };
        let v = read(&ws, &json!({})).await.unwrap();
        assert_eq!(v["untracked_count"], 2);
        assert!(v["diff"].as_str().unwrap().contains("+fn feature()"));
        assert!(v["diff"].as_str().unwrap().contains("binary"));
        let first = v["version"].clone();
        std::fs::write(d.path().join("new.rs"), "changed").unwrap();
        assert!(read(&ws, &json!({"expected_version":first})).await.is_err());
    }
}
