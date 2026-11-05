// A build script reports failure by panicking — that is the interface cargo
// gives it. The crate-wide denials exist for the shipped binary, where a panic
// is a crash in front of a user; here it is a build error in a terminal.
#![allow(clippy::expect_used, clippy::panic)]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// OCR model weights, fetched at build time and embedded in the binary.
///
/// Kept out of git (12 MB of binary blobs) and out of the *runtime* too: the
/// app has to OCR offline, and must never fetch anything while handling a
/// user's file. Each entry is pinned by SHA-256, so a swapped or truncated
/// download fails the build instead of silently shipping.
///
/// Every URL for a model must serve the *same bytes*; the mirrors are tried
/// in order and the first one whose download matches the pin wins. The ocrs
/// author's S3 bucket is first because it is the canonical home, but it went
/// 403 `AccessDenied` in November 2026 (its README always said the URLs were
/// not guaranteed stable), so a Wayback Machine snapshot of each file is the
/// fallback. The `id_` flag asks the archive for the raw bytes, with no
/// rewriting, and the timestamp pins one snapshot rather than "latest".
///
/// Not a usable mirror: the `.rten` files on Hugging Face
/// (`robertknight/ocrs`). They are a 2024 export of the same checkpoints with
/// different bytes, and with them `ocr::tests::reads_text_out_of_an_image`
/// fails (reads `THello Biobbies`), so repinning to them would ship worse OCR.
const MODELS: [Model; 2] = [
    Model {
        name: "text-detection.rten",
        urls: &[
            "https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten",
            "https://web.archive.org/web/20261006150332id_/https://ocrs-models.s3-accelerate.amazonaws.com/text-detection.rten",
        ],
        sha256: "f15cfb56bd02c4bf478a20343986504a1f01e1665c2b3a0ad66340f054b1b5ca",
    },
    Model {
        name: "text-recognition.rten",
        urls: &[
            "https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten",
            "https://web.archive.org/web/20260904021412id_/https://ocrs-models.s3-accelerate.amazonaws.com/text-recognition.rten",
        ],
        sha256: "e484866d4cce403175bd8d00b128feb08ab42e208de30e42cd9889d8f1735a6e",
    },
];

struct Model {
    /// File name in `OUT_DIR`, and in `OCRS_MODEL_DIR`.
    name: &'static str,
    /// Mirrors, tried in order. All must serve bytes matching `sha256`.
    urls: &'static [&'static str],
    sha256: &'static str,
}

fn main() {
    fetch_models();
    build_acp_relay();
    tauri_build::build();
}

/// Build the ACP relay into the `externalBin` path Tauri expects.
///
/// `tauri_build::build()` refuses to run at all while
/// `binaries/blobbies-acp-<target-triple>` is missing, so this cannot be left
/// to a wrapper script: `cargo build`, `cargo clippy` and `cargo test` are all
/// entry points CI and a fresh clone use directly, and every one of them would
/// otherwise fail on a checkout that had never run the npm build.
///
/// Safe to nest inside a build script because the relay crate has **no
/// dependencies** — nothing to resolve and no registry lock to contend for —
/// and it compiles into its own target directory, so it never races the outer
/// build for the lock on this one.
fn build_acp_relay() {
    let crate_dir =
        PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo always sets this"));
    let manifest = crate_dir.join("acp-relay").join("Cargo.toml");
    println!(
        "cargo:rerun-if-changed={}",
        crate_dir.join("acp-relay").join("src").display()
    );
    println!("cargo:rerun-if-changed={}", manifest.display());

    let target = std::env::var("TARGET").expect("cargo always sets TARGET");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("cargo always sets OUT_DIR"));
    let relay_target = out_dir.join("acp-relay");
    let suffix = if target.contains("windows") {
        ".exe"
    } else {
        ""
    };

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let mut command = std::process::Command::new(cargo);
    command
        .args(["build", "--release", "--manifest-path"])
        .arg(&manifest)
        .arg("--target-dir")
        .arg(&relay_target)
        .args(["--target", &target]);
    // Cargo exports the *parent* build's flags and target selection into a
    // build script's environment. Inherited, they would rebuild the relay with
    // the wrong settings, or point it back at this crate.
    for leaked in [
        "CARGO_ENCODED_RUSTFLAGS",
        "RUSTFLAGS",
        "CARGO_BUILD_TARGET",
        "CARGO_BUILD_RUSTFLAGS",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
    ] {
        command.env_remove(leaked);
    }

    let status = command
        .status()
        .unwrap_or_else(|error| panic!("could not run cargo to build the ACP relay: {error}"));
    assert!(status.success(), "building the ACP relay failed");

    let built = relay_target
        .join(&target)
        .join("release")
        .join(format!("blobbies-acp{suffix}"));
    let bundled = crate_dir
        .join("binaries")
        .join(format!("blobbies-acp-{target}{suffix}"));
    std::fs::create_dir_all(bundled.parent().expect("joined a file name"))
        .unwrap_or_else(|error| panic!("creating the binaries directory: {error}"));
    std::fs::copy(&built, &bundled).unwrap_or_else(|error| {
        panic!(
            "copying {} to {}: {error}",
            built.display(),
            bundled.display()
        )
    });
}

