use std::io::{self, Read, Write};

use serde::{Deserialize, Serialize, de::DeserializeOwned};

pub const ADMIN_PROTOCOL_VERSION: u16 = 1;
pub const MAX_ADMIN_FRAME_BYTES: usize = 64 * 1024;
pub const MAX_LOG_CHUNK_BYTES: u32 = 48 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdminRequest {
    pub version: u16,
    pub request_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    pub command: AdminCommand,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdminCommand {
    Health,
    Status,
    ListApps {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
        #[serde(default = "default_list_limit")]
        limit: u16,
    },
    GetApp {
        app: String,
    },
    ListDeployments {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        app: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cursor: Option<String>,
        #[serde(default = "default_list_limit")]
        limit: u16,
    },
    GetDeployment {
        deployment: String,
    },
    ReadLog {
        deployment: String,
        stream: LogStream,
        #[serde(default)]
        offset: u64,
        #[serde(default = "default_log_limit")]
        limit: u32,
    },
}

const fn default_list_limit() -> u16 {
    50
}

const fn default_log_limit() -> u32 {
    16 * 1024
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LogStream {
    Stdout,
    Stderr,
}

impl LogStream {
    pub fn filename(self) -> &'static str {
        match self {
            Self::Stdout => "build.stdout.log",
            Self::Stderr => "build.stderr.log",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdminResponse {
    Ok {
        version: u16,
        request_id: String,
        data: AdminData,
    },
    Error {
        version: u16,
        request_id: String,
        error: AdminFault,
    },
}

impl AdminResponse {
    pub fn ok(request_id: impl Into<String>, data: AdminData) -> Self {
        Self::Ok {
            version: ADMIN_PROTOCOL_VERSION,
            request_id: request_id.into(),
            data,
        }
    }

    pub fn error(
        request_id: impl Into<String>,
        code: AdminErrorCode,
        message: impl Into<String>,
    ) -> Self {
        Self::Error {
            version: ADMIN_PROTOCOL_VERSION,
            request_id: request_id.into(),
            error: AdminFault {
                code,
                message: message.into(),
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdminData {
    Health {
        service: String,
        isolation: String,
    },
    Status {
        node: NodeView,
    },
    Apps {
        apps: Vec<AppView>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_cursor: Option<String>,
    },
    App {
        app: AppView,
    },
    Deployments {
        deployments: Vec<DeploymentView>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        next_cursor: Option<String>,
    },
    Deployment {
        deployment: DeploymentView,
    },
    Log {
        deployment: String,
        stream: LogStream,
        offset: u64,
        next_offset: u64,
        eof: bool,
        data_base64: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeView {
    pub listen: String,
    pub app_count: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AppView {
    pub name: String,
    pub domains: Vec<String>,
    pub lifecycle_state: String,
    pub pinned: bool,
    pub idle_ttl_ms: u64,
    pub egress: String,
    pub memory_max: u64,
    pub env_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<ActiveDeploymentView>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveDeploymentView {
    pub deployment_id: String,
    pub artifact_hash: String,
    pub engine_version: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeploymentView {
    pub id: String,
    pub app: String,
    pub source_hash: String,
    pub engine_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact_hash: Option<String>,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AdminFault {
    pub code: AdminErrorCode,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminErrorCode {
    InvalidRequest,
    UnsupportedVersion,
    Unauthorized,
    NotFound,
    Conflict,
    Validation,
    Internal,
}

pub fn valid_request_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub fn read_frame<T: DeserializeOwned>(reader: &mut impl Read) -> io::Result<T> {
    let mut length = [0_u8; 4];
    reader.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length) as usize;
    if length == 0 || length > MAX_ADMIN_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("admin frame length {length} is outside 1..={MAX_ADMIN_FRAME_BYTES}"),
        ));
    }
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload)?;
    serde_json::from_slice(&payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn write_frame<T: Serialize>(writer: &mut impl Write, value: &T) -> io::Result<()> {
    let payload = serde_json::to_vec(value)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if payload.is_empty() || payload.len() > MAX_ADMIN_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "encoded admin frame length {} is outside 1..={MAX_ADMIN_FRAME_BYTES}",
                payload.len()
            ),
        ));
    }
    writer.write_all(&(payload.len() as u32).to_be_bytes())?;
    writer.write_all(&payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trip_preserves_tagged_command() {
        let request = AdminRequest {
            version: ADMIN_PROTOCOL_VERSION,
            actor: Some("operator@example.test".into()),
            request_id: "0123456789abcdef0123456789abcdef".into(),
            command: AdminCommand::ReadLog {
                deployment: "deploy-1".into(),
                stream: LogStream::Stderr,
                offset: 7,
                limit: 4096,
            },
        };
        let mut frame = Vec::new();
        write_frame(&mut frame, &request).unwrap();
        assert_eq!(
            read_frame::<AdminRequest>(&mut frame.as_slice()).unwrap(),
            request
        );
    }

    #[test]
    fn frame_reader_rejects_zero_and_oversized_lengths_before_allocating() {
        assert_eq!(
            read_frame::<AdminRequest>(&mut [0_u8; 4].as_slice())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        let oversized = ((MAX_ADMIN_FRAME_BYTES + 1) as u32).to_be_bytes();
        assert_eq!(
            read_frame::<AdminRequest>(&mut oversized.as_slice())
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn defaults_bound_lists_and_log_chunks() {
        let request: AdminRequest = serde_json::from_value(serde_json::json!({
            "version": 1,
            "request_id": "0123456789abcdef0123456789abcdef",
            "command": { "type": "list_deployments" }
        }))
        .unwrap();
        assert_eq!(
            request.command,
            AdminCommand::ListDeployments {
                app: None,
                cursor: None,
                limit: 50
            }
        );
        assert!(default_log_limit() <= MAX_LOG_CHUNK_BYTES);
        assert!(valid_request_id(&request.request_id));
        assert!(!valid_request_id("ABCDEF0123456789ABCDEF0123456789"));
    }
}
