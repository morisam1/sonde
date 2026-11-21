//! The updater against a stand-in for GitHub's release API: it finds the
//! newer release, downloads its package, refuses one whose checksum does not
//! match, and installs the good one.

use std::{
    collections::HashMap,
    io::{BufRead as _, BufReader, Write as _},
    net::TcpListener,
    sync::Arc,
    thread,
};

use futures::executor::block_on;
use semver::Version;
use sha2::{Digest as _, Sha256};
use sonde_core::update::{self, Channel, Source};

fn package_bytes() -> Vec<u8> {
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::fast(),
    ));
    let program = if cfg!(windows) { "sonde.exe" } else { "sonde" };
    let mut header = tar::Header::new_gnu();
    header.set_size(11);
    header.set_mode(0o755);
    header.set_cksum();
    tar.append_data(&mut header, format!("sonde/{program}"), &b"new version"[..])
        .unwrap();
    tar.into_inner().unwrap().finish().unwrap()
}

/// Serve `files` by path on a free port until the test ends.
fn serve(files: HashMap<String, Vec<u8>>) -> String {
    serve_on("127.0.0.1:0", files)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// A stand-in whose release 9.0.0 carries `package` with `checksum`.
fn release_server(package: Vec<u8>, checksum: String) -> String {
    // The release list names download URLs, so it needs the address first.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let name = update::package_name(update::target());
    let list = serde_json::json!([{
        "tag_name": "v9.0.0",
        "name": "Sonde 9.0.0",
        "body": "Notes",
        "html_url": "https://example.test/v9.0.0",
        "draft": false,
        "prerelease": false,
        "assets": [
            {"name": name, "browser_download_url": format!("{base}/dl/{name}")},
            {"name": format!("{name}.sha256"), "browser_download_url": format!("{base}/dl/{name}.sha256")}
        ]
    }]);
    let mut files = HashMap::new();
    files.insert(
        "/repos/me/sonde/releases".to_owned(),
        list.to_string().into_bytes(),
    );
    files.insert(format!("/dl/{name}"), package);
    files.insert(
        format!("/dl/{name}.sha256"),
        format!("{checksum}  {name}\n").into_bytes(),
    );
    // Serve on the address the list names.
    let served = serve_on(base.trim_start_matches("http://"), files);
    assert_eq!(served, base);
    base
}

/// Serve `files` by path on `addr` until the test ends; returns the base URL.
fn serve_on(addr: &str, files: HashMap<String, Vec<u8>>) -> String {
    let listener = TcpListener::bind(addr).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let files = Arc::new(files);
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut line = String::new();
            let _ = reader.read_line(&mut line);
            let path = line
                .split_whitespace()
                .nth(1)
                .unwrap_or("")
                .split('?')
                .next()
                .unwrap_or("")
                .to_owned();
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                    break;
                }
            }
            let mut stream = stream;
            match files.get(&path) {
                Some(body) => {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(body);
                }
                None => {
                    let _ = write!(
                        stream,
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                }
            }
        }
    });
    url
}

fn source(base: &str) -> Source {
    Source {
        api: base.to_owned(),
        repository: "me/sonde".into(),
    }
}

#[test]
fn a_newer_release_should_download_verify_and_install() {
    if cfg!(target_os = "macos") {
        return; // macOS packages hold an app bundle; covered by the release workflow.
    }
    let package = package_bytes();
    let base = release_server(package.clone(), hex(&Sha256::digest(&package)));
    let current = Version::parse("0.1.0").unwrap();

    let release = block_on(update::check(&source(&base), Channel::Stable, &current))
        .unwrap()
        .expect("9.0.0 is newer");
    assert_eq!(release.version.to_string(), "9.0.0");

    let dir = std::env::temp_dir().join(format!("sonde-update-it-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let downloaded = block_on(update::download(&release, &dir)).unwrap();
    let exe = dir.join(if cfg!(windows) { "sonde.exe" } else { "sonde" });
    std::fs::write(&exe, b"old").unwrap();
    update::install(&downloaded, &exe).unwrap();
    assert_eq!(std::fs::read(&exe).unwrap(), b"new version");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        block_on(update::check(
            &source(&base),
            Channel::Stable,
            &Version::parse("9.0.0").unwrap()
        ))
        .unwrap()
        .is_none()
    );
}

#[test]
fn a_package_that_does_not_match_its_checksum_should_be_refused() {
    let base = release_server(package_bytes(), "0".repeat(64));
    let current = Version::parse("0.1.0").unwrap();
    let release = block_on(update::check(&source(&base), Channel::Stable, &current))
        .unwrap()
        .unwrap();
    let dir = std::env::temp_dir().join(format!("sonde-update-bad-{}", std::process::id()));

    let error = block_on(update::download(&release, &dir)).unwrap_err();

    assert!(
        error.to_string().contains("does not match its checksum"),
        "{error}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_repository_should_say_so() {
    let base = serve(HashMap::new());

    let error = block_on(update::check(
        &source(&base),
        Channel::Stable,
        &Version::parse("0.1.0").unwrap(),
    ))
    .unwrap_err();

    assert!(error.to_string().contains("no public releases"), "{error}");
}
