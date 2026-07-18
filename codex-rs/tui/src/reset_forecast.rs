//! Live reset-likelihood forecast used by the configurable TUI status line.
//!
//! The source is the unofficial Will Codex Reset site. Its forecast is a probability over a
//! fixed horizon, not a personal quota balance or an exact reset ETA. Only the numeric score,
//! announcement flag, and server refresh timestamps are accepted from the third-party response.

use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;
use serde::Serialize;

pub(crate) const RESET_FORECAST_RETRY_INTERVAL: Duration = Duration::from_secs(60 * 60);

const FORECAST_API_URL: &str = "https://www.willcodexquotareset.com/api/forecast";
const FORECAST_HORIZON_HOURS: u8 = 48;
const ANNOUNCED_FORECAST_HORIZON_HOURS: u8 = 24;
const FORECAST_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const BROWSER_REFRESH_TIMEOUT: Duration = Duration::from_secs(60);
const MIN_FORECAST_REFRESH_DELAY: Duration = Duration::from_secs(60);
const MAX_SERVER_REFRESH_INTERVAL: Duration = Duration::from_secs(2 * 60 * 60);
const MAX_STALE_FORECAST_AGE: Duration = MAX_SERVER_REFRESH_INTERVAL;
const FORECAST_CACHE_DIRECTORY: &str = "cache";
const FORECAST_CACHE_FILE: &str = "will-codex-reset-forecast.json";
const FORECAST_BROWSER_REFRESH_SCRIPT: &str = "scripts/refresh-codex-reset-forecast";
const USER_AGENT: &str = "codex-reset-forecast-statusline/1.0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResetForecast {
    score: u8,
    reset_announced: bool,
    refresh_after: Duration,
    stale: bool,
}

impl ResetForecast {
    pub(crate) fn new(
        score: u16,
        reset_announced: bool,
        refresh_after: Duration,
    ) -> Result<Self, String> {
        if score > 100 {
            return Err(format!("reset-forecast score is outside 0..=100: {score}"));
        }

        Ok(Self {
            score: score as u8,
            reset_announced,
            refresh_after,
            stale: false,
        })
    }

    pub(crate) fn status_line_text(&self) -> String {
        let marker = if self.reset_announced {
            "reset!"
        } else {
            "reset?"
        };
        let horizon_hours = if self.reset_announced {
            ANNOUNCED_FORECAST_HORIZON_HOURS
        } else {
            FORECAST_HORIZON_HOURS
        };
        let text = format!("{marker} {}% ≤{horizon_hours}h", self.score);
        if self.stale {
            format!("{text} STALE")
        } else {
            text
        }
    }

    pub(crate) fn refresh_after(&self) -> Duration {
        self.refresh_after
    }

