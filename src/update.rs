//! Updates from the project's GitHub releases. Each release carries a minisign signature of its
//! installer, whose trusted comment names the installer and the version. The latest release's
//! signature announces the version, and an installer is run only when that signature verifies
//! against the release key built into the app.

// The build script reads the changelog with this module, which the crate holds for its tests.
#[cfg(test)]
mod changelog;
#[cfg(windows)]
pub(crate) mod client;
pub(crate) mod notes;

use minisign_verify::{PublicKey, Signature};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};
use std::{
    cmp::Ordering,
    fmt,
    path::{Path, PathBuf},
    str::FromStr,
};
use thiserror::Error;

const REPOSITORY: &str = env!("CARGO_PKG_REPOSITORY");
pub(crate) const INSTALLER: &str = "hindsight-setup-x64.exe";
const SIGNATURE_EXTENSION: &str = "minisig";
/// The public half of the key the release workflow signs installers with.
const RELEASE_KEY: &str = "RWS8QHCApYhZBeuje7KwHPxzYQvonUdt6eOwQGpVK2M4ZB8xsX+hCSrA";
/// The installer leaves its uninstaller beside the app; the portable zip has none.
const UNINSTALLER: &str = "uninstall.exe";
/// The environment variable that, set to a version, makes update checks take this copy for that
/// version, so updating can be tried against the latest release while it is installed.
#[cfg(windows)]
pub(crate) const VERSION_SWITCH: &str = "HINDSIGHT_UPDATE_FROM";
/// The installer's flags: silent, then waiting for this copy to exit and starting the new one.
/// `/D=` sets the folder, and NSIS requires it last and unquoted.
const INSTALLER_FLAGS: &str = "/S /UPDATE";

/// A semantic version. Build metadata is dropped, since it does not order versions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    pre_release: Vec<Identifier>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Identifier {
    Numeric(u64),
    Text(String),
}

impl Version {
    /// The version of this build.
    pub(crate) fn running() -> Self {
        env!("CARGO_PKG_VERSION")
            .parse()
            .expect("Cargo requires the package version to be a semantic version")
    }

    /// The version update checks compare the latest release with.
    #[cfg(windows)]
    pub(crate) fn for_updates() -> Self {
        Self::chosen(std::env::var(VERSION_SWITCH).ok().as_deref())
    }

    /// The version `switch` names, or this build's when it names none.
    fn chosen(switch: Option<&str>) -> Self {
        switch
            .and_then(|version| version.trim().parse().ok())
            .unwrap_or_else(Self::running)
    }
}

impl FromStr for Version {
    type Err = VersionError;

    fn from_str(text: &str) -> Result<Self, VersionError> {
        let invalid = || VersionError(text.to_owned());
        let version = text.split_once('+').map_or(text, |(version, _)| version);
        let (core, pre_release) = match version.split_once('-') {
            Some((core, pre_release)) => (core, Some(pre_release)),
            None => (version, None),
        };
        let mut numbers = core.split('.').map(number);
        let (Some(Some(major)), Some(Some(minor)), Some(Some(patch)), None) = (
            numbers.next(),
            numbers.next(),
            numbers.next(),
            numbers.next(),
        ) else {
            return Err(invalid());
        };
        let pre_release = match pre_release {
            Some(identifiers) => identifiers
                .split('.')
                .map(identifier)
                .collect::<Option<_>>()
                .ok_or_else(invalid)?,
            None => Vec::new(),
        };
        Ok(Self {
            major,
            minor,
            patch,
            pre_release,
        })
    }
}

fn number(text: &str) -> Option<u64> {
    let digits = !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    let leading_zero = text.len() > 1 && text.starts_with('0');
    (digits && !leading_zero)
        .then(|| text.parse().ok())
        .flatten()
}

