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

#[derive(Serialize)]
pub struct ErrorOutput {
    pub state: &'static str,
    pub error: ErrorDetail,
}

#[derive(Debug)]
pub struct PartialSync(pub Vec<crate::model::SyncResult>);

impl fmt::Display for PartialSync {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("部分同步失敗；已同步資料與失敗種類的原快取仍保留")
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
            // 與成功時同一組頂層欄位（checks、version、build_commit…），
            // `state` 維持 "error"；`report` 保留給既有的解析腳本。
            for (key, value) in report.as_object().expect("report is an object") {
                if key != "state" {
                    output[key] = value.clone();
                }
            }
            output["report"] = report;
        }
        if let Some(partial) = error.downcast_ref::<PartialSync>() {
            output["state"] = "partial".into();
            output["results"] =
                serde_json::to_value(&partial.0).expect("serializable sync results");
        }
        output
    }

    pub fn from_error(error: &anyhow::Error) -> Self {
        let (code, retryable) = if error.downcast_ref::<PartialSync>().is_some() {
            ("partial_sync", false)
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
