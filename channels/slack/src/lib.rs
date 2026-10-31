pub mod apps;

/// Operator-owned Slack API configuration, also used by isolated child-process tests.
pub fn api_base_url() -> String {
    std::env::var("SLACK_API_BASE_URL").unwrap_or_else(|_| "https://slack.com/api".to_string())
}