fn identifier(text: &str) -> Option<Identifier> {
    if let Some(value) = number(text) {
        return Some(Identifier::Numeric(value));
    }
    let valid = !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        && !text.bytes().all(|byte| byte.is_ascii_digit());
    valid.then(|| Identifier::Text(text.to_owned()))
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| {
                // A pre-release comes before the release it leads to.
                match (self.pre_release.is_empty(), other.pre_release.is_empty()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => Ordering::Greater,
                    (false, true) => Ordering::Less,
                    (false, false) => self.pre_release.cmp(&other.pre_release),
                }
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)?;
        for (index, identifier) in self.pre_release.iter().enumerate() {
            formatter.write_str(if index == 0 { "-" } else { "." })?;
            match identifier {
                Identifier::Numeric(value) => write!(formatter, "{value}")?,
                Identifier::Text(text) => formatter.write_str(text)?,
            }
        }
        Ok(())
    }
}

impl Serialize for Version {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(de::Error::custom)
    }
}

#[derive(Debug, Error)]
#[error("{0:?} is not a version")]
pub(crate) struct VersionError(String);

/// A release as its installer's signature announces it. The announcement is trusted only once
/// [`Release::verify`] has checked the installer against it.
#[derive(Clone)]
pub(crate) struct Release {
    version: Version,
    /// Boxed, since it is several times the size of everything else that carries a release.
    signature: Box<Signature>,
}

impl Release {
    /// Reads the release that an installer signature, the text of a `.minisig` file, names.
    pub(crate) fn announced(signature: &str) -> Result<Self, ReleaseError> {
        let signature = Signature::decode(signature).map_err(ReleaseError::Unreadable)?;
        let (file, version) = signature
            .trusted_comment()
            .split_once(' ')
            .ok_or_else(|| ReleaseError::Unnamed(signature.trusted_comment().to_owned()))?;
        if file != INSTALLER {
            return Err(ReleaseError::OtherFile(file.to_owned()));
        }
        let version = version.parse()?;
        Ok(Self {
            version,
            signature: Box::new(signature),
        })
    }

    pub(crate) fn version(&self) -> &Version {
        &self.version
    }

    pub(crate) fn installer_url(&self) -> String {
        format!("{}/{INSTALLER}", self.download_root())
    }

    pub(crate) fn notes_url(&self) -> String {
        format!("{}/releases/tag/v{}", repository(), self.version)
    }

    /// Checks that `installer` is the one this release's signature signs. The trusted comment,
    /// and so the version, is covered by the same signature.
    pub(crate) fn verify(&self, installer: &[u8]) -> Result<(), ForgedInstaller> {
        self.verify_with(&release_key(), installer)
    }

    fn verify_with(&self, key: &PublicKey, installer: &[u8]) -> Result<(), ForgedInstaller> {
        key.verify(installer, &self.signature, false)
            .map_err(ForgedInstaller)
    }

    fn download_root(&self) -> String {
        format!("{}/releases/download/v{}", repository(), self.version)
    }
}