    fn into_stale(mut self) -> Self {
        self.stale = true;
        self.refresh_after = RESET_FORECAST_RETRY_INTERVAL;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResetForecastState {
    Loading,
    Available(ResetForecast),
    Failed,
}

impl ResetForecastState {
    pub(crate) fn status_line_text(&self) -> String {
        match self {
            Self::Loading => "reset? loading".to_string(),
            Self::Available(forecast) => forecast.status_line_text(),
            Self::Failed => "reset? UNAVAILABLE".to_string(),
        }
    }

    pub(crate) fn after_fetch_failure(&self) -> Self {
        match self {
            Self::Available(forecast) => Self::Available(forecast.clone().into_stale()),
            Self::Loading | Self::Failed => Self::Failed,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ForecastApiResponse {
    fetched_at: DateTime<Utc>,
    next_refresh_at: DateTime<Utc>,
    forecast: ForecastApiValue,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct ForecastApiValue {
    score: u16,
    reset_announced: bool,
}

#[derive(Debug)]
enum LiveForecastError {
    BrowserCheckpoint(String),
    Other(String),
}

pub(crate) async fn fetch_reset_forecast() -> Result<ResetForecast, String> {
    let cache_path = forecast_cache_path()?;
    let network_result = match fetch_live_forecast_payload().await {
        Ok(payload) => Ok(payload),
        Err(LiveForecastError::BrowserCheckpoint(network_error)) => {
            let browser_result = refresh_forecast_cache_in_browser().await;
            let combined_error = match browser_result {
                Ok(()) => network_error,
                Err(browser_error) => {
                    format!("{network_error}; browser refresh failed: {browser_error}")
                }
            };
            Err(combined_error)
        }
        Err(LiveForecastError::Other(network_error)) => Err(network_error),
    };
    resolve_forecast_result(network_result, &cache_path, Utc::now())
}

async fn fetch_live_forecast_payload() -> Result<ForecastApiResponse, LiveForecastError> {
    let client = reqwest::Client::builder()
        .timeout(FORECAST_REQUEST_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|err| {
            LiveForecastError::Other(format!("failed to build reset-forecast HTTP client: {err}"))
        })?;
    let response = client
        .get(FORECAST_API_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|err| LiveForecastError::Other(format!("reset-forecast request failed: {err}")))?;
    if response.status() == reqwest::StatusCode::FORBIDDEN
        && response
            .headers()
            .get("x-vercel-mitigated")
            .and_then(|value| value.to_str().ok())
            == Some("challenge")
    {
        return Err(LiveForecastError::BrowserCheckpoint(
            "reset-forecast endpoint is blocked by a Vercel browser checkpoint (HTTP 403)"
                .to_string(),
        ));
    }
    let response = response.error_for_status().map_err(|err| {
        LiveForecastError::Other(format!("reset-forecast endpoint returned an error: {err}"))
    })?;
    let payload = response
        .json::<ForecastApiResponse>()
        .await
        .map_err(|err| {
            LiveForecastError::Other(format!(
                "reset-forecast endpoint returned invalid JSON: {err}"
            ))
        })?;

    Ok(payload)
}

async fn refresh_forecast_cache_in_browser() -> Result<(), String> {
    let codex_home = codex_utils_home_dir::find_codex_home().map_err(|err| {
        format!("failed to locate Codex home for reset-forecast browser refresh: {err}")
    })?;
    let script_path = codex_home.join(FORECAST_BROWSER_REFRESH_SCRIPT);
    let mut command = tokio::process::Command::new(&*script_path);
    command.kill_on_drop(true);
    let output = tokio::time::timeout(BROWSER_REFRESH_TIMEOUT, command.output())
        .await
        .map_err(|_| {
            format!(
                "reset-forecast browser refresh exceeded {BROWSER_REFRESH_TIMEOUT:?}: {}",
                script_path.display()
            )
        })?
        .map_err(|err| {
            format!(
                "failed to run reset-forecast browser refresh {}: {err}",
                script_path.display()
            )
        })?;
    if !output.status.success() {
        return Err(format!(
            "reset-forecast browser refresh {} exited with {}; stderr: {}",
            script_path.display(),
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    tracing::debug!(
        output = %String::from_utf8_lossy(&output.stdout).trim(),
        "reset-forecast browser refresh completed"
    );
    Ok(())
}

fn forecast_cache_path() -> Result<PathBuf, String> {
    let codex_home = codex_utils_home_dir::find_codex_home()
        .map_err(|err| format!("failed to locate Codex home for reset-forecast cache: {err}"))?;
    Ok(codex_home
        .join(FORECAST_CACHE_DIRECTORY)
        .join(FORECAST_CACHE_FILE)
        .to_path_buf())
}

fn resolve_forecast_result(
    network_result: Result<ForecastApiResponse, String>,
    cache_path: &Path,
    now: DateTime<Utc>,
) -> Result<ResetForecast, String> {
    match network_result {
        Ok(payload) => {
            let forecast = validate_forecast(&payload, now)?;
            persist_forecast_cache(cache_path, &payload)?;
            Ok(forecast)
        }
        Err(network_error) => match load_cached_forecast(cache_path, now) {
            Ok(forecast) => {
                if forecast.stale {
                    tracing::warn!(
                        error = %network_error,
                        cache = %cache_path.display(),
                        "using explicitly stale Will Codex Reset forecast"
                    );
                } else {
                    tracing::debug!(
                        error = %network_error,
                        cache = %cache_path.display(),
                        "using current browser-refreshed Will Codex Reset forecast"
                    );
                }
                Ok(forecast)
            }
            Err(cache_error) => Err(format!(
                "{network_error}; cached reset forecast unavailable: {cache_error}"
            )),
        },
    }
}

fn load_cached_forecast(cache_path: &Path, now: DateTime<Utc>) -> Result<ResetForecast, String> {
    let serialized = std::fs::read(cache_path).map_err(|err| {
        format!(
            "failed to read reset-forecast cache {}: {err}",
            cache_path.display()
        )
    })?;
    let payload = serde_json::from_slice::<ForecastApiResponse>(&serialized).map_err(|err| {
        format!(
            "reset-forecast cache {} contains invalid JSON: {err}",
            cache_path.display()
        )
    })?;
    let cache_age = now
        .signed_duration_since(payload.fetched_at)
        .to_std()
        .map_err(|_| "cached reset forecast was fetched in the future".to_string())?;
    if cache_age > MAX_STALE_FORECAST_AGE {
        return Err(format!(
            "cached reset forecast is older than {MAX_STALE_FORECAST_AGE:?}: {cache_age:?}"
        ));
    }

    let forecast = validate_forecast(&payload, now)?;
    if payload.next_refresh_at <= now {
        Ok(forecast.into_stale())
    } else {
        Ok(forecast)
    }
}

fn persist_forecast_cache(cache_path: &Path, payload: &ForecastApiResponse) -> Result<(), String> {
    let parent = cache_path.parent().ok_or_else(|| {
        format!(
            "reset-forecast cache path has no parent: {}",
            cache_path.display()
        )
    })?;
    std::fs::create_dir_all(parent).map_err(|err| {
        format!(
            "failed to create reset-forecast cache directory {}: {err}",
            parent.display()
        )
    })?;
    let serialized = serde_json::to_vec(payload)
        .map_err(|err| format!("failed to serialize reset-forecast cache: {err}"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|err| {
        format!(
            "failed to create temporary reset-forecast cache in {}: {err}",
            parent.display()
        )
    })?;
    temporary
        .write_all(&serialized)
        .map_err(|err| format!("failed to write temporary reset-forecast cache: {err}"))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|err| format!("failed to sync temporary reset-forecast cache: {err}"))?;
    temporary.persist(cache_path).map_err(|err| {
        format!(
            "failed to atomically replace reset-forecast cache {}: {err}",
            cache_path.display()
        )
    })?;
    Ok(())
}

fn validate_forecast(
    payload: &ForecastApiResponse,
    now: DateTime<Utc>,
) -> Result<ResetForecast, String> {
    let server_refresh_interval = payload
        .next_refresh_at
        .signed_duration_since(payload.fetched_at)
        .to_std()
        .map_err(|_| "reset-forecast nextRefreshAt is not after fetchedAt".to_string())?;
    if server_refresh_interval.is_zero() || server_refresh_interval > MAX_SERVER_REFRESH_INTERVAL {
        return Err(format!(
            "reset-forecast server refresh interval is invalid: {server_refresh_interval:?}"
        ));
    }

    let refresh_after = payload
        .next_refresh_at
        .signed_duration_since(now)
        .to_std()
        .unwrap_or(MIN_FORECAST_REFRESH_DELAY)
        .max(MIN_FORECAST_REFRESH_DELAY);

    ResetForecast::new(
        payload.forecast.score,
        payload.forecast.reset_announced,
        refresh_after,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use tempfile::tempdir;

    fn payload(json: &str) -> ForecastApiResponse {
        serde_json::from_str::<ForecastApiResponse>(json)
            .expect("forecast fixture should deserialize")
    }

    fn time(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .expect("test time should parse")
            .with_timezone(&Utc)
    }

    fn parse(json: &str, now: &str) -> Result<ResetForecast, String> {
        let payload = payload(json);
        validate_forecast(&payload, time(now))
    }

    #[test]
    fn forecast_renders_probability_and_site_horizon() {
        let forecast = parse(
            r#"{
                "fetchedAt": "2026-07-16T10:57:01.806Z",
                "nextRefreshAt": "2026-07-16T11:27:01.806Z",
                "forecast": {"score": 51, "resetAnnounced": false}
            }"#,
            "2026-07-16T11:00:01.806Z",
        )
        .expect("valid forecast");

        assert_eq!(
            forecast,
            ResetForecast {
                score: 51,
                reset_announced: false,
                refresh_after: Duration::from_secs(27 * 60),
                stale: false,
            }
        );
        assert_eq!(forecast.status_line_text(), "reset? 51% ≤48h");
    }

    #[test]
    fn announced_reset_uses_the_sites_24_hour_announcement_window() {
        let forecast = parse(
            r#"{
                "fetchedAt": "2026-07-16T10:57:01.806Z",
                "nextRefreshAt": "2026-07-16T11:27:01.806Z",
                "forecast": {"score": 100, "resetAnnounced": true}
            }"#,
            "2026-07-16T11:00:01.806Z",
        )
        .expect("valid forecast");

        assert_eq!(forecast.status_line_text(), "reset! 100% ≤24h");
    }

    #[test]
    fn invalid_score_fails_loudly() {
        let error = parse(
            r#"{
                "fetchedAt": "2026-07-16T10:57:01.806Z",
                "nextRefreshAt": "2026-07-16T11:27:01.806Z",
                "forecast": {"score": 101, "resetAnnounced": false}
            }"#,
            "2026-07-16T11:00:01.806Z",
        )
        .expect_err("out-of-range score should fail");

        assert_eq!(error, "reset-forecast score is outside 0..=100: 101");
    }

    #[test]
    fn browser_checkpoint_uses_current_browser_refreshed_forecast_as_fresh() {
        let temp = tempdir().expect("temp cache directory");
        let cache_path = temp.path().join("reset-forecast.json");
        let cached_payload = payload(
            r#"{
                "fetchedAt": "2026-07-18T08:32:11.055Z",
                "nextRefreshAt": "2026-07-18T09:02:11.055Z",
                "forecast": {"score": 3, "resetAnnounced": false}
            }"#,
        );
        persist_forecast_cache(&cache_path, &cached_payload).expect("persist cache fixture");

        let forecast = resolve_forecast_result(
            Err("Vercel browser checkpoint (HTTP 403)".to_string()),
            &cache_path,
            time("2026-07-18T08:37:11.055Z"),
        )
        .expect("recent cache should remain visible");

        assert_eq!(forecast.status_line_text(), "reset? 3% ≤48h");
        assert_eq!(forecast.refresh_after(), Duration::from_secs(25 * 60));
    }

    #[test]
    fn browser_checkpoint_rejects_expired_persisted_forecast() {
        let temp = tempdir().expect("temp cache directory");
        let cache_path = temp.path().join("reset-forecast.json");
        let cached_payload = payload(
            r#"{
                "fetchedAt": "2026-07-18T05:32:11.055Z",
                "nextRefreshAt": "2026-07-18T06:02:11.055Z",
                "forecast": {"score": 95, "resetAnnounced": false}
            }"#,
        );
        persist_forecast_cache(&cache_path, &cached_payload).expect("persist cache fixture");

        let error = resolve_forecast_result(
            Err("Vercel browser checkpoint (HTTP 403)".to_string()),
            &cache_path,
            time("2026-07-18T08:37:11.055Z"),
        )
        .expect_err("expired cache must not be shown as current");

        assert!(error.contains("cached reset forecast is older than"));
    }

    #[test]
    fn recent_cache_past_next_refresh_is_explicitly_stale_and_retries_hourly() {
        let temp = tempdir().expect("temp cache directory");
        let cache_path = temp.path().join("reset-forecast.json");
        let cached_payload = payload(
            r#"{
                "fetchedAt": "2026-07-18T08:32:11.055Z",
                "nextRefreshAt": "2026-07-18T09:02:11.055Z",
                "forecast": {"score": 3, "resetAnnounced": false}
            }"#,
        );
        persist_forecast_cache(&cache_path, &cached_payload).expect("persist cache fixture");

        let forecast = resolve_forecast_result(
            Err("forecast request failed".to_string()),
            &cache_path,
            time("2026-07-18T09:32:11.055Z"),
        )
        .expect("recent expired cache should remain visible as stale");

        assert_eq!(forecast.status_line_text(), "reset? 3% ≤48h STALE");
        assert_eq!(forecast.refresh_after(), RESET_FORECAST_RETRY_INTERVAL);
    }

    #[test]
    fn successful_live_request_persists_forecast_for_a_new_terminal() {
        let temp = tempdir().expect("temp cache directory");
        let cache_path = temp.path().join("reset-forecast.json");
        let live_payload = payload(
            r#"{
                "fetchedAt": "2026-07-18T08:32:11.055Z",
                "nextRefreshAt": "2026-07-18T09:02:11.055Z",
                "forecast": {"score": 3, "resetAnnounced": false}
            }"#,
        );

        let forecast = resolve_forecast_result(
            Ok(live_payload),
            &cache_path,
            time("2026-07-18T08:37:11.055Z"),
        )
        .expect("live forecast should validate and persist");

        assert_eq!(forecast.status_line_text(), "reset? 3% ≤48h");
        let persisted = std::fs::read_to_string(&cache_path).expect("persisted forecast cache");
        assert_eq!(
            payload(&persisted).forecast.score,
            3,
            "new terminals should be able to read the last validated score"
        );
    }
}
