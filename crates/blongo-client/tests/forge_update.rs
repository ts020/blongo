//! The PR inbox (GitHub and GitLab shapes) and the update check against a
//! local fake HTTP server, through the real curl. No real forge or update
//! server is ever contacted.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use blongo_client::forge::{Forge, ForgeKind, ReviewComment};
use blongo_client::update;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};

#[derive(Clone, Debug)]
struct Seen {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

/// A fake server answering `routes` (method + path → status, body);
/// records every request.
type Routes = Vec<(&'static str, String, u16, Vec<u8>)>;

fn fake_server(routes: Routes) -> (String, Arc<Mutex<Vec<Seen>>>) {
    serve(TcpListener::bind("127.0.0.1:0").unwrap(), routes)
}

fn serve(listener: TcpListener, routes: Routes) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut parts = line.split_whitespace();
            let method = parts.next().unwrap_or("").to_owned();
            let path = parts.next().unwrap_or("").to_owned();
            let mut headers = HashMap::new();
            loop {
                let mut h = String::new();
                reader.read_line(&mut h).unwrap();
                let h = h.trim_end();
                if h.is_empty() {
                    break;
                }
                if let Some((k, v)) = h.split_once(':') {
                    headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
                }
            }
            let len: usize = headers
                .get("content-length")
                .and_then(|l| l.parse().ok())
                .unwrap_or(0);
            let mut body = vec![0; len];
            reader.read_exact(&mut body).unwrap();
            log.lock().unwrap().push(Seen {
                method: method.clone(),
                path: path.clone(),
                headers,
                body: String::from_utf8_lossy(&body).into_owned(),
            });
            let (status, body) = routes
                .iter()
                .find(|(m, p, _, _)| *m == method && *p == path)
                .map(|(_, _, s, b)| (*s, b.clone()))
                .unwrap_or((404, br#"{"message":"Not Found"}"#.to_vec()));
            let head = format!(
                "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(&body);
        }
    });
    (format!("http://{addr}"), seen)
}

fn j(v: Value) -> Vec<u8> {
    v.to_string().into_bytes()
}

#[tokio::test]
async fn github_inbox_files_and_review() {
    let (base, seen) = fake_server(vec![
        (
            "GET",
            "/search/issues?q=is%3Apr+is%3Aopen+review-requested%3A%40me&per_page=50".into(),
            200,
            j(json!({"items": [{
                "number": 42, "title": "Fix the parser",
                "repository_url": "https://api.github.com/repos/acme/widgets",
                "html_url": "https://github.com/acme/widgets/pull/42",
                "user": {"login": "alice"}, "updated_at": "2026-10-01T10:00:00Z"
            }]})),
        ),
        (
            "GET",
            "/repos/acme/widgets/pulls/42".into(),
            200,
            j(json!({"head": {"sha": "abc123"}, "base": {"sha": "def456"}})),
        ),
        (
            "GET",
            "/repos/acme/widgets/pulls/42/files?per_page=300".into(),
            200,
            j(json!([{
                "filename": "src/parse.rs", "status": "modified", "additions": 1,
                "deletions": 1, "patch": "@@ -1,2 +1,2 @@\n fn a() {}\n-fn b() {}\n+fn c() {}"
            }])),
        ),
        (
            "POST",
            "/repos/acme/widgets/pulls/42/reviews".into(),
            200,
            j(json!({"id": 1})),
        ),
    ]);
    let forge = Forge {
        kind: ForgeKind::GitHub,
        api: base,
        token: "ghp_secret".into(),
    };
    let prs = forge.inbox().await.unwrap();
    assert_eq!(prs.len(), 1);
    assert_eq!(prs[0].repo, "acme/widgets");
    assert_eq!(prs[0].key(), "acme/widgets#42");
    assert_eq!(prs[0].author, "alice");
    let detail = forge.detail(&prs[0]).await.unwrap();
    assert_eq!(detail.head_sha, "abc123");
    assert_eq!(detail.files[0].path, "src/parse.rs");
    assert!(detail.files[0].patch.as_ref().unwrap().contains("+fn c()"));
    forge
        .review(
            &prs[0],
            &detail,
            "Looks good overall",
            &[ReviewComment {
                path: "src/parse.rs".into(),
                line: 2,
                body: "Why rename b?".into(),
            }],
        )
        .await
        .unwrap();
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.iter()
            .all(|s| s.headers["authorization"] == "Bearer ghp_secret")
    );
    let post = seen.iter().find(|s| s.method == "POST").unwrap();
    let body: Value = serde_json::from_str(&post.body).unwrap();
    assert_eq!(body["commit_id"], "abc123");
    assert_eq!(body["comments"][0]["line"], 2);
    assert_eq!(body["comments"][0]["side"], "RIGHT");

    // A refused token is a readable error.
    let (base, _) = fake_server(vec![(
        "GET",
        "/search/issues?q=is%3Apr+is%3Aopen+review-requested%3A%40me&per_page=50".into(),
        401,
        j(json!({"message": "Bad credentials"})),
    )]);
    let err = Forge {
        kind: ForgeKind::GitHub,
        api: base,
        token: "bad".into(),
    }
    .inbox()
    .await
    .unwrap_err();
    assert!(
        err.contains("401") && err.contains("Bad credentials"),
        "{err}"
    );
}