impl fmt::Debug for Release {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Release")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Error)]
pub(crate) enum ReleaseError {
    #[error("the release signature cannot be read")]
    Unreadable(#[source] minisign_verify::Error),
    #[error("the release signature names no file and version: {0:?}")]
    Unnamed(String),
    #[error("the release signature is for {0}, not the installer")]
    OtherFile(String),
    #[error("the release signature names no valid version")]
    Version(#[from] VersionError),
}

#[derive(Debug, Error)]
#[error("the downloaded installer is not the signed release")]
pub(crate) struct ForgedInstaller(#[source] minisign_verify::Error);

/// Where the latest release's installer signature is. GitHub redirects this to the newest
/// release that is neither a draft nor a pre-release.
pub(crate) fn latest_signature_url() -> String {
    format!(
        "{}/releases/latest/download/{INSTALLER}.{SIGNATURE_EXTENSION}",
        repository()
    )
}

/// Where every release's notes are.
pub(crate) fn releases_url() -> String {
    format!("{}/releases", repository())
}

/// `CARGO_PKG_REPOSITORY` without a trailing slash or `.git`, either of which it may include.
fn repository() -> &'static str {
    let root = REPOSITORY.trim_end_matches('/');
    root.strip_suffix(".git").unwrap_or(root)
}

fn release_key() -> PublicKey {
    PublicKey::from_base64(RELEASE_KEY).expect("the release key is a valid minisign public key")
}

/// What a check found.
#[derive(Debug)]
pub(crate) enum Check {
    Current,
    Newer(Release),
}

impl Check {
    pub(crate) fn of(running: &Version, latest: Release) -> Self {
        if latest.version > *running {
            Self::Newer(latest)
        } else {
            Self::Current
        }
    }
}

/// How this copy of hindsight was put on the machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Installation {
    /// By the installer, into this folder, which an update installs over.
    Installed(PathBuf),
    /// From the zip, which the installer does not update.
    Portable,
}

impl Installation {
    pub(crate) fn of(executable: &Path) -> Self {
        match executable.parent() {
            Some(folder) if folder.join(UNINSTALLER).is_file() => {
                Self::Installed(folder.to_path_buf())
            }
            _ => Self::Portable,
        }
    }
}

/// The installer's command line for updating the copy in `folder`.
pub(crate) fn installer_parameters(folder: &str) -> String {
    format!("{INSTALLER_FLAGS} /D={folder}")
}

/// Where updating stands, as the About tab shows it.
#[derive(Clone, Debug)]
pub(crate) enum State {
    Unchecked,
    Checking,
    Current,
    Available(Release),
    Downloading(Release),
    /// The check failed, with the reason.
    CheckFailed(String),
    /// Downloading or starting the installer failed, with the reason.
    UpdateFailed(
        Release,
        #[cfg_attr(
            all(not(windows), test),
            expect(dead_code, reason = "only the Windows About tab shows it")
        )]
        String,
    ),
}

impl State {
    /// Moves to `Checking` and reports `true`, unless a check or a download is under way.
    pub(crate) fn begin_check(&mut self) -> bool {
        if matches!(self, Self::Checking | Self::Downloading(_)) {
            return false;
        }
        *self = Self::Checking;
        true
    }

    pub(crate) fn checked(&mut self, found: Result<Check, String>) {
        *self = match found {
            Ok(Check::Current) => Self::Current,
            Ok(Check::Newer(release)) => Self::Available(release),
            Err(reason) => Self::CheckFailed(reason),
        };
    }

    /// The release to download, after moving to `Downloading`, when one is on offer.
    pub(crate) fn begin_download(&mut self) -> Option<Release> {
        let (Self::Available(release) | Self::UpdateFailed(release, _)) = self else {
            return None;
        };
        let release = release.clone();
        *self = Self::Downloading(release.clone());
        Some(release)
    }