/// Put every model in `OUT_DIR`, fetching only what is missing or wrong.
///
/// `OCRS_MODEL_DIR` points at already-downloaded copies for offline and
/// air-gapped builds. The checksum is still enforced, so it is a cache, not a
/// bypass.
fn fetch_models() {
    println!("cargo:rerun-if-env-changed=OCRS_MODEL_DIR");
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("cargo always sets OUT_DIR"));
    let local = std::env::var("OCRS_MODEL_DIR").ok().map(PathBuf::from);

    for model in &MODELS {
        let target = out_dir.join(model.name);
        if file_matches(&target, model.sha256) {
            continue;
        }
        let bytes = match local.as_ref().map(|dir| dir.join(model.name)) {
            Some(path) if path.exists() => {
                let bytes = std::fs::read(&path)
                    .unwrap_or_else(|error| panic!("reading {}: {error}", path.display()));
                let found = hex_sha256(&bytes);
                assert!(
                    found == model.sha256,
                    "{} failed its checksum.\n  expected {}\n  found    {found}\n\
                     Refusing to embed an OCR model that is not the pinned one.",
                    path.display(),
                    model.sha256,
                );
                bytes
            }
            _ => download_pinned(model),
        };
        std::fs::write(&target, &bytes)
            .unwrap_or_else(|error| panic!("writing {}: {error}", target.display()));
    }
}

fn file_matches(path: &Path, sha256: &str) -> bool {
    std::fs::read(path).is_ok_and(|bytes| hex_sha256(&bytes) == sha256)
}

/// Try each mirror in turn; the first download whose bytes match the pin is
/// the one. A mirror that is down, or serves something else, is skipped with
/// its reason kept for the error, so a build that fails says what every
/// mirror did instead of only the last one.
fn download_pinned(model: &Model) -> Vec<u8> {
    let mut failures = Vec::new();
    for url in model.urls {
        match download(url) {
            Ok(bytes) if hex_sha256(&bytes) == model.sha256 => return bytes,
            Ok(bytes) => failures.push(format!(
                "{url}: checksum mismatch\n    expected {}\n    found    {}",
                model.sha256,
                hex_sha256(&bytes)
            )),
            Err(error) => failures.push(format!("{url}: {error}")),
        }
    }
    panic!(
        "no mirror served the pinned {}:\n  {}\n\
         Refusing to embed an OCR model that is not the pinned one.\n\
         Set OCRS_MODEL_DIR to a directory holding the model files to build offline.",
        model.name,
        failures.join("\n  "),
    );
}

/// Fetch with the platform's own HTTP client, so downloading at build time
/// does not pull a second TLS stack into the dependency tree.
fn download(url: &str) -> Result<Vec<u8>, String> {
    let output = std::process::Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--silent",
            "--show-error",
            "--output",
            "-",
            url,
        ])
        .output()
        .map_err(|error| format!("could not run curl: {error}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_owned());
    }
    Ok(output.stdout)
}

fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}
