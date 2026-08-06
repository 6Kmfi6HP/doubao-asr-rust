use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::{Client, Response};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use url::Url;
use uuid::Uuid;

use crate::error::{Error, Result};

const REGISTER_URL: &str = "https://log.snssdk.com/service/2/device_register/";
const SETTINGS_URL: &str = "https://is.snssdk.com/service/settings/v3/";
const MAX_RESPONSE_BYTES: usize = 1 << 20;
pub(crate) const DEVICE_USER_AGENT: &str = "com.bytedance.android.doubaoime/100311008 (Linux; U; Android 9; zh_CN; Redmi Note 7; Build/PKQ1.181121.001; Cronet/TTNetVersion:94cf429a 2025-11-17 QuicVersion:1f89f732 2025-05-08)";

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Credentials {
    pub(crate) device_id: String,
    pub(crate) install_id: String,
    pub(crate) cdid: String,
    pub(crate) openudid: String,
    pub(crate) clientudid: String,
    pub(crate) token: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field("device_id", &self.device_id)
            .field("install_id", &self.install_id)
            .field("cdid", &self.cdid)
            .field("openudid", &self.openudid)
            .field("clientudid", &self.clientudid)
            .field("token", &"[REDACTED]")
            .finish()
    }
}

impl Credentials {
    fn new() -> Self {
        Self {
            device_id: String::new(),
            install_id: String::new(),
            cdid: Uuid::new_v4().to_string(),
            openudid: hex(&rand::random::<[u8; 8]>()),
            clientudid: Uuid::new_v4().to_string(),
            token: String::new(),
        }
    }

    fn complete(&self) -> bool {
        !self.device_id.is_empty() && !self.install_id.is_empty() && !self.token.is_empty()
    }

    #[cfg(test)]
    pub(crate) fn for_test(device_id: &str, install_id: &str, token: &str) -> Self {
        Self {
            device_id: device_id.into(),
            install_id: install_id.into(),
            token: token.into(),
            cdid: "00000000-0000-4000-8000-000000000000".into(),
            openudid: "0000000000000000".into(),
            clientudid: "00000000-0000-4000-8000-000000000000".into(),
        }
    }

    async fn register_device(&mut self, client: &Client) -> Result<()> {
        let header = json!({
            "device_id": 0, "install_id": 0, "aid": 401734,
            "app_name": "oime", "version_code": 100311008,
            "version_name": "1.3.11", "manifest_version_code": 100311008,
            "update_version_code": 100311008, "channel": "official",
            "package": "com.bytedance.android.doubaoime",
            "device_platform": "android", "os": "android", "os_api": "28",
            "os_version": "9", "device_type": "Redmi Note 7",
            "device_brand": "xiaomi", "device_model": "Redmi Note 7",
            "resolution": "1080*2340", "dpi": "440", "language": "zh",
            "timezone": 8, "access": "wifi", "rom": "PKQ1.181121.001",
            "rom_version": "PKQ1.181121.001", "openudid": self.openudid,
            "clientudid": self.clientudid, "cdid": self.cdid, "region": "CN",
            "tz_name": "Asia/Shanghai", "tz_offset": 28800,
            "sim_region": "cn", "carrier_region": "cn", "cpu_abi": "arm64-v8a",
            "build_serial": "unknown", "not_request_sender": 0,
            "sig_hash": "", "google_aid": "", "mc": "", "serial_number": ""
        });
        let body = json!({
            "magic_tag": "ss_app_log", "header": header, "_gen_time": unix_millis()
        });
        let response = client
            .post(device_url(REGISTER_URL, self, false)?)
            .header(reqwest::header::USER_AGENT, DEVICE_USER_AGENT)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
            .await
            .map_err(|_| Error::msg("Doubao device registration request failed"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::msg(format!(
                "Doubao device registration failed with HTTP {}",
                status.as_u16()
            )));
        }
        #[derive(Deserialize)]
        struct Registration {
            device_id: i64,
            install_id: i64,
        }
        let result: Registration = decode_limited(
            response,
            "Doubao device registration returned an invalid response",
        )
        .await?;
        if result.device_id == 0 || result.install_id == 0 {
            return Err(Error::msg(
                "Doubao device registration returned an invalid response",
            ));
        }
        self.device_id = result.device_id.to_string();
        self.install_id = result.install_id.to_string();
        Ok(())
    }

