use serde_json::{json, Value};
use url::Url;

use crate::credentials::Credentials;

pub(crate) const DEFAULT_WEBSOCKET_URL: &str =
    "wss://frontier-audio-ime-ws.doubao.com/ocean/api/v1/ws";

#[derive(Clone)]
pub(crate) struct Config {
    pub device_id: String,
    pub install_id: String,
    pub token: String,
    pub punctuation: bool,
    pub websocket_url: String,
}

impl Config {
    pub fn new(credentials: &Credentials, punctuation: bool) -> Self {
        Self {
            device_id: credentials.device_id.clone(),
            install_id: credentials.install_id.clone(),
            token: credentials.token.clone(),
            punctuation,
            websocket_url: DEFAULT_WEBSOCKET_URL.into(),
        }
    }

    pub fn ws_url(&self) -> Result<Url, url::ParseError> {
        let mut url = Url::parse(&self.websocket_url)?;
        let values = [
            ("uid", "0"),
            ("aid", "401734"),
            ("app_name", "oime"),
            ("did", &self.device_id),
            ("iid", &self.install_id),
            ("install_id", &self.install_id),
            ("channel", "official"),
            ("os_version", "9"),
            ("version_code", "100311008"),
            ("update_version_code", "100311008"),
            ("version_name", "1.3.11"),
            ("device_platform", "android"),
            ("device_type", "Redmi Note 7"),
            ("brand", "xiaomi"),
            ("device_id", &self.device_id),
            ("ip", "0"),
            ("user_agent", ""),
            ("forwarded", ""),
            ("target", ""),
            ("mobile", ""),
        ];
        url.query_pairs_mut().clear().extend_pairs(values);
        Ok(url)
    }

    pub fn user_agent(&self) -> &'static str {
        crate::credentials::DEVICE_USER_AGENT
    }

    pub fn session_json(&self) -> Vec<u8> {
        let extra: Value = json!({
            "app_name":"com.android.chrome", "app_version":"1.3.11",
            "cell_compress_rate":8, "device_brand":"xiaomi", "device_model":"Redmi Note 7",
            "did":self.device_id, "disable_user_words":false, "enable_asr_threepass":true,
            "enable_asr_twopass":true, "enable_print_chinese":false,
            "end_smooth_window_ms":800, "finish_wait_offline_time":1000,
            "input_mode":"tool", "join_user_experience_improve_program":false,
            "max_wait_switch_offline_time":1000,
            "network_change":{"switch_network_ping_timeout":2000,"switch_network_quality_threshold":4,"switch_network_rtt_threshold":273},
            "offline_wait_online_interval_time":5000, "offline_wait_online_time":5000,
            "os":"Android", "os_version":"9", "remove_space_between_han_eng":false,
            "remove_space_between_han_num":false,
            "retry_server_code":[40100000,40100004,50000104,50700000],
            "s2a_send_commands":["帮我发送"], "s2a_send_enable":false,
            "strong_ddc":false, "use_twopass_retry":true
        });
        serde_json::to_vec(&json!({
            "audio_info":{"channel":1,"format":"speech_opus","sample_rate":16000},
            "enable_punctuation":self.punctuation,
            "enable_speech_rejection":false,
            "extra":extra
        }))
        .expect("JSON values are serializable")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn url_contains_compatibility_parameters() {
        let c = Credentials::for_test("d", "i", "t");
        let url = Config::new(&c, true).ws_url().unwrap();
        assert_eq!(
            url.query_pairs().find(|(k, _)| k == "aid").unwrap().1,
            "401734"
        );
        assert_eq!(url.query_pairs().find(|(k, _)| k == "did").unwrap().1, "d");
    }
    #[test]
    fn session_punctuation_is_configurable() {
        let c = Credentials::for_test("d", "i", "t");
        let v: Value = serde_json::from_slice(&Config::new(&c, false).session_json()).unwrap();
        assert_eq!(v["enable_punctuation"], false);
        assert_eq!(v["audio_info"]["format"], "speech_opus");
    }
}
