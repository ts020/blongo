use super::*;

struct Repo(PathBuf);

impl Repo {
    async fn new(name: &str, commit: bool) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "blongo-git-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "--quiet", "-b", "main"]).await.unwrap();
        git(&dir, &["config", "user.name", "Test"]).await.unwrap();
        git(&dir, &["config", "user.email", "test@localhost"])
            .await
            .unwrap();
        git(&dir, &["config", "commit.gpgsign", "false"])
            .await
            .unwrap();
        let repo = Self(dir);
        repo.write(".gitignore", "target/\n");
        repo.write("a.txt", "one\n");
        if commit {
            git(&repo.0, &["add", "-A"]).await.unwrap();
            git(&repo.0, &["commit", "--quiet", "-m", "init"])
                .await
                .unwrap();
        }
        repo
    }

    fn write(&self, path: &str, text: &str) {
        let path = self.0.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.0.join(path)).ok()
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn checkpoint_round_trip_restores_the_tree_exactly() {
    let repo = Repo::new("roundtrip", true).await;
    // Uncommitted state at checkpoint time: an edit and a new file.
    repo.write("a.txt", "two\n");
    repo.write("dir/new.txt", "new\n");
    repo.write("target/build.o", "ignored\n");
    let head_before = head(&repo.0).await.unwrap();
    let status_before = git(&repo.0, &["status", "--porcelain"]).await.unwrap();
    let r = checkpoint_ref("t1", "r1");
    let commit = capture_checkpoint(&repo.0, &r).await.unwrap();

    // HEAD, branches and the user's index are untouched; the commit is
    // only reachable from the hidden ref.
    assert_eq!(head(&repo.0).await.unwrap(), head_before);
    assert_eq!(git(&repo.0, &["rev-parse", &r]).await.unwrap(), commit);
    assert_eq!(
        git(&repo.0, &["status", "--porcelain"]).await.unwrap(),
        status_before
    );
    assert!(status_before.contains("a.txt") && status_before.contains("dir/"));
    assert!(
        !git(&repo.0, &["branch", "--list"])
            .await
            .unwrap()
            .contains("blongo")
    );

    // The agent changes things.
    repo.write("a.txt", "three\n");
    std::fs::remove_file(repo.0.join("dir/new.txt")).unwrap();
    repo.write("created.txt", "later\n");
    repo.write("dir/sub/deep.txt", "later\n");
    repo.write("target/other.o", "ignored too\n");

    restore_checkpoint(&repo.0, &commit).await.unwrap();
    assert_eq!(repo.read("a.txt").as_deref(), Some("two\n"));
    assert_eq!(repo.read("dir/new.txt").as_deref(), Some("new\n"));
    assert_eq!(repo.read("created.txt"), None);
    assert_eq!(repo.read("dir/sub/deep.txt"), None);
    // Ignored files are never touched.
    assert_eq!(repo.read("target/build.o").as_deref(), Some("ignored\n"));
    assert_eq!(
        repo.read("target/other.o").as_deref(),
        Some("ignored too\n")
    );
    assert_eq!(head(&repo.0).await.unwrap(), head_before);
    // Index back in sync with HEAD (the edit and new file are unstaged).
    assert_eq!(
        git(&repo.0, &["diff", "--cached", "--name-only"])
            .await
            .unwrap(),
        ""
    );
}

#[tokio::test]
async fn checkpoint_in_a_repo_without_commits() {
    let repo = Repo::new("empty", false).await;
    let commit = capture_checkpoint(&repo.0, &checkpoint_ref("t", "r"))
        .await
        .unwrap();
    repo.write("a.txt", "changed\n");
    repo.write("b.txt", "new\n");
    restore_checkpoint(&repo.0, &commit).await.unwrap();
    assert_eq!(repo.read("a.txt").as_deref(), Some("one\n"));
    assert_eq!(repo.read("b.txt"), None);
    assert!(head(&repo.0).await.is_none());
}

