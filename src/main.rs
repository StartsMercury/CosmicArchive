use hex::FromHexError;
use itertools::Itertools;
use log::{error, info, warn};
use once_cell::sync::Lazy;
use sha2::Digest;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{stdout, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{fmt, io, ops, str};

const ARCHIVED_VERSIONS_URL: &str =
    "https://raw.githubusercontent.com/CRModders/CosmicArchive/main/versions.json";

const ITCH_GAME_URL: &str = "https://finalforeach.itch.io/cosmic-reach";

const TARGET_DOWNLOAD_TITLE: &str = "cosmic-reach-jar.zip";

static CSRF_TOKEN: Lazy<String> = Lazy::new(|| std::env::var("CSRF_TOKEN").unwrap_or_default());

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(()) => ExitCode::FAILURE,
    }
}

async fn run() -> Result<(), ()> {
    env_logger::init();

    if CSRF_TOKEN.is_empty() {
        warn!("Environmental variable 'CSRF_TOKEN' is empty");
    }

    let client = itch_io::Client::new();
    let download_id = get_jar_download_id(&client);
    let archived_hashes = get_version_hashes(&client);

    let download_id = download_id.await?;
    // TODO: only download and check hash if git branch does not yet exist
    let path = download_with_id(&client, download_id);

    let (path, archived_hashes) = tokio::try_join!(path, archived_hashes)?;

    if is_version_unarchived(&archived_hashes, &path) {
        warn!("Printing to STDOUT the JAR path that is NOT yet archived.");
        println!("{}", path.display());
        Ok(())
    } else {
        error!("'{}' is already archived", path.display());
        Err(())
    }
}

async fn get_version_hashes(client: &itch_io::Client) -> Result<HashSet<Sha256Hash>, ()> {
    warn!("Sending GET request to archived versions data ({ARCHIVED_VERSIONS_URL})...");
    let versions_response = match client.client.get(ARCHIVED_VERSIONS_URL).send().await {
        Ok(it) => it,
        Err(cause) => {
            error!("Failed to send GET request for archived versions data: {cause}");
            return Err(());
        }
    };

    if !versions_response.status().is_success() {
        error!(
            "Non-success GET response status: {}",
            versions_response.status()
        );
        return Err(());
    }

    info!("Reading bytes from GET response to archived versions data...");
    let versions_bytes = match versions_response.bytes().await {
        Ok(it) => it,
        Err(cause) => {
            error!("Failed to read bytes from GET response to archived versions data: {cause}");
            error!("This usually happens with unstable connection from either end");
            return Err(());
        }
    };

    info!("Deserialize received bytes as valid JSON...");
    let versions: Versions = match serde_json::from_slice(&versions_bytes) {
        Ok(it) => it,
        Err(cause) => {
            error!("Failed to deserialize received bytes as valid JSON: {cause})");

            error!("Dumping bytes to STDOUT...");
            if let Err(cause) = stdout().write_all(&versions_bytes) {
                error!("Failed to bump the bytes: {cause}");
            }

            return Err(());
        }
    };

    let hashes = versions.versions.into_iter().map(|it| it.sha256).collect();
    info!("Collected known game jar sha256 hashes");
    for hash in &hashes {
        info!("        {hash}");
    }

    Ok(hashes)
}

async fn get_jar_download_id(client: &itch_io::Client) -> Result<u64, ()> {
    warn!("Getting game page data of {ITCH_GAME_URL}...");
    let game_page = match client.get_game_page(ITCH_GAME_URL).await {
        Ok(it) => it,
        Err(cause) => {
            error!("Failed getting game page data: {cause}");
            return Err(());
        }
    };

    info!("Following are available downloads:");
    for download in &game_page.downloads {
        info!("        {}", download.title);
    }

    let jar_download = match game_page
        .downloads
        .into_iter()
        .filter(|download| matches!(download.title.as_str(), TARGET_DOWNLOAD_TITLE))
        .at_most_one()
    {
        Ok(None) => {
            error!("NO download options matched `{TARGET_DOWNLOAD_TITLE}`");
            return Err(());
        }
        Ok(Some(it)) => it,
        Err(downloads) => {
            error!("There is more than one '{TARGET_DOWNLOAD_TITLE}':");
            for download in downloads {
                error!("        {download:?}");
            }
            return Err(());
        }
    };

    jar_download.id.map_or_else(
        || {
            error!("Jar download option has NO id");
            Err(())
        },
        Ok,
    )
}

