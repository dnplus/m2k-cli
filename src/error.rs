use serde::Serialize;
use std::fmt;

#[derive(Debug)]
pub struct Failure {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Failure {}

pub const AUTH_FAILED: &str = "auth_failed";

pub fn auth_failed(service: &str, detail: impl fmt::Display) -> Failure {
    Failure {
        code: AUTH_FAILED,
        message: format!("{service} 帳號或密碼遭拒絕（{detail}）；請確認密碼後執行 m2k auth login"),
        retryable: false,
    }
}

pub fn is_auth_failed(error: &anyhow::Error) -> bool {
    ErrorOutput::from_error(error).error.code == AUTH_FAILED
}

#[derive(Serialize)]
pub struct ErrorOutput {
    pub state: &'static str,
    pub error: ErrorDetail,
}

#[derive(Debug)]
pub struct PartialSync(pub Vec<crate::model::SyncResult>);

impl PartialSync {
    fn auth_failed(&self) -> bool {
        self.0
            .iter()
            .any(|result| result.code.as_deref() == Some(AUTH_FAILED))
    }
}

impl fmt::Display for PartialSync {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("部分同步失敗；已同步資料與失敗種類的原快取仍保留")?;
        if self.auth_failed() {
            f.write_str("；帳號或密碼遭拒絕，請確認密碼後執行 m2k auth login")?;
        }
        Ok(())
    }
}

impl std::error::Error for PartialSync {}

#[derive(Serialize)]
pub struct ErrorDetail {
    pub code: &'static str,
    pub message: String,
    pub retryable: bool,
}

impl ErrorOutput {
    pub fn json(error: &anyhow::Error) -> serde_json::Value {
        let mut output =
            serde_json::to_value(Self::from_error(error)).expect("serializable error detail");
        if let Some(doctor) = error.downcast_ref::<crate::doctor::DoctorFailed>() {
            let report = serde_json::to_value(&doctor.0).expect("serializable doctor report");
            for (key, value) in report.as_object().expect("report is an object") {
                if key != "state" {
                    output[key] = value.clone();
                }
            }
        }
        if let Some(partial) = error.downcast_ref::<PartialSync>() {
            output["state"] = "partial".into();
            output["results"] =
                serde_json::to_value(&partial.0).expect("serializable sync results");
        }
        output
    }

    pub fn from_error(error: &anyhow::Error) -> Self {
        let (code, retryable) = if let Some(partial) = error.downcast_ref::<PartialSync>() {
            (
                if partial.auth_failed() {
                    AUTH_FAILED
                } else {
                    "partial_sync"
                },
                false,
            )
        } else if error
            .downcast_ref::<crate::doctor::DoctorFailed>()
            .is_some()
        {
            ("doctor_failed", false)
        } else if let Some(failure) = error.downcast_ref::<Failure>() {
            (failure.code, failure.retryable)
        } else if let Some(network) = error.downcast_ref::<reqwest::Error>() {
            (
                if network.is_timeout() {
                    "network_timeout"
                } else {
                    "network_error"
                },
                false,
            )
        } else if error.downcast_ref::<rusqlite::Error>().is_some() {
            ("storage_error", false)
        } else if let Some(io) = error.downcast_ref::<std::io::Error>() {
            (
                match io.kind() {
                    std::io::ErrorKind::NotFound => "not_found",
                    std::io::ErrorKind::PermissionDenied => "permission_denied",
                    _ => "io_error",
                },
                false,
            )
        } else {
            ("operation_failed", false)
        };
        Self {
            state: "error",
            error: ErrorDetail {
                code,
                message: format!("{error:#}"),
                retryable,
            },
        }
    }

    pub fn invalid_arguments(message: String) -> Self {
        Self {
            state: "error",
            error: ErrorDetail {
                code: "invalid_arguments",
                message,
                retryable: false,
            },
        }
    }
}
