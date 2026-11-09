//! H1 end to end: `build` writes an identity, `verify` checks it, and a rewritten
//! artifact is refused even when every FNV in it has been recomputed.
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_rustsearch")
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rustsearch-verify-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn write_site(dir: &Path, body: &str) {
    fs::create_dir_all(dir).expect("site dir");
    for page in 0..3 {
        let html = format!(
            "<html><head><title>Page {page}</title></head><body><main>\
             {body} page {page} of the site</main></body></html>"
        );
        fs::write(dir.join(format!("page{page}.html")), html).expect("page");
    }
}

fn run(args: &[&str]) -> Output {
    Command::new(bin())
        .args(args)
        .output()
        .expect("run rustsearch")
}

fn sidecar(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.sha256", path.display()))
}

#[test]
fn a_pinned_digest_refuses_a_rewritten_artifact() {
    let root = temp_dir("digest");
    let site = root.join("site");
    write_site(&site, "walkdir index handy");
    let first = root.join("first.rsx");
    let output = run(&[
        "build",
        site.to_str().unwrap(),
        "-o",
        first.to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let pinned = fs::read_to_string(sidecar(&first)).expect("build writes the sidecar");
    assert!(pinned.trim().starts_with("sha256:"));
    assert_eq!(pinned.trim().len(), "sha256:".len() + 64);

    // The sidecar makes the plain command answer the deploy question.
    let checked = run(&["verify", first.to_str().unwrap()]);
    assert!(
        checked.status.success(),
        "verify failed: {}",
        String::from_utf8_lossy(&checked.stderr)
    );
    assert!(String::from_utf8_lossy(&checked.stdout).contains("digest: ok"));

    // A second artifact, built from different pages, is a different identity: the
    // pinned digest of the first refuses it.
    write_site(&site, "serde_json parser tokens");
    let second = root.join("second.rsx");
    let output = run(&[
        "build",
        site.to_str().unwrap(),
        "-o",
        second.to_str().unwrap(),
    ]);
    assert!(output.status.success());
    let refused = run(&[
        "verify",
        second.to_str().unwrap(),
        "--expect-digest",
        pinned.trim(),
    ]);
    assert!(
        !refused.status.success(),
        "a different artifact must not pass the pinned digest"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("digest mismatch"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );

    // Flipping one byte of the artifact is caught by the digest even though the
    // FNV the format carries is untouched.
    let mut bytes = fs::read(&first).expect("artifact");
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    fs::write(&first, &bytes).expect("tamper");
    let tampered = run(&["verify", first.to_str().unwrap()]);
    assert!(
        !tampered.status.success(),
        "a tampered artifact must fail its own sidecar"
    );

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_shard_manifest_carries_its_own_identity() {
    let root = temp_dir("manifest");
    let site = root.join("site");
    write_site(&site, "walkdir index handy");
    let artifact = root.join("index.rsx");
    assert!(run(&[
        "build",
        site.to_str().unwrap(),
        "-o",
        artifact.to_str().unwrap(),
        "--term-chunk-bytes",
        "1k",
    ])
    .status
    .success());
    let shards = root.join("shards");
    let output = run(&[
        "shard",
        artifact.to_str().unwrap(),
        "-o",
        shards.to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "shard failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let manifest = shards.join("manifest.txt");
    assert!(
        sidecar(&manifest).exists(),
        "shard writes the manifest digest"
    );
    let checked = run(&["verify", manifest.to_str().unwrap()]);
    assert!(
        checked.status.success(),
        "{}",
        String::from_utf8_lossy(&checked.stderr)
    );

    // Rewrite the manifest (and drop its sidecar): the pinned digest of the set the
    // deploy published refuses it.
    let pinned = fs::read_to_string(sidecar(&manifest)).expect("sidecar");
    // A byte the parser ignores (a comment line) is still a different manifest: the
    // digest covers the bytes, not the meaning the client gives them.
    let mut text = fs::read_to_string(&manifest).expect("manifest");
    text.push_str("# rebuilt elsewhere\n");
    fs::write(&manifest, text).expect("tamper");
    fs::remove_file(sidecar(&manifest)).expect("remove sidecar");
    let refused = run(&[
        "verify",
        manifest.to_str().unwrap(),
        "--expect-digest",
        pinned.trim(),
    ]);
    assert!(!refused.status.success());

    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_signature_that_does_not_verify_is_an_error() {
    let root = temp_dir("signature");
    let site = root.join("site");
    write_site(&site, "walkdir index handy");
    let artifact = root.join("index.rsx");
    assert!(run(&[
        "build",
        site.to_str().unwrap(),
        "-o",
        artifact.to_str().unwrap()
    ])
    .status
    .success());
    // A syntactically plausible but wrong key/signature pair: `verify` must fail,
    // never warn and continue.
    let key = root.join("key.pub");
    fs::write(
        &key,
        "untrusted comment: test key\nRWQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
    )
    .expect("key");
    let sig = root.join("index.rsx.minisig");
    fs::write(
        &sig,
        "untrusted comment: test signature\nRWSAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==\ntrusted comment: test\ntimestamp:0\tfile:index.rsx\nRWQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n",
    )
    .expect("sig");
    let refused = run(&[
        "verify",
        artifact.to_str().unwrap(),
        "--require-signature",
        sig.to_str().unwrap(),
        "--public-key",
        key.to_str().unwrap(),
    ]);
    assert!(!refused.status.success(), "a bad signature must be refused");

    // Asking for a signature without a key is a usage error, not a silent pass.
    let missing_key = run(&[
        "verify",
        artifact.to_str().unwrap(),
        "--require-signature",
        sig.to_str().unwrap(),
    ]);
    assert!(!missing_key.status.success());
    assert!(
        String::from_utf8_lossy(&missing_key.stderr).contains("--public-key"),
        "{}",
        String::from_utf8_lossy(&missing_key.stderr)
    );

    let _ = fs::remove_dir_all(&root);
}
