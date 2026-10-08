//! Server identity and readiness ([`ServerInfo`]).
//!
//! `GET /api/info` is the version gate: it is a v2-only route. A v1 server
//! answers with a 404 (or a non-JSON web page), which is how [`Client::probe`]
//! distinguishes "v1" from "v2 and healthy".

use serde::{Deserialize, Serialize};

use crate::error::OcError;

/// Identity, addresses, and capabilities of a running `opencode serve`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
pub struct ServerInfo {
    /// The opencode version, e.g. `"2.0.24"`.
    #[serde(default)]
    pub version: String,
    /// The server process id.
    #[serde(default)]
    pub pid: Option<u64>,
    /// Connection URLs the server advertises.
    #[serde(default)]
    pub urls: Vec<String>,
    /// Server-side path configuration.
    #[serde(default)]
    pub paths: Option<ServerPaths>,
    /// Optional feature capabilities.
    #[serde(default)]
    pub capabilities: Option<ServerCapabilities>,
}

impl ServerInfo {
    /// Whether the reported version is an opencode v2 release.
    ///
    /// The check is deliberately structural (major version) rather than an
    /// exact string so that patch releases do not require a client change.
    pub fn is_v2(&self) -> bool {
        self.version
            .split_once('.')
            .is_some_and(|(major, _)| major == "2")
    }
}

/// The `paths` object reported by `/api/info`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ServerPaths {
    /// The server's temporary/working directory.
    #[serde(default)]
    pub tmp: String,
}

/// The `capabilities` object reported by `/api/info`.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ServerCapabilities {
    /// Whether persistent PTY sessions are supported.
    #[serde(default)]
    pub persistent_pty: Option<bool>,
}

/// Validate that a probed server is v2, returning [`OcError::UnsupportedServerVersion`]
/// otherwise.
pub fn ensure_supported(info: &ServerInfo) -> Result<(), OcError> {
    if info.is_v2() {
        Ok(())
    } else {
        Err(OcError::UnsupportedServerVersion {
            found: info.version.clone(),
        })
    }
}

use super::Client;
use super::paths;

impl Client {
    /// Fetch the server identity (`GET /api/info`).
    ///
    /// A 404/405 here is the v1 signature and is reported as
    /// [`OcError::MissingInfoEndpoint`] (spec AC-16).
    pub async fn info(&self) -> Result<ServerInfo, OcError> {
        let request = self
            .http
            .get(self.url(paths::INFO)?)
            .basic_auth(crate::AUTH_USERNAME, Some(&self.password));
        let response = request.send().await?;
        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND
            || status == reqwest::StatusCode::METHOD_NOT_ALLOWED
        {
            return Err(OcError::MissingInfoEndpoint {
                status: status.as_u16(),
            });
        }
        self.decode_success(response).await
    }

    /// Probe the server and confirm it is opencode v2 (FR-40). This is the
    /// startup gate every higher layer should call once.
    pub async fn probe(&self) -> Result<ServerInfo, OcError> {
        let info = self.info().await?;
        ensure_supported(&info)?;
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use super::{ServerInfo, ensure_supported};
    use crate::error::OcError;

    #[test]
    fn v2_version_is_supported() {
        let info = ServerInfo {
            version: "2.0.24".to_owned(),
            ..ServerInfo::default()
        };
        assert!(ensure_supported(&info).is_ok());
    }

    #[test]
    fn v1_version_is_refused() {
        let info = ServerInfo {
            version: "1.5.2".to_owned(),
            ..ServerInfo::default()
        };
        assert!(matches!(
            ensure_supported(&info),
            Err(OcError::UnsupportedServerVersion { found }) if found == "1.5.2"
        ));
    }

    #[test]
    fn empty_version_means_not_v2() {
        let info = ServerInfo::default();
        assert!(ensure_supported(&info).is_err());
    }
}