#[tokio::test]
async fn gitlab_inbox_changes_and_discussions() {
    let (base, seen) = fake_server(vec![
        ("GET", "/user".into(), 200, j(json!({"id": 7}))),
        (
            "GET",
            "/merge_requests?state=opened&scope=all&reviewer_id=7&per_page=50".into(),
            200,
            j(json!([{
                "iid": 5, "project_id": 99, "title": "Add cache",
                "references": {"full": "group/app!5"}, "author": {"username": "bob"},
                "web_url": "https://gitlab.example/group/app/-/merge_requests/5",
                "updated_at": "2026-10-02T09:00:00Z"
            }])),
        ),
        (
            "GET",
            "/projects/99/merge_requests/5/changes".into(),
            200,
            j(json!({
                "diff_refs": {"base_sha": "b1", "head_sha": "h1", "start_sha": "s1"},
                "changes": [{"old_path": "a.py", "new_path": "a.py", "new_file": false,
                             "deleted_file": false, "renamed_file": false,
                             "diff": "@@ -1 +1,2 @@\n x = 1\n+y = 2\n"}]
            })),
        ),
        (
            "POST",
            "/projects/99/merge_requests/5/discussions".into(),
            201,
            j(json!({"id": "d"})),
        ),
        (
            "POST",
            "/projects/99/merge_requests/5/notes".into(),
            201,
            j(json!({"id": 3})),
        ),
    ]);
    let forge = Forge {
        kind: ForgeKind::GitLab,
        api: base,
        token: "glpat-secret".into(),
    };
    let mrs = forge.inbox().await.unwrap();
    assert_eq!(mrs[0].repo, "group/app");
    assert_eq!(mrs[0].project_id, Some(99));
    let detail = forge.detail(&mrs[0]).await.unwrap();
    assert_eq!((detail.files[0].added, detail.files[0].removed), (1, 0));
    forge
        .review(
            &mrs[0],
            &detail,
            "Summary",
            &[ReviewComment {
                path: "a.py".into(),
                line: 2,
                body: "Name it better".into(),
            }],
        )
        .await
        .unwrap();
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.iter()
            .all(|s| s.headers["private-token"] == "glpat-secret")
    );
    let d = seen
        .iter()
        .find(|s| s.path.ends_with("/discussions"))
        .unwrap();
    let body: Value = serde_json::from_str(&d.body).unwrap();
    assert_eq!(body["position"]["head_sha"], "h1");
    assert_eq!(body["position"]["new_line"], 2);
    assert!(seen.iter().any(|s| s.path.ends_with("/notes")));
}

#[tokio::test]
async fn update_check_and_verified_download() {
    let key = SigningKey::from_bytes(&[3u8; 32]);
    let payload = b"pretend this is a release archive".to_vec();
    let sha: String = {
        use sha2::Digest;
        sha2::Sha256::digest(&payload)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    };
    // The manifest names the file on the same fake server: bind first to
    // learn the address.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let manifest = update::Manifest {
        version: "99.0.0".into(),
        url: format!("http://127.0.0.1:{port}/blongo-99.tar.gz"),
        sha256: sha.clone(),
        notes: "big release".into(),
    };
    let sign = |m: &update::Manifest| {
        let bytes = serde_json::to_vec(m).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD;
        j(
            json!({"manifest": b64.encode(&bytes), "signature": b64.encode(key.sign(&bytes).to_bytes())}),
        )
    };
    let bad = update::Manifest {
        url: format!("http://127.0.0.1:{port}/bad.tar.gz"),
        ..manifest.clone()
    };
    let routes = vec![
        ("GET", "/manifest.json".into(), 200, sign(&manifest)),
        ("GET", "/bad-manifest.json".into(), 200, sign(&bad)),
        ("GET", "/blongo-99.tar.gz".into(), 200, payload.clone()),
        ("GET", "/bad.tar.gz".into(), 200, b"tampered".to_vec()),
    ];
    let (base, _) = serve(listener, routes);

    let status = update::check(
        &format!("{base}/manifest.json"),
        &key.verifying_key(),
        "0.0.1",
    )
    .await
    .unwrap();
    let update::Status::Available(found) = status else {
        panic!("{status:?}")
    };
    assert_eq!(found, manifest);
    let dir = std::env::temp_dir().join(format!("blongo-update-test-{port}"));
    let file = update::download(&found, &dir).await.unwrap();
    assert_eq!(std::fs::read(&file).unwrap(), payload);
    // Same version: up to date.
    assert!(matches!(
        update::check(
            &format!("{base}/manifest.json"),
            &key.verifying_key(),
            "99.0.0"
        )
        .await
        .unwrap(),
        update::Status::UpToDate { .. }
    ));
    // A file that does not match the signed hash is refused and removed.
    let update::Status::Available(bad) = update::check(
        &format!("{base}/bad-manifest.json"),
        &key.verifying_key(),
        "0.0.1",
    )
    .await
    .unwrap() else {
        panic!()
    };
    let err = update::download(&bad, &dir).await.unwrap_err();
    assert!(err.contains("does not match"), "{err}");
    assert!(!dir.join("bad.tar.gz").exists());
    // Another key's signature is refused.
    let other = SigningKey::from_bytes(&[4u8; 32]);
    assert!(
        update::check(
            &format!("{base}/manifest.json"),
            &other.verifying_key(),
            "0.0.1"
        )
        .await
        .is_err()
    );
    let _ = std::fs::remove_dir_all(&dir);
}