    async fn fetch_token(&mut self, client: &Client) -> Result<()> {
        let body = "body=null";
        let stub = format!("{:X}", md5::compute(body));
        let response = client
            .post(device_url(SETTINGS_URL, self, true)?)
            .header(reqwest::header::USER_AGENT, DEVICE_USER_AGENT)
            .header("x-ss-stub", stub)
            .header(
                reqwest::header::CONTENT_TYPE,
                "application/x-www-form-urlencoded",
            )
            .body(body)
            .send()
            .await
            .map_err(|_| Error::msg("Doubao ASR token request failed"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::msg(format!(
                "Doubao ASR token request failed with HTTP {}",
                status.as_u16()
            )));
        }
        let result: Value =
            decode_limited(response, "Doubao ASR token response was invalid").await?;
        let token = result
            .pointer("/data/settings/asr_config/app_key")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Error::msg("Doubao ASR token response was invalid"))?;
        self.token = token.to_owned();
        Ok(())
    }
}

pub(crate) fn default_path() -> Result<PathBuf> {
    let directory = dirs::config_dir().ok_or_else(|| {
        Error::msg("find user config directory: no user config directory is available")
    })?;
    Ok(directory
        .join("doubao-asr-skill")
        .join("asr_credentials.json"))
}

pub(crate) async fn load(path: &Path) -> Result<Option<Credentials>> {
    let data = match tokio::fs::read(path).await {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(Error::msg(format!("read ASR credentials: {error}"))),
    };
    let credentials: Credentials = serde_json::from_slice(&data).map_err(|_| {
        Error::msg("stored ASR credentials are malformed; move the credential file aside and retry")
    })?;
    if !credentials.complete() {
        return Err(Error::msg(
            "stored ASR credentials are incomplete; move the credential file aside and retry",
        ));
    }
    Ok(Some(credentials))
}

pub(crate) async fn save(path: &Path, credentials: &Credentials) -> Result<()> {
    if !credentials.complete() {
        return Err(Error::msg("refusing to save incomplete ASR credentials"));
    }
    let directory = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let directory_existed = tokio::fs::metadata(directory).await.is_ok();
    tokio::fs::create_dir_all(directory)
        .await
        .map_err(|error| Error::msg(format!("create credential directory: {error}")))?;
    // Match MkdirAll(0700) without tightening permissions on a caller-owned
    // directory that was already present (for example, a test's temp dir).
    #[cfg(unix)]
    if !directory_existed {
        tokio::fs::set_permissions(
            directory,
            std::os::unix::fs::PermissionsExt::from_mode(0o700),
        )
        .await
        .map_err(|error| Error::msg(format!("protect credential directory: {error}")))?;
    }

    let data =
        serde_json::to_vec_pretty(credentials).map_err(|_| Error::msg("encode ASR credentials"))?;
    let temporary = directory.join(format!(".asr_credentials.{}.tmp", Uuid::new_v4()));
    let result = write_and_replace(&temporary, path, &data).await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

async fn write_and_replace(temporary: &Path, destination: &Path, data: &[u8]) -> Result<()> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options
        .open(temporary)
        .await
        .map_err(|error| Error::msg(format!("create temporary credential file: {error}")))?;
    file.write_all(data)
        .await
        .map_err(|error| Error::msg(format!("write ASR credentials: {error}")))?;
    file.flush()
        .await
        .map_err(|error| Error::msg(format!("write ASR credentials: {error}")))?;
    file.sync_all()
        .await
        .map_err(|error| Error::msg(format!("write ASR credentials: {error}")))?;
    #[cfg(unix)]
    file.set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .await
        .map_err(|error| Error::msg(format!("protect ASR credentials: {error}")))?;
    drop(file);
    tokio::fs::rename(temporary, destination)
        .await
        .map_err(|error| Error::msg(format!("replace ASR credentials: {error}")))?;
    Ok(())
}