#[tokio::test]
async fn restore_of_an_empty_checkpoint_removes_everything_unignored() {
    let repo = Repo::new("emptytree", false).await;
    std::fs::remove_file(repo.0.join("a.txt")).unwrap();
    std::fs::remove_file(repo.0.join(".gitignore")).unwrap();
    let commit = capture_checkpoint(&repo.0, &checkpoint_ref("t", "r"))
        .await
        .unwrap();
    repo.write("x.txt", "x");
    restore_checkpoint(&repo.0, &commit).await.unwrap();
    assert_eq!(repo.read("x.txt"), None);
}

#[tokio::test]
async fn worktree_add_and_remove() {
    let repo = Repo::new("worktree", true).await;
    let path = repo.0.with_extension("wt");
    add_worktree(&repo.0, &path, "blongo/test").await.unwrap();
    assert_eq!(
        std::fs::read_to_string(path.join("a.txt")).unwrap(),
        "one\n"
    );
    assert_eq!(
        git(&path, &["rev-parse", "--abbrev-ref", "HEAD"])
            .await
            .unwrap(),
        "blongo/test"
    );
    assert_eq!(
        work_tree_root(&path).await.unwrap().canonicalize().unwrap(),
        path.canonicalize().unwrap()
    );
    // A second worktree on the same branch is refused by git.
    assert!(
        add_worktree(&repo.0, &repo.0.with_extension("wt2"), "blongo/test")
            .await
            .is_err()
    );
    remove_worktree(&repo.0, &path).await.unwrap();
    assert!(!path.exists());
    let empty = Repo::new("worktree-empty", false).await;
    let err = add_worktree(&empty.0, &empty.0.with_extension("wt"), "b")
        .await
        .unwrap_err();
    assert!(err.to_string().contains("no commits"), "{err}");
}

#[tokio::test]
async fn outside_git() {
    let dir = std::env::temp_dir().join(format!("blongo-nogit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // /tmp is not inside a repository in CI or here.
    if work_tree_root(&dir).await.is_none() {
        assert!(
            capture_checkpoint(&dir, &checkpoint_ref("t", "r"))
                .await
                .is_err()
        );
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn restore_keeps_what_was_staged() {
    let repo = Repo::new("staged", true).await;
    // a.txt partially staged: "two" staged, "three" in the work tree.
    repo.write("a.txt", "two\n");
    git(&repo.0, &["add", "a.txt"]).await.unwrap();
    repo.write("a.txt", "three\n");
    repo.write("staged-new.txt", "s\n");
    git(&repo.0, &["add", "staged-new.txt"]).await.unwrap();
    let commit = capture_checkpoint(&repo.0, &checkpoint_ref("t", "r"))
        .await
        .unwrap();
    // The agent changes everything and resets the index.
    repo.write("a.txt", "agent\n");
    repo.write("other.txt", "agent\n");
    git(&repo.0, &["reset", "--quiet"]).await.unwrap();
    restore_checkpoint(&repo.0, &commit).await.unwrap();
    assert_eq!(repo.read("a.txt").as_deref(), Some("three\n"));
    assert_eq!(repo.read("other.txt"), None);
    assert_eq!(
        git(&repo.0, &["show", ":a.txt"]).await.unwrap(),
        "two",
        "the staged version is back in the index"
    );
    assert_eq!(
        git(&repo.0, &["diff", "--cached", "--name-only"])
            .await
            .unwrap(),
        "a.txt\nstaged-new.txt"
    );
}

#[tokio::test]
async fn huge_untracked_files_skip_the_checkpoint() {
    let repo = Repo::new("huge", true).await;
    let file = std::fs::File::create(repo.0.join("big.bin")).unwrap();
    file.set_len(MAX_UNTRACKED_FILE + 1).unwrap();
    let err = capture_checkpoint(&repo.0, &checkpoint_ref("t", "r"))
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("big.bin"));
    // Ignored, it is fine.
    repo.write(".gitignore", "target/\nbig.bin\n");
    capture_checkpoint(&repo.0, &checkpoint_ref("t", "r"))
        .await
        .unwrap();
}
