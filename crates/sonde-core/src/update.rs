//! Finding, downloading and installing new versions of Sonde.
//!
//! Releases live on GitHub. Each carries one package per platform, named
//! `sonde-<target>.tar.gz` (Linux) or `sonde-<target>.zip` (Windows, and
//! macOS, where it holds `Sonde.app`), and a `<package>.sha256` beside it. A
//! package is installed only when its checksum matches.
//!
//! The stable channel takes releases; the nightly channel also takes
//! prereleases. Nightly builds are versioned `X.Y.Z-nightly.DATE.RUN`, which
//! orders them before the release they lead up to.

use std::{
    fs,
    io::{self, Read as _},
    path::{Path, PathBuf},
    time::Duration,
};

use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::error::{Error, Result};

/// Where Sonde's releases are published, as `owner/name` on GitHub.
pub const REPOSITORY: &str = "morisam1/sonde";

const API: &str = "https://api.github.com";
const TIMEOUT: Duration = Duration::from_secs(30);

/// Which releases to follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Tagged releases only.
    #[default]
    Stable,
    /// Tagged releases and the nightly build of the main branch.
    Nightly,
}

/// Where to look for releases. The defaults are GitHub and [`REPOSITORY`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// The API's base URL, without a trailing slash.
    pub api: String,
    /// `owner/name`.
    pub repository: String,
}

impl Default for Source {
    fn default() -> Self {
        Self {
            api: API.to_owned(),
            repository: REPOSITORY.to_owned(),
        }
    }
}

/// A release newer than the running version, with this platform's package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Its version.
    pub version: Version,
    /// The release notes, as Markdown.
    pub notes: String,
    /// The release's page, for people to read.
    pub page: String,
    /// Whether it is a nightly build.
    pub prerelease: bool,
    /// The package for this platform.
    pub package: Asset,
    /// The package's SHA-256 checksum file.
    pub checksum: Asset,
}

/// A file attached to a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    /// The file name.
    pub name: String,
    /// Where to download it.
    pub url: String,
}

/// The platform this build runs on, as release packages name it.
pub fn target() -> &'static str {
    if cfg!(target_os = "macos") {
        "universal-apple-darwin"
    } else if cfg!(all(target_os = "windows", target_arch = "aarch64")) {
        "aarch64-pc-windows-msvc"
    } else if cfg!(target_os = "windows") {
        "x86_64-pc-windows-msvc"
    } else if cfg!(target_arch = "aarch64") {
        "aarch64-unknown-linux-gnu"
    } else {
        "x86_64-unknown-linux-gnu"
    }
}

/// The package file name for `target`.
pub fn package_name(target: &str) -> String {
    if target.contains("linux") {
        format!("sonde-{target}.tar.gz")
    } else {
        format!("sonde-{target}.zip")
    }
}

#[derive(Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    body: Option<String>,
    html_url: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GitHubAsset>,
}

#[derive(Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

/// The version a release carries: its tag (`v1.2.3`), or for the moving
/// `nightly` tag, the last word of its title.
fn version_of(release: &GitHubRelease) -> Option<Version> {
    let tag = release.tag_name.trim_start_matches('v');
    Version::parse(tag).ok().or_else(|| {
        release
            .name
            .as_deref()?
            .split_whitespace()
            .last()
            .and_then(|word| Version::parse(word.trim_start_matches('v')).ok())
    })
}

/// The newest release on `channel` that is newer than `current` and has a
/// package for `target`.
fn pick(
    releases: Vec<GitHubRelease>,
    channel: Channel,
    current: &Version,
    target: &str,
) -> Option<Release> {
    let package = package_name(target);
    let checksum = format!("{package}.sha256");
    releases
        .into_iter()
        .filter(|r| !r.draft && (channel == Channel::Nightly || !r.prerelease))
        .filter_map(|r| {
            let version = version_of(&r)?;
            if &version <= current {
                return None;
            }
            let find = |name: &str| {
                r.assets.iter().find(|a| a.name == name).map(|a| Asset {
                    name: a.name.clone(),
                    url: a.browser_download_url.clone(),
                })
            };
            Some(Release {
                package: find(&package)?,
                checksum: find(&checksum)?,
                version,
                notes: r.body.clone().unwrap_or_default(),
                page: r.html_url.clone(),
                prerelease: r.prerelease,
            })
        })
        .max_by(|a, b| a.version.cmp(&b.version))
}

