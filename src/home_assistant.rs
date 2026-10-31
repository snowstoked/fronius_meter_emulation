use serde_derive::{Deserialize, Serialize};
use std::{env, time::Duration};

/// Time limit for one request to Home Assistant. Without a limit a hanging Home Assistant would stall the polling
/// cycle of the power reading.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

pub struct HomeAssistantAPI {
    endpoint_url: String,
    auth_token: String,
    client: reqwest::Client,
}

impl HomeAssistantAPI {
    pub fn new() -> Self {
        Self::with_endpoint(
            env::var("HA_URL").unwrap_or_default(),
            env::var("HA_TOKEN").unwrap_or_default(),
        )
        .with_timeout(
            env::var("HA_TIMEOUT_MS")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|ms| *ms >= 100)
                .map(Duration::from_millis)
                .unwrap_or(DEFAULT_TIMEOUT),
        )
    }

    /// Same as `new` but without reading the environment (used by tests)
    pub fn with_endpoint(endpoint_url: String, auth_token: String) -> Self {
        Self {
            endpoint_url,
            auth_token,
            client: Self::client(DEFAULT_TIMEOUT),
        }
    }

    /// Change the time limit of a request (connecting and answering together)
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.client = Self::client(timeout);
        self
    }

    fn client(timeout: Duration) -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(timeout)
            .connect_timeout(timeout)
            .build()
            .unwrap_or_default()
    }

    pub fn is_configured(&self) -> bool {
        !self.endpoint_url.is_empty() && !self.auth_token.is_empty()
    }

    pub async fn read_sensor_value(&self, sensor_path: &str) -> Result<HASensor, anyhow::Error> {
        if self.endpoint_url.is_empty() {
            anyhow::bail!("No HA connection");
        }
        let result = self
            .client
            .get(format!("{}/api/states/{}", self.endpoint_url, sensor_path))
            .bearer_auth(&self.auth_token)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        Ok(result)
    }
}

#[derive(Default, Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HASensor {
    #[serde(rename = "entity_id")]
    pub entity_id: String,
    pub state: String,
    #[serde(rename = "last_changed")]
    pub last_changed: String,
    #[serde(rename = "last_reported")]
    pub last_reported: String,
    #[serde(rename = "last_updated")]
    pub last_updated: String,
}

#[cfg(test)]
mod test_ha_wrapper {
    use super::*;

    #[tokio::test]
    async fn test_home_assistant_api() {
        // Set up the mock server
        let mut server = mockito::Server::new_async().await;

        let mock = server
            .mock("GET", "/api/states/sensor.temperature")
            .match_header("Authorization", "Bearer test_token")
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(
                r#"
                {
                    "entity_id": "sensor.temperature",
                    "state": "22.5",
                    "last_changed": "2023-01-01T12:00:00Z",
                    "last_reported": "2023-01-01T12:00:00Z",
                    "last_updated": "2023-01-01T12:00:00Z"
                }
            "#,
            )
            .create();

        let api = HomeAssistantAPI::with_endpoint(server.url(), "test_token".into());
        let result = api.read_sensor_value("sensor.temperature").await.unwrap();

        // Verify result
        assert_eq!(result.entity_id, "sensor.temperature");
        assert_eq!(result.state, "22.5");
        assert_eq!(result.last_changed, "2023-01-01T12:00:00Z");
        assert_eq!(result.last_reported, "2023-01-01T12:00:00Z");
        assert_eq!(result.last_updated, "2023-01-01T12:00:00Z");

        // Verify that the mock was called
        mock.assert();
    }

    #[tokio::test]
    async fn test_home_assistant_api_no_connection() {
        let api = HomeAssistantAPI::with_endpoint(String::new(), String::new());
        let result = api.read_sensor_value("sensor.temperature").await;

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "No HA connection");
    }

    #[tokio::test]
    async fn http_error_status_is_an_error() {
        let mut server = mockito::Server::new_async().await;
        let _m = server
            .mock("GET", "/api/states/x")
            .with_status(401)
            .with_body("{\"message\":\"401: Unauthorized\"}")
            .create_async()
            .await;
        let api = HomeAssistantAPI::with_endpoint(server.url(), "t".into());
        let e = api.read_sensor_value("x").await.unwrap_err().to_string();
        assert!(e.contains("401"), "{e}");
    }

    /// A server that accepts the connection and then says nothing
    async fn silent_server() -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                held.push(socket);
            }
        });
        (url, task)
    }

    #[tokio::test]
    async fn hanging_server_is_given_up_after_the_timeout() {
        let (url, task) = silent_server().await;
        let api = HomeAssistantAPI::with_endpoint(url, "t".into())
            .with_timeout(Duration::from_millis(200));
        let started = std::time::Instant::now();
        let result = api.read_sensor_value("x").await;
        assert!(result.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        task.abort();
    }

    #[tokio::test]
    async fn default_timeout_is_short() {
        assert!(DEFAULT_TIMEOUT <= Duration::from_secs(3));
    }
}