async fn download_with_id(client: &itch_io::Client, download_id: u64) -> Result<PathBuf, ()> {
    info!("Getting download info");
    let url = match client
        .get_download_info(ITCH_GAME_URL, download_id, &CSRF_TOKEN)
        .await
    {
        Ok(it) => it.url,
        Err(cause) => {
            error!("Failed getting download info: {cause}");
            return Err(());
        }
    };

    warn!("Sending GET request to download url ({ARCHIVED_VERSIONS_URL})...");
    let response = match client.client.get(url).send().await {
        Ok(it) => it,
        Err(cause) => {
            error!("Failed to send GET request to download url: {cause}");
            return Err(());
        }
    };

    if !response.status().is_success() {
        error!("Non-success GET response status: {}", response.status());
        return Err(());
    }

    info!("Reading bytes from GET response to download url...");
    let bytes = match response.bytes().await {
        Ok(it) => it,
        Err(cause) => {
            error!("Failed to read bytes from GET response to download url: {cause}");
            error!("This usually happens with unstable connection from either end");
            return Err(());
        }
    };

    info!("Reading bytes as zip archive...");
    let mut archive = match zip::ZipArchive::new(io::Cursor::new(bytes)) {
        Ok(it) => it,
        Err(cause) => {
            error!("Failed to read bytes as zip archive: {cause}");
            return Err(());
        }
    };

    info!("Archive contains the following files:");
    for file_name in archive.file_names() {
        info!("    {file_name}");
    }

    let file_name = match archive
        .file_names()
        .filter(|file_name| {
            file_name.starts_with("Cosmic Reach-")
                || Path::extension(file_name.as_ref())
                    .map_or(false, |it| it.eq_ignore_ascii_case("jar"))
        })
        .at_most_one()
    {
        Ok(None) => {
            error!("Archive did NOT contain the game JAR");
            return Err(());
        }
        Ok(Some(it)) => {
            info!("Found game JAR: {it}");
            String::from(it)
        }
        Err(file_names) => {
            error!("Archived contained MULTIPLE game JARs:");
            for file_name in file_names {
                error!("        {file_name}");
            }
            return Err(());
        }
    };

    info!("Reading archived game jar...");
    let mut file = match archive.by_name(&file_name) {
        Ok(it) => it,
        Err(cause) => {
            error!(
                "Previously accessed archived file is no longer accessible '{file_name}': {cause}"
            );
            return Err(());
        }
    };

    // NOTE: might as well stay in the safety of ZipFile::mangled_name
    let relative_path = file.mangled_name();

    info!("Creating destination game jar file if absent...");
    let mut extracted = match File::create(&relative_path) {
        Ok(it) => it,
        Err(cause) => {
            error!("Failed to create destination game jar file: {cause}");
            return Err(());
        }
    };

    info!("Extracting extract game jar file...");
    if let Err(cause) = std::io::copy(&mut file, &mut extracted) {
        error!("Failed copy archived game jar contents to destination file: {cause}");
        Err(())
    } else {
        Ok(relative_path)
    }
}

fn is_version_unarchived<P: AsRef<Path>>(archived_hashes: &HashSet<Sha256Hash>, path: P) -> bool {
    let path = path.as_ref();

    let mut hasher = sha2::Sha256::new();

    info!("Opening file before hash calculation...");
    let mut file = match File::open(path) {
        Ok(it) => it,
        Err(cause) => {
            error!("Failed to open file for hash calculations: {cause}");
            return false;
        }
    };

    info!("Calculating sha256 hash...");
    if let Err(cause) = std::io::copy(&mut file, &mut hasher) {
        error!("Failed to calculate sha256 hash: {cause}");
        return false;
    }

    info!("Generating sha256 hash...");
    let mut hash = Sha256Hash::default();
    if let Err(cause) = sha2::digest::DynDigest::finalize_into(hasher, &mut hash) {
        error!("Failed to generate sha256 hash: {cause}");
        error!("[FATAL] This is likely a logic bug concerning incorrect byte buffer size");
        return false;
    };
    info!("Game JAR hash: {hash}");

    !archived_hashes.contains(&hash)
}

#[derive(Debug, Clone, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
struct Versions {
    pub latest: HashMap<String, String>,
    pub versions: Vec<Version>,
}

#[derive(Debug, Clone, Eq, PartialEq, serde::Deserialize, serde::Serialize)]
struct Version {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(rename = "releaseTime")]
    pub release_time: u64,
    pub url: url::Url,
    pub sha256: Sha256Hash,
    pub size: u64,
}

#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    Eq,
    Hash,
    Ord,
    PartialEq,
    PartialOrd,
    derive_new::new,
    serde::Deserialize,
    serde::Serialize,
)]
#[repr(transparent)]
#[serde(try_from = "String", into = "String")]
struct Sha256Hash {
    inner: [u8; 32],
}

impl AsRef<[u8]> for Sha256Hash {
    fn as_ref(&self) -> &[u8] {
        &self.inner
    }
}

impl AsRef<[u8; 32]> for Sha256Hash {
    fn as_ref(&self) -> &[u8; 32] {
        &self.inner
    }
}

impl AsMut<[u8; 32]> for Sha256Hash {
    fn as_mut(&mut self) -> &mut [u8; 32] {
        &mut self.inner
    }
}

impl AsMut<[u8]> for Sha256Hash {
    fn as_mut(&mut self) -> &mut [u8] {
        &mut self.inner
    }
}

impl From<[u8; 32]> for Sha256Hash {
    #[inline]
    fn from(value: [u8; 32]) -> Self {
        Self::new(value)
    }
}

impl From<Sha256Hash> for [u8; 32] {
    #[inline]
    fn from(value: Sha256Hash) -> Self {
        value.inner
    }
}

impl From<Sha256Hash> for String {
    #[inline]
    fn from(hash: Sha256Hash) -> Self {
        hex::encode(hash)
    }
}

impl TryFrom<String> for Sha256Hash {
    type Error = FromHexError;

    #[inline]
    fn try_from(s: String) -> Result<Self, Self::Error> {
        let mut array = [0; 32];
        hex::decode_to_slice(s, &mut array)?;
        Ok(Self::new(array))
    }
}

impl fmt::Display for Sha256Hash {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self))
    }
}

impl ops::Deref for Sha256Hash {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl ops::DerefMut for Sha256Hash {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl str::FromStr for Sha256Hash {
    type Err = FromHexError;

    #[inline]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut array = [0; 32];
        hex::decode_to_slice(s, &mut array)?;
        Ok(Self::new(array))
    }
}