pub(crate) async fn register() -> Result<Credentials> {
    let client = Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|_| Error::msg("could not prepare Doubao device registration"))?;
    let mut credentials = Credentials::new();
    credentials.register_device(&client).await?;
    credentials.fetch_token(&client).await?;
    Ok(credentials)
}

fn device_url(base: &str, credentials: &Credentials, include_device_id: bool) -> Result<Url> {
    let mut url = Url::parse(base).map_err(|_| Error::msg("could not prepare Doubao request"))?;
    let rticket = unix_millis().to_string();
    let mut query = url.query_pairs_mut();
    for (key, value) in [
        ("device_platform", "android"),
        ("os", "android"),
        ("ssmix", "a"),
        ("_rticket", &rticket),
        ("cdid", &credentials.cdid),
        ("aid", "401734"),
        ("app_name", "oime"),
        ("version_code", "100311008"),
        ("version_name", "1.3.11"),
        ("channel", "official"),
        ("ac", "wifi"),
        ("resolution", "1080*2340"),
        ("dpi", "440"),
        ("device_type", "Redmi Note 7"),
        ("device_brand", "xiaomi"),
        ("language", "zh"),
        ("os_api", "28"),
        ("os_version", "9"),
    ] {
        query.append_pair(key, value);
    }
    if include_device_id {
        query.append_pair("device_id", &credentials.device_id);
    }
    drop(query);
    Ok(url)
}

async fn decode_limited<T: for<'de> Deserialize<'de>>(
    mut response: Response,
    message: &'static str,
) -> Result<T> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(Error::msg(message));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| Error::msg(message))? {
        if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
            return Err(Error::msg(message));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| Error::msg(message))
}

fn unix_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(
        String::with_capacity(bytes.len() * 2),
        |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_file_loads_as_none() {
        let directory = tempfile::tempdir().unwrap();
        assert!(load(&directory.path().join("missing.json"))
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn malformed_and_incomplete_files_are_rejected_safely() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.json");
        tokio::fs::write(&path, b"not json").await.unwrap();
        assert!(load(&path)
            .await
            .unwrap_err()
            .to_string()
            .contains("malformed"));
        tokio::fs::write(&path, br#"{"device_id":"1","install_id":"2","cdid":"c","openudid":"o","clientudid":"u","token":""}"#).await.unwrap();
        assert!(load(&path)
            .await
            .unwrap_err()
            .to_string()
            .contains("incomplete"));
    }

    #[tokio::test]
    async fn save_is_atomic_and_round_trips() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/credentials.json");
        let first = Credentials::for_test("1", "2", "secret-one");
        let second = Credentials::for_test("3", "4", "secret-two");
        save(&path, &first).await.unwrap();
        save(&path, &second).await.unwrap();
        let loaded = load(&path).await.unwrap().unwrap();
        assert_eq!(loaded.device_id, "3");
        assert_eq!(loaded.token, "secret-two");
        assert_eq!(
            tokio::fs::read_dir(path.parent().unwrap())
                .await
                .unwrap()
                .next_entry()
                .await
                .unwrap()
                .unwrap()
                .file_name(),
            "credentials.json"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn saved_file_is_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("credentials.json");
        save(&path, &Credentials::for_test("1", "2", "secret"))
            .await
            .unwrap();
        assert_eq!(
            tokio::fs::metadata(path)
                .await
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn generated_identifiers_have_expected_shapes() {
        let credentials = Credentials::new();
        assert_eq!(credentials.openudid.len(), 16);
        assert_eq!(credentials.cdid.len(), 36);
        assert_eq!(credentials.clientudid.len(), 36);
    }

    #[test]
    fn device_parameters_match_android_client() {
        let credentials = Credentials::for_test("123", "456", "token");
        let url = device_url(REGISTER_URL, &credentials, true).unwrap();
        let pairs: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(pairs.get("aid").unwrap(), "401734");
        assert_eq!(pairs.get("device_id").unwrap(), "123");
        assert_eq!(pairs.get("resolution").unwrap(), "1080*2340");
    }
}
