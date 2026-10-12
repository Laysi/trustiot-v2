//! `migrate wait-for-job`: the init container of every service pod. It holds the pod until the
//! migration Job named in its settings (the chart names it after a hash of the Job's spec) has
//! completed, so a service never starts on a schema older than its own
//! (docs/architecture/deployment.md, "migration Job").
//!
//! It reads the Job's status from the Kubernetes API with a projected ServiceAccount token that
//! only this container mounts; the chart gives the services' ServiceAccounts `get` on that one
//! Job and nothing else.

use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::config::KubeSettings;
use crate::http;

const POLL_EVERY: Duration = Duration::from_secs(2);
const REPORT_EVERY: Duration = Duration::from_secs(30);

/// Why waiting stopped without a completed Job.
#[derive(Debug, thiserror::Error)]
pub enum WaitError {
    #[error("the migration Job {0} failed; see its pod's logs")]
    JobFailed(String),
    #[error(
        "the Kubernetes API refused to show the migration Job {job} (HTTP {status}); the chart \
         grants the service's ServiceAccount get on it"
    )]
    Forbidden { job: String, status: u16 },
    #[error("cannot read the ServiceAccount token {path}: {message}")]
    Token { path: String, message: String },
    #[error("{0}")]
    Client(#[from] http::ClientError),
}

#[derive(Deserialize)]
struct Job {
    #[serde(default)]
    status: JobStatus,
}

#[derive(Default, Deserialize)]
struct JobStatus {
    #[serde(default)]
    conditions: Vec<Condition>,
}

#[derive(Deserialize)]
struct Condition {
    #[serde(rename = "type")]
    kind: String,
    status: String,
}

/// What one look at the Job found.
#[derive(Debug, PartialEq, Eq)]
pub enum Seen {
    Complete,
    Failed,
    /// The Job does not exist; look again. The text says why that may be and what to do.
    Missing(String),
    /// Running, or the API did not answer; look again.
    Pending(String),
}

/// What a pod waiting for a Job that does not exist logs. On a fresh install or upgrade the Job
/// appears within seconds; when it never does, the pod belongs to another release than the one
/// installed, because an upgrade or rollback deleted the Job this pod's spec names. The wait
/// goes on (the Deployment's progress deadline surfaces it), so the message names the way out.
#[must_use]
pub fn missing_job_message(job: &str) -> String {
    format!(
        "the migration Job {job} does not exist. Right after helm install or upgrade it appears \
         within seconds; if it does not, this pod comes from another release than the one \
         installed (an upgrade or rollback replaced the Job it waits for): finish the upgrade so \
         its new pods replace this one, or return to the release that has this Job with \
         `helm rollback`; to run a Job again on purpose, change migrate.revision \
         (docs/runbooks/migrations.md)"
    )
}

/// Reads the Job's conditions from an API response body.
#[must_use]
pub fn conditions(body: &str) -> Seen {
    let Ok(job) = serde_json::from_str::<Job>(body) else {
        return Seen::Pending("the API answered something that is not a Job".to_owned());
    };
    let is = |kind: &str| {
        job.status
            .conditions
            .iter()
            .any(|condition| condition.kind == kind && condition.status == "True")
    };
    if is("Complete") {
        Seen::Complete
    } else if is("Failed") {
        Seen::Failed
    } else {
        Seen::Pending("running".to_owned())
    }
}

async fn look(
    client: &http::Client,
    settings: &KubeSettings,
    url: &str,
) -> Result<Seen, WaitError> {
    let token = tokio::fs::read_to_string(&settings.token_file)
        .await
        .map_err(|error| WaitError::Token {
            path: settings.token_file.display().to_string(),
            message: error.to_string(),
        })?;
    let response = match client.get(url, http::Auth::Bearer(token.trim())).await {
        Ok(response) => response,
        Err(error) => return Ok(Seen::Pending(format!("the API did not answer: {error}"))),
    };
    let status = response.status;
    match status.as_u16() {
        200 => Ok(conditions(&response.body)),
        401 | 403 => Err(WaitError::Forbidden {
            job: settings.job.clone(),
            status: status.as_u16(),
        }),
        404 => Ok(Seen::Missing(missing_job_message(&settings.job))),
        other => Ok(Seen::Pending(format!("the API answered HTTP {other}"))),
    }
}

/// Waits until the Job has the `Complete` condition.
///
/// # Errors
///
/// When the Job failed, the API refuses access, or the token or CA cannot be read. A missing
/// Job or an API that does not answer is waited out: the kubelet keeps the pod waiting, and the
/// logs say why every 30 seconds.
pub async fn wait_for_job(settings: &KubeSettings) -> Result<(), WaitError> {
    let (client, url) = client_and_url(settings)?;
    let started = Instant::now();
    let mut reported = None::<Instant>;
    loop {
        match look(&client, settings, &url).await? {
            Seen::Complete => {
                tracing::info!(
                    job = %settings.job,
                    waited_ms = started.elapsed().as_millis(),
                    "the migration Job has completed"
                );
                return Ok(());
            }
            Seen::Failed => return Err(WaitError::JobFailed(settings.job.clone())),
            Seen::Missing(why) => {
                if reported.is_none_or(|at| at.elapsed() >= REPORT_EVERY) {
                    tracing::warn!(job = %settings.job, state = %why, "waiting for the migration Job");
                    reported = Some(Instant::now());
                }
            }
            Seen::Pending(why) => {
                if reported.is_none_or(|at| at.elapsed() >= REPORT_EVERY) {
                    tracing::info!(job = %settings.job, state = %why, "waiting for the migration Job");
                    reported = Some(Instant::now());
                }
            }
        }
        tokio::time::sleep(POLL_EVERY).await;
    }
}

/// One look at the Job, as the wait takes it every two seconds.
///
/// # Errors
///
/// As [`wait_for_job`], except that a failed Job is [`Seen::Failed`].
pub async fn look_once(settings: &KubeSettings) -> Result<Seen, WaitError> {
    let (client, url) = client_and_url(settings)?;
    look(&client, settings, &url).await
}

fn client_and_url(settings: &KubeSettings) -> Result<(http::Client, String), WaitError> {
    let client = if settings.api_url.starts_with("https://") {
        http::trusting(&settings.ca_file)?
    } else {
        http::plain()
    };
    let url = format!(
        "{}/apis/batch/v1/namespaces/{}/jobs/{}",
        settings.api_url.trim_end_matches('/'),
        settings.namespace,
        settings.job
    );
    Ok((client, url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_job_conditions() {
        let complete = r#"{"kind":"Job","status":{"conditions":[{"type":"SuccessCriteriaMet","status":"True"},{"type":"Complete","status":"True"}]}}"#;
        assert_eq!(conditions(complete), Seen::Complete);
        let failed = r#"{"status":{"conditions":[{"type":"Failed","status":"True"}]}}"#;
        assert_eq!(conditions(failed), Seen::Failed);
        let running = r#"{"status":{"active":1}}"#;
        assert!(matches!(conditions(running), Seen::Pending(_)));
        let suspended = r#"{"status":{"conditions":[{"type":"Complete","status":"False"}]}}"#;
        assert!(matches!(conditions(suspended), Seen::Pending(_)));
        assert!(matches!(conditions("not json"), Seen::Pending(_)));
    }
}
