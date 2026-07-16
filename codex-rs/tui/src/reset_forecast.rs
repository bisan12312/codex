//! Live reset-likelihood forecast used by the configurable TUI status line.
//!
//! The source is the unofficial Will Codex Reset site. Its forecast is a probability over a
//! fixed horizon, not a personal quota balance or an exact reset ETA. Only the numeric score,
//! announcement flag, and server refresh timestamps are accepted from the third-party response.

use std::time::Duration;

use chrono::DateTime;
use chrono::Utc;
use serde::Deserialize;

pub(crate) const RESET_FORECAST_RETRY_INTERVAL: Duration = Duration::from_secs(60 * 60);

const FORECAST_API_URL: &str = "https://www.willcodexquotareset.com/api/forecast";
const FORECAST_HORIZON_HOURS: u8 = 48;
const ANNOUNCED_FORECAST_HORIZON_HOURS: u8 = 24;
const FORECAST_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const MIN_FORECAST_REFRESH_DELAY: Duration = Duration::from_secs(60);
const MAX_SERVER_REFRESH_INTERVAL: Duration = Duration::from_secs(2 * 60 * 60);
const USER_AGENT: &str = "codex-reset-forecast-statusline/1.0";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResetForecast {
    score: u8,
    reset_announced: bool,
    refresh_after: Duration,
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
        format!("{marker} {}% ≤{horizon_hours}h", self.score)
    }

    pub(crate) fn refresh_after(&self) -> Duration {
        self.refresh_after
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
            Self::Failed => "reset? ERROR".to_string(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForecastApiResponse {
    fetched_at: DateTime<Utc>,
    next_refresh_at: DateTime<Utc>,
    forecast: ForecastApiValue,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ForecastApiValue {
    score: u16,
    reset_announced: bool,
}

pub(crate) async fn fetch_reset_forecast() -> Result<ResetForecast, String> {
    let client = reqwest::Client::builder()
        .timeout(FORECAST_REQUEST_TIMEOUT)
        .user_agent(USER_AGENT)
        .build()
        .map_err(|err| format!("failed to build reset-forecast HTTP client: {err}"))?;
    let response = client
        .get(FORECAST_API_URL)
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .map_err(|err| format!("reset-forecast request failed: {err}"))?;
    let response = response
        .error_for_status()
        .map_err(|err| format!("reset-forecast endpoint returned an error: {err}"))?;
    let payload = response
        .json::<ForecastApiResponse>()
        .await
        .map_err(|err| format!("reset-forecast endpoint returned invalid JSON: {err}"))?;

    validate_forecast(payload, Utc::now())
}

fn validate_forecast(
    payload: ForecastApiResponse,
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

    fn parse(json: &str, now: &str) -> Result<ResetForecast, String> {
        let payload = serde_json::from_str::<ForecastApiResponse>(json)
            .expect("forecast fixture should deserialize");
        let now = DateTime::parse_from_rfc3339(now)
            .expect("test time should parse")
            .with_timezone(&Utc);
        validate_forecast(payload, now)
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
}