fn client() -> Result<reqwest::Client> {
    crate::tls_provider();
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(concat!("Sonde/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| Error::Config(error.to_string()))
}

fn http_error(error: &reqwest::Error) -> Error {
    Error::Connect(format!("could not reach the update server: {error}"))
}

/// Ask `source` for a release newer than `current`. `None` when Sonde is up
/// to date, or no newer release has a package for this platform.
///
/// # Errors
///
/// [`Error::Connect`] when the server cannot be reached or answers with
/// something other than a release list.
pub async fn check(
    source: &Source,
    channel: Channel,
    current: &Version,
) -> Result<Option<Release>> {
    let source = source.clone();
    let current = current.clone();
    crate::stream::bridge(async move {
        let url = format!(
            "{}/repos/{}/releases?per_page=30",
            source.api, source.repository
        );
        let response = client()?
            .get(url)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|error| http_error(&error))?;
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Err(Error::Connect(format!(
                "{} has no public releases to check",
                source.repository
            )));
        }
        if !response.status().is_success() {
            return Err(Error::Connect(format!(
                "the update server answered {}",
                response.status()
            )));
        }
        let releases: Vec<GitHubRelease> = response.json().await.map_err(|error| {
            Error::Connect(format!("the release list could not be read: {error}"))
        })?;
        Ok(pick(releases, channel, &current, target()))
    })
    .await
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Download `release`'s package into `dir` and check it against its
/// checksum. Returns the package's path.
///
/// # Errors
///
/// [`Error::Connect`] when a download fails, [`Error::Config`] when the
/// package does not match its checksum (it is then deleted).
pub async fn download(release: &Release, dir: &Path) -> Result<PathBuf> {
    let release = release.clone();
    let dir = dir.to_owned();
    crate::stream::bridge(async move {
        let client = client()?;
        let fetch = |url: String| {
            let client = client.clone();
            async move {
                let response = client
                    .get(url)
                    .send()
                    .await
                    .map_err(|error| http_error(&error))?;
                if !response.status().is_success() {
                    return Err(Error::Connect(format!(
                        "the download answered {}",
                        response.status()
                    )));
                }
                response.bytes().await.map_err(|error| http_error(&error))
            }
        };
        let expected = fetch(release.checksum.url.clone()).await?;
        let expected = String::from_utf8_lossy(&expected)
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let package = fetch(release.package.url.clone()).await?;
        let actual = hex(&Sha256::digest(&package));
        if expected.len() != 64 || actual != expected {
            return Err(Error::Config(format!(
                "the downloaded {} does not match its checksum; it was not installed",
                release.package.name
            )));
        }
        fs::create_dir_all(&dir).map_err(io_error)?;
        let path = dir.join(&release.package.name);
        fs::write(&path, &package).map_err(io_error)?;
        Ok(path)
    })
    .await
}

fn io_error(error: io::Error) -> Error {
    Error::Config(format!("the update could not be installed: {error}"))
}

/// Unpack `package` into `into`.
fn unpack(package: &Path, into: &Path) -> Result<()> {
    fs::create_dir_all(into).map_err(io_error)?;
    let name = package.to_string_lossy();
    if name.ends_with(".tar.gz") {
        let file = fs::File::open(package).map_err(io_error)?;
        tar::Archive::new(flate2::read::GzDecoder::new(file))
            .unpack(into)
            .map_err(io_error)?;
        return Ok(());
    }
    let file = fs::File::open(package).map_err(io_error)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| io_error(io::Error::other(e)))?;
    for ix in 0..archive.len() {
        let mut entry = archive
            .by_index(ix)
            .map_err(|e| io_error(io::Error::other(e)))?;
        // Entries that would land outside `into` are skipped.
        let Some(relative) = entry.enclosed_name() else {
            continue;
        };
        let path = into.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&path).map_err(io_error)?;
            continue;
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(io_error)?;
        }
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(io_error)?;
        fs::write(&path, bytes).map_err(io_error)?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).map_err(io_error)?;
        }
    }
    Ok(())
}

/// The `.app` bundle `exe` runs from, if any.
fn bundle_of(exe: &Path) -> Option<PathBuf> {
    exe.ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .map(Path::to_path_buf)
}

/// Find an entry named `name` anywhere under `dir`: a directory when
/// `directory` is set, otherwise a file.
fn find(dir: &Path, name: &str, directory: bool) -> Option<PathBuf> {
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == name) && path.is_dir() == directory {
            return Some(path);
        }
        if path.is_dir()
            && let Some(found) = find(&path, name, directory)
        {
            return Some(found);
        }
    }
    None
}