    /// The release on offer, if there is one.
    pub(crate) fn release(&self) -> Option<&Release> {
        match self {
            Self::Available(release)
            | Self::Downloading(release)
            | Self::UpdateFailed(release, _) => Some(release),
            Self::Unchecked | Self::Checking | Self::Current | Self::CheckFailed(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_KEY: &str = "RWRFTc+RanIuNMQkn3zNYSCgJLxeeBJ8It9dU1OLPOEotCvA+m37gK7I";
    const SIGNED: &[u8] = b"not really an installer";
    const INSTALLER_SIGNATURE: &str = "untrusted comment: signature from minisign secret key
RURFTc+RanIuNEe0BLOuWFKm0grJ+5bwtyK7DXGxWoMEbmP+dtW1bv4iO9738EXPzs2iWZkcA1gs4lxjbeZ8JjBxrwOrYWLoRgY=
trusted comment: hindsight-setup-x64.exe 0.2.1
ECsGrypHQc7rPjElOrtcewavYnZiYPzX5WKTumkDp8Dhp3x8q9xXJ/k4D63Yep4l4a7bNPISJaKPkwUvbjeyCA==
";
    const ZIP_SIGNATURE: &str = "untrusted comment: signature from minisign secret key
RURFTc+RanIuNEe0BLOuWFKm0grJ+5bwtyK7DXGxWoMEbmP+dtW1bv4iO9738EXPzs2iWZkcA1gs4lxjbeZ8JjBxrwOrYWLoRgY=
trusted comment: hindsight-windows-x64.zip 0.2.1
4KwOGS/h02wORAgRFPay1s9Ul9eXPRB2fJovS/njsqKjhJk/ODx6wZxqhJlVhx3JzKdQfORvGJD2WZ2KJeKiCA==
";

    fn version(text: &str) -> Version {
        text.parse().unwrap()
    }

    fn test_key() -> PublicKey {
        PublicKey::from_base64(TEST_KEY).unwrap()
    }

    #[test]
    fn versions_order_as_semantic_versioning_says() {
        let ordered = [
            "0.1.0",
            "0.2.0-alpha",
            "0.2.0-alpha.1",
            "0.2.0-alpha.beta",
            "0.2.0-beta",
            "0.2.0-beta.2",
            "0.2.0-beta.11",
            "0.2.0-rc.1",
            "0.2.0",
            "0.2.1",
            "0.10.0",
            "1.0.0",
        ]
        .map(version);
        for pair in ordered.windows(2) {
            assert!(pair[0] < pair[1], "{} < {}", pair[0], pair[1]);
        }
        assert_eq!(version("1.2.3+build.5"), version("1.2.3"));
    }

    #[test]
    fn versions_print_as_they_were_written() {
        for text in ["0.2.0", "0.2.0-beta.1", "10.0.3-rc-2.x"] {
            assert_eq!(version(text).to_string(), text);
        }
    }

    #[test]
    fn malformed_versions_are_refused() {
        for text in [
            "",
            "1",
            "1.2",
            "1.2.3.4",
            "v1.2.3",
            "01.2.3",
            "1.2.3-",
            "1.2.3-a..b",
            "1.2.3-é",
            "1.-2.3",
        ] {
            assert!(text.parse::<Version>().is_err(), "{text:?}");
        }
    }

    #[test]
    fn the_running_version_is_the_package_version() {
        assert_eq!(Version::running().to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn versions_are_stored_as_text() {
        let stored = serde_json::to_string(&version("0.2.1-beta.1")).unwrap();
        assert_eq!(stored, r#""0.2.1-beta.1""#);
        assert_eq!(
            serde_json::from_str::<Version>(&stored).unwrap(),
            version("0.2.1-beta.1")
        );
        assert!(serde_json::from_str::<Version>(r#""0.2""#).is_err());
        assert!(serde_json::from_str::<Version>("21").is_err());
    }

    #[test]
    fn the_switch_names_the_version_to_update_from() {
        assert_eq!(Version::chosen(Some("0.1.0")), version("0.1.0"));
        assert_eq!(Version::chosen(Some(" 0.1.0\r\n")), version("0.1.0"));
        assert_eq!(Version::chosen(None), Version::running());
        assert_eq!(Version::chosen(Some("old")), Version::running());
    }

    #[test]
    fn the_release_key_decodes() {
        let _ = release_key();
    }

    #[test]
    fn a_signature_announces_its_release_and_where_to_get_it() {
        let release = Release::announced(INSTALLER_SIGNATURE).unwrap();
        assert_eq!(*release.version(), version("0.2.1"));
        assert_eq!(
            release.installer_url(),
            "https://github.com/malinoskj2/hindsight/releases/download/v0.2.1/hindsight-setup-x64.exe"
        );
        assert_eq!(
            release.notes_url(),
            "https://github.com/malinoskj2/hindsight/releases/tag/v0.2.1"
        );
        assert_eq!(
            releases_url(),
            "https://github.com/malinoskj2/hindsight/releases"
        );
        assert_eq!(
            latest_signature_url(),
            "https://github.com/malinoskj2/hindsight/releases/latest/download/hindsight-setup-x64.exe.minisig"
        );
    }

    #[test]
    fn only_the_signed_installer_verifies() {
        let release = Release::announced(INSTALLER_SIGNATURE).unwrap();
        assert!(release.verify_with(&test_key(), SIGNED).is_ok());
        assert!(
            release
                .verify_with(&test_key(), b"a different file")
                .is_err()
        );
        assert!(
            release.verify(SIGNED).is_err(),
            "the test key is not the release key"
        );
    }

    #[test]
    fn a_signature_for_another_file_or_version_is_refused() {
        assert!(matches!(
            Release::announced(ZIP_SIGNATURE),
            Err(ReleaseError::OtherFile(file)) if file == "hindsight-windows-x64.zip"
        ));

        let downgraded = INSTALLER_SIGNATURE.replace("0.2.1", "0.1.0");
        let release = Release::announced(&downgraded).unwrap();
        assert!(
            release.verify_with(&test_key(), SIGNED).is_err(),
            "the trusted comment is signed too"
        );

        let unnamed = INSTALLER_SIGNATURE.replace("hindsight-setup-x64.exe 0.2.1", "nothing");
        assert!(matches!(
            Release::announced(&unnamed),
            Err(ReleaseError::Unnamed(_))
        ));
        assert!(matches!(
            Release::announced("not a signature"),
            Err(ReleaseError::Unreadable(_))
        ));
    }

    #[test]
    fn only_a_newer_release_is_offered() {
        let latest = || Release::announced(INSTALLER_SIGNATURE).unwrap();
        assert!(matches!(
            Check::of(&version("0.2.0"), latest()),
            Check::Newer(_)
        ));
        assert!(matches!(
            Check::of(&version("0.2.1-beta.1"), latest()),
            Check::Newer(_)
        ));
        assert!(matches!(
            Check::of(&version("0.2.1"), latest()),
            Check::Current
        ));
        assert!(matches!(
            Check::of(&version("0.3.0"), latest()),
            Check::Current
        ));
    }

    #[test]
    fn only_a_copy_beside_its_uninstaller_counts_as_installed() {
        let folder = tempfile::tempdir().unwrap();
        let executable = folder.path().join("hindsight.exe");
        assert_eq!(Installation::of(&executable), Installation::Portable);

        std::fs::write(folder.path().join(UNINSTALLER), b"").unwrap();
        assert_eq!(
            Installation::of(&executable),
            Installation::Installed(folder.path().to_path_buf())
        );
    }

    #[test]
    fn the_installer_is_told_the_folder_last_and_unquoted() {
        assert_eq!(
            installer_parameters(r"C:\Program Files\hindsight"),
            r"/S /UPDATE /D=C:\Program Files\hindsight"
        );
    }

    #[test]
    fn a_check_or_download_under_way_is_not_started_again() {
        let release = || Release::announced(INSTALLER_SIGNATURE).unwrap();
        let mut state = State::Unchecked;
        assert!(state.begin_download().is_none());
        assert!(state.begin_check());
        assert!(!state.begin_check());

        state.checked(Ok(Check::Newer(release())));
        assert!(matches!(state, State::Available(_)));
        assert!(state.begin_download().is_some());
        assert!(!state.begin_check());
        assert!(state.begin_download().is_none());

        state = State::UpdateFailed(release(), "offline".to_owned());
        assert!(state.begin_download().is_some());
        assert!(matches!(state, State::Downloading(_)));
    }

    #[test]
    fn a_failed_check_keeps_its_reason() {
        let mut state = State::Checking;
        state.checked(Err("offline".to_owned()));
        assert!(matches!(&state, State::CheckFailed(reason) if reason == "offline"));
        assert!(state.release().is_none());
        assert!(state.begin_check());
    }
}
