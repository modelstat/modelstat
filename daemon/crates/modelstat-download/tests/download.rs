//! Downloader integration tests against a fake file server (Range-capable), plus
//! the checksum-verify + resume + already-present paths.

use std::path::PathBuf;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use modelstat_download::{download, DownloadError, DownloadSpec, SilentSink};
use sha2::{Digest, Sha256};

const BODY_LEN: usize = 300_000;

fn body() -> Vec<u8> {
    (0..BODY_LEN).map(|i| (i % 251) as u8).collect()
}

fn body_sha() -> String {
    let d = Sha256::digest(body());
    d.iter().map(|b| format!("{b:02x}")).collect()
}

#[derive(Clone, Copy)]
struct Fake {
    supports_range: bool,
}

async fn serve(State(fake): State<Fake>, headers: HeaderMap) -> axum::response::Response {
    let full = body();
    if fake.supports_range {
        if let Some(r) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
            if let Some(start) = r.strip_prefix("bytes=").and_then(|s| s.trim_end_matches('-').parse::<usize>().ok()) {
                let slice = full[start.min(full.len())..].to_vec();
                return (StatusCode::PARTIAL_CONTENT, slice).into_response();
            }
        }
    }
    (StatusCode::OK, full).into_response()
}

async fn spawn(supports_range: bool) -> String {
    let app = Router::new()
        .route("/file", get(serve))
        .with_state(Fake { supports_range });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{addr}/file")
}

fn tmp(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("modelstat-dl-{}-{name}", std::process::id()))
}

#[tokio::test]
async fn full_download_verifies_checksum_and_renames() {
    let url = spawn(false).await;
    let dest = tmp("full.bin");
    let _ = std::fs::remove_file(&dest);
    let spec = DownloadSpec {
        url,
        dest: dest.clone(),
        expected_sha256: Some(body_sha()),
        size_label: Some("~300 KB".into()),
        label: "test-artifact".into(),
    };
    let client = reqwest::Client::new();
    let out = download(&client, &spec, &SilentSink).await.unwrap();
    assert_eq!(out, dest);
    assert_eq!(std::fs::read(&dest).unwrap(), body());
    // The `.partial` is gone after the atomic rename.
    assert!(!dest.with_extension("bin.partial").exists());
    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn checksum_mismatch_is_an_error_and_deletes_partial() {
    let url = spawn(false).await;
    let dest = tmp("bad.bin");
    let _ = std::fs::remove_file(&dest);
    let spec = DownloadSpec {
        url,
        dest: dest.clone(),
        expected_sha256: Some("deadbeef".repeat(8)),
        size_label: None,
        label: "test".into(),
    };
    let client = reqwest::Client::new();
    let err = download(&client, &spec, &SilentSink).await.unwrap_err();
    assert!(matches!(err, DownloadError::ChecksumMismatch { .. }), "got {err}");
    assert!(!dest.exists());
    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn resumes_from_a_partial() {
    let url = spawn(true).await;
    let dest = tmp("resume.bin");
    let partial = {
        let mut s = dest.clone().into_os_string();
        s.push(".partial");
        PathBuf::from(s)
    };
    let _ = std::fs::remove_file(&dest);
    // Seed the first half as a prior partial download.
    std::fs::write(&partial, &body()[..150_000]).unwrap();

    let spec = DownloadSpec {
        url,
        dest: dest.clone(),
        expected_sha256: Some(body_sha()),
        size_label: None,
        label: "test".into(),
    };
    let client = reqwest::Client::new();
    download(&client, &spec, &SilentSink).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), body());
    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn already_present_is_a_noop() {
    let dest = tmp("present.bin");
    std::fs::write(&dest, b"already here").unwrap();
    let spec = DownloadSpec {
        url: "http://127.0.0.1:1/never".into(),
        dest: dest.clone(),
        expected_sha256: None,
        size_label: None,
        label: "test".into(),
    };
    let client = reqwest::Client::new();
    let out = download(&client, &spec, &SilentSink).await.unwrap();
    assert_eq!(out, dest);
    assert_eq!(std::fs::read(&dest).unwrap(), b"already here");
    let _ = std::fs::remove_file(&dest);
}
