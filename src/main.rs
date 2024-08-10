use derive_new::new;
use hex::FromHexError;
use itertools::Itertools;
use log::{debug, error, info, warn};
use once_cell::sync::Lazy;
use sha2::digest::DynDigest;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fmt::Formatter;
use std::fs::File;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::{fmt, ops};
use tokio::io::{stdout, AsyncWriteExt};
use tokio::task::JoinSet;
use tokio::try_join;
use url::Url;
use zip::ZipArchive;

const COSMIC_ARCHIVE_VERSIONS_URL: &str =
    "https://raw.githubusercontent.com/CRModders/CosmicArchive/main/versions.json";

const COSMIC_REACH_URL: &str = "https://finalforeach.itch.io/cosmic-reach";

static CSRF_TOKEN: Lazy<String> = Lazy::new(|| std::env::var("CSRF_TOKEN").unwrap_or_default());

macro_rules! is_jar_platform {
    ($it:expr) => {
        $it.contains(&::itch_io::Platform::Linux) && $it.contains(&::itch_io::Platform::Windows)
    };
}

#[tokio::main]
async fn main() -> Result<(), ()> {
    env_logger::init();

    if CSRF_TOKEN.is_empty() {
        warn!("Environmental variable 'CSRF_TOKEN' is empty");
    }

    let client = itch_io::Client::new();
    let join_set = get_game_jars(&client);
    let archived_hashes = get_version_hashes(&client);

    let (mut join_set, archived_hashes) = try_join!(join_set, archived_hashes)?;

    let mut hasher = {
        use sha2::Digest;
        sha2::Sha256::new()
    };
    let mut paths = Vec::new();

    while let Some(res) = join_set.join_next().await {
        match res {
            Ok(Ok(files)) => {
                for path in files {
                    if is_version_unarchived(&archived_hashes, &mut hasher, &path) {
                        paths.push(path);
                    }
                }
            }
            Ok(Err(())) => {}
            Err(cause) => error!("Unexpected join error: {cause}"),
        };
    }

    // NOTE: most of the time will print none when the latest is archived; and
    //       one when the current latest is not yet archived.
    if paths.is_empty() {
        warn!("NO JAR files found whose hashes are NOT already in the archive");
    } else {
        warn!("Following are downloaded game JAR files whose hashes are not found in the archive:");
        for path in paths {
            println!("{}", path.display());
        }
    }

    Ok(())
}