/// Install `package` over the program at `exe`: the `.app` bundle it runs
/// from on macOS, the executable itself elsewhere. The running program
/// keeps running; the new version starts next time.
///
/// # Errors
///
/// [`Error::Config`] when the package lacks the program or the files cannot
/// be replaced (for example, a system install the user cannot write to).
pub fn install(package: &Path, exe: &Path) -> Result<()> {
    let work = package.with_extension("unpacked");
    let _ = fs::remove_dir_all(&work);
    unpack(package, &work)?;
    let outcome = (|| {
        if let Some(bundle) = bundle_of(exe) {
            let new = find(&work, "Sonde.app", true)
                .ok_or_else(|| Error::Config("the package holds no Sonde.app".into()))?;
            let old = bundle.with_extension("app.old");
            let _ = fs::remove_dir_all(&old);
            fs::rename(&bundle, &old).map_err(io_error)?;
            if let Err(error) = fs::rename(&new, &bundle) {
                let _ = fs::rename(&old, &bundle);
                return Err(io_error(error));
            }
            let _ = fs::remove_dir_all(&old);
            return Ok(());
        }
        let name = if cfg!(windows) { "sonde.exe" } else { "sonde" };
        let new = find(&work, name, false)
            .ok_or_else(|| Error::Config(format!("the package holds no {name}")))?;
        let running = std::env::current_exe()
            .ok()
            .and_then(|p| p.canonicalize().ok());
        if running.is_some() && running == exe.canonicalize().ok() {
            // Replacing a running executable needs care on Windows.
            self_replace::self_replace(&new).map_err(io_error)
        } else {
            let staged = exe.with_extension("new");
            fs::copy(&new, &staged).map_err(io_error)?;
            fs::rename(&staged, exe).map_err(io_error)
        }
    })();
    let _ = fs::remove_dir_all(&work);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, name: &str, prerelease: bool, assets: &[&str]) -> GitHubRelease {
        GitHubRelease {
            tag_name: tag.into(),
            name: Some(name.into()),
            body: None,
            html_url: format!("https://example.test/{tag}"),
            draft: false,
            prerelease,
            assets: assets
                .iter()
                .map(|a| GitHubAsset {
                    name: (*a).into(),
                    browser_download_url: format!("https://example.test/{a}"),
                })
                .collect(),
        }
    }

    fn assets(target: &str) -> Vec<String> {
        let package = package_name(target);
        vec![package.clone(), format!("{package}.sha256")]
    }

    #[test]
    fn pick_should_take_the_newest_release_on_the_channel() {
        let target = "x86_64-unknown-linux-gnu";
        let a = assets(target);
        let a: Vec<&str> = a.iter().map(String::as_str).collect();
        let releases = || {
            vec![
                release("v0.2.0", "Sonde 0.2.0", false, &a),
                release("nightly", "Nightly 0.3.0-nightly.20261121.4", true, &a),
                release("v0.1.5", "Sonde 0.1.5", false, &a),
            ]
        };
        let current = Version::parse("0.1.0").unwrap();

        let stable = pick(releases(), Channel::Stable, &current, target).unwrap();
        assert_eq!(stable.version.to_string(), "0.2.0");
        let nightly = pick(releases(), Channel::Nightly, &current, target).unwrap();
        assert_eq!(nightly.version.to_string(), "0.3.0-nightly.20261121.4");
        assert!(
            pick(
                releases(),
                Channel::Stable,
                &Version::parse("0.2.0").unwrap(),
                target
            )
            .is_none()
        );
    }

    #[test]
    fn pick_should_skip_releases_without_this_platform_s_package() {
        let current = Version::parse("0.1.0").unwrap();
        let releases = vec![release(
            "v0.2.0",
            "Sonde 0.2.0",
            false,
            &["sonde-other.zip"],
        )];

        assert!(
            pick(
                releases,
                Channel::Stable,
                &current,
                "x86_64-unknown-linux-gnu"
            )
            .is_none()
        );
    }

    #[test]
    fn nightly_versions_should_order_before_their_release() {
        let nightly = Version::parse("0.2.0-nightly.20261121.4").unwrap();
        assert!(nightly < Version::parse("0.2.0").unwrap());
        assert!(nightly > Version::parse("0.2.0-nightly.20261120.9").unwrap());
    }

    #[test]
    fn install_should_replace_the_program_from_a_tarball() {
        let dir = std::env::temp_dir().join(format!("sonde-update-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("sonde");
        fs::write(&exe, b"old").unwrap();

        let package = dir.join("sonde-x86_64-unknown-linux-gnu.tar.gz");
        {
            let file = fs::File::create(&package).unwrap();
            let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
                file,
                flate2::Compression::fast(),
            ));
            let mut header = tar::Header::new_gnu();
            header.set_size(3);
            header.set_mode(0o755);
            header.set_cksum();
            tar.append_data(&mut header, "sonde/sonde", &b"new"[..])
                .unwrap();
            tar.into_inner().unwrap().finish().unwrap();
        }

        install(&package, &exe).unwrap();

        assert_eq!(fs::read(&exe).unwrap(), b"new");
        let _ = fs::remove_dir_all(&dir);
    }
}