async fn get_version_hashes(client: &itch_io::Client) -> Result<HashSet<Sha256Hash>, ()> {
    info!("Sending GET request to archived versions data...");
    debug!("GET request {COSMIC_ARCHIVE_VERSIONS_URL}");
    let versions_response = match client.client.get(COSMIC_ARCHIVE_VERSIONS_URL).send().await {
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

    info!("Reading bytes from GET response...");
    let versions_bytes = match versions_response.bytes().await {
        Ok(it) => it,
        Err(cause) => {
            error!("Failed to read bytes from GET response: {cause}");
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
            if let Err(cause) = stdout().write_all(&versions_bytes).await {
                error!("Failed to bump the bytes: {cause}");
            }

            return Err(());
        }
    };

    let hashes = versions.versions.into_iter().map(|it| it.sha256).collect();
    info!("Collected known game jar sha256 hashes");
    for hash in &hashes {
        debug!("        {hash}");
    }

    Ok(hashes)
}

async fn get_game_jars(client: &itch_io::Client) -> Result<JoinSet<Result<Vec<PathBuf>, ()>>, ()> {
    info!("Getting game page data...");
    debug!("Getting game page data for {COSMIC_REACH_URL}...");
    let download_ids = match client.get_game_page(COSMIC_REACH_URL).await {
        Ok(game_page) => {
            let download_ids = game_page.downloads.into_iter().filter_map(|download| {
                let id = download.id?;
                is_jar_platform!(download.platforms).then_some(id)
            });
            info!("Collected likely JAR containing version download ids");
            download_ids.collect_vec()
        }
        Err(cause) => {
            error!("Failed getting game page data: {cause}");
            return Err(());
        }
    };

    let join_set = download_ids
        .into_iter()
        .map(|download_id| {
            let client = client.clone();
            async move { download_game_jars(client, download_id).await }
        })
        .collect();
    Ok(join_set)
}

async fn download_game_jars(client: itch_io::Client, download_id: u64) -> Result<Vec<PathBuf>, ()> {
    info!("[{download_id}] Getting download info");
    let url = match client
        .get_download_info(COSMIC_REACH_URL, download_id, &CSRF_TOKEN)
        .await
    {
        Ok(it) => it.url,
        Err(cause) => {
            error!("[{download_id}] Failed getting download info: {cause}");
            return Err(());
        }
    };

    info!("[{download_id}] Sending GET request to download url...");
    debug!("[{download_id}] GET request {url}");
    let response = match client.client.get(url).send().await {
        Ok(it) => it,
        Err(cause) => {
            error!("[{download_id}] Failed to send GET request to download url: {cause}");
            return Err(());
        }
    };

    if !response.status().is_success() {
        error!("Non-success GET response status: {}", response.status());
        return Err(());
    }

    info!("[{download_id}] Reading bytes from GET response...");
    let bytes = match response.bytes().await {
        Ok(it) => it,
        Err(cause) => {
            error!("[{download_id}] Failed to read bytes from GET response: {cause}");
            error!("[{download_id}] This usually happens with unstable connection from either end");
            return Err(());
        }
    };

    let path = PathBuf::from(download_id.to_string());

    info!("[{download_id}] Creating destination folder for extraction...");
    debug!("[{download_id}] {}", path.display());
    if let Err(cause) = std::fs::create_dir(&path) {
        if cause.kind() == std::io::ErrorKind::AlreadyExists {
            info!("[{download_id}] Destination directory already exists");
        } else {
            error!("[{download_id}] Failed to create destination folder for extraction: {cause}");
            return Err(());
        }
    };

    info!("[{download_id}] Reading bytes as zip archive...");
    let mut archive = match ZipArchive::new(Cursor::new(bytes)) {
        Ok(it) => it,
        Err(cause) => {
            error!("[{download_id}] Failed to read bytes as zip archive: {cause}");
            return Err(());
        }
    };

    let paths = (0..archive.len()).filter_map(|archive_id| {
        error!("[{download_id}] Accessing archived file at index {archive_id}...");
        let mut file = match archive.by_index(archive_id) {
            Ok(it) => it,
            Err(cause) => {
                error!("[{download_id}] Unable to access archived file at index {archive_id}: {cause}");
                return None;
            }
        };

        let relative_path = file.mangled_name();
        if let Some(file_name) = relative_path.file_name().and_then(OsStr::to_str) {
            if file_name.starts_with("Cosmic Reach-")
                && relative_path
                .extension()
                .map_or(false, |ext| ext.eq_ignore_ascii_case("jar"))
            {
                info!("[{download_id}] ({archive_id}) Found game jar file {}", relative_path.display());
                let extracted_path = path.join(&relative_path);

                info!("[{download_id}] ({archive_id}) Creating destination game jar file if absent...");
                let mut extracted = match File::create(&extracted_path) {
                    Ok(it) => it,
                    Err(cause) => {
                        error!("[{download_id}] ({archive_id}) Failed to create destination game jar file: {cause}");
                        return None;
                    }
                };

                info!("[{download_id}] ({archive_id}) Extracting extract game jar file...");
                return if let Err(cause) = std::io::copy(&mut file, &mut extracted) {
                    error!("[{download_id}] ({archive_id}) Failed to extract game jar file: {cause}");
                    None
                } else {
                    Some(extracted_path)
                }
            }
        }

        debug!("[{download_id}] ({archive_id}) Ignored non-game jar file {}", relative_path.display());
        None
    }).collect_vec();

    Ok(paths)
}

fn is_version_unarchived<P: AsRef<Path>>(
    archived_hashes: &HashSet<Sha256Hash>,
    hasher: &mut sha2::Sha256,
    path: P,
) -> bool {
    let path = path.as_ref();

    info!(
        "[{}] Opening file before hash calculation...",
        path.display()
    );
    let mut file = match File::open(path) {
        Ok(it) => it,
        Err(cause) => {
            error!(
                "[{}] Failed to open file for hash calculations: {cause}",
                path.display()
            );
            return false;
        }
    };

    info!("[{}] Calculating sha256 hash...", path.display());
    if let Err(cause) = std::io::copy(&mut file, hasher) {
        error!(
            "[{}] Failed to calculate sha256 hash: {cause}",
            path.display()
        );
        return false;
    }

    info!("[{}] Generating sha256 hash...", path.display());
    let mut hash = Sha256Hash::default();
    if let Err(cause) = hasher.finalize_into_reset(&mut hash) {
        error!(
            "[{}] Failed to generate sha256 hash: {cause}",
            path.display()
        );
        error!("[FATAL] This is likely a logic bug concerning incorrect byte buffer size");
        return false;
    };
    debug!("[{}] {hash}", path.display());

    if archived_hashes.contains(&hash) {
        warn!("[{}] Sha256 already archived, skipping", path.display());
        false
    } else {
        true
    }
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
    pub url: Url,
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
    new,
    Ord,
    PartialEq,
    PartialOrd,
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

impl FromStr for Sha256Hash {
    type Err = FromHexError;

    #[inline]
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut array = [0; 32];
        hex::decode_to_slice(s, &mut array)?;
        Ok(Self::new(array))
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
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
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
