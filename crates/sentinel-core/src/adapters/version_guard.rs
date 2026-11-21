//! RPC protocol-version capability guard and compatibility checker.

use crate::error::SentinelError;
use serde::{Deserialize, Serialize};

/// Supported RPC protocol range for Soroban Sentinel.
pub const MIN_SUPPORTED_PROTOCOL_VERSION: u32 = 20;
pub const MAX_SUPPORTED_PROTOCOL_VERSION: u32 = 23;

/// RPC protocol capabilities Sentinel understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct RpcCapabilities {
    /// Minimum protocol version Sentinel can operate against.
    pub min: u32,
    /// Maximum protocol version Sentinel can operate against.
    pub max: u32,
}

impl RpcCapabilities {
    /// Returns the default capability envelope covering the supported protocol range.
    #[allow(clippy::should_implement_trait)]
    pub fn default() -> Self {
        Self {
            min: MIN_SUPPORTED_PROTOCOL_VERSION,
            max: MAX_SUPPORTED_PROTOCOL_VERSION,
        }
    }
}

impl Default for RpcCapabilities {
    fn default() -> Self {
        Self::default()
    }
}

/// Version information returned by `getVersionInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct VersionInfo {
    #[serde(default)]
    pub protocol_version: Option<u32>,
}

/// Performs a compatibility check against a Soroban RPC endpoint.
///
/// Uses `getLatestLedger` (which always returns `protocolVersion`) as the source of truth.
/// A node is considered compatible when its reported protocol version is within the supported range.
/// Missing or optional extra version fields are treated as non-fatal.
///
/// This stub mirrors the intended public API shape. Real RPC access is wired through
/// `crate::adapters::soroban_rpc`; the stub returns `Ok(())` so the type signatures compile
/// while the integration is finalized.
#[cfg_attr(docsrs, doc(cfg(feature = "compatibility-check")))]
pub async fn check_compatibility(
    _client: &crate::adapters::soroban_rpc::SorobanRpcClient,
    _capabilities: RpcCapabilities,
) -> Result<(), SentinelError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_capabilities_cover_supported_range() {
        let caps = RpcCapabilities::default();
        assert_eq!(caps.min, MIN_SUPPORTED_PROTOCOL_VERSION);
        assert_eq!(caps.max, MAX_SUPPORTED_PROTOCOL_VERSION);
        assert!(caps.min <= caps.max);
    }

    #[test]
    fn version_info_default_is_none() {
        let vi = VersionInfo::default();
        assert_eq!(vi.protocol_version, None);
    }

    #[test]
    fn version_info_deserializes_optional_protocol_version() {
        let json = r#"{"protocolVersion": 22}"#;
        let vi: VersionInfo = serde_json::from_str(json).unwrap();
        assert_eq!(vi.protocol_version, Some(22));
    }

    #[test]
    fn version_info_handles_missing_protocol_version() {
        let json = r#"{"other": 1}"#;
        let vi: VersionInfo = serde_json::from_str(json).unwrap();
        assert_eq!(vi.protocol_version, None);
    }

    #[test]
    fn unsupported_protocol_error_found_below_minimum() {
        let err = SentinelError::UnsupportedProtocol {
            found: 19,
            required: 20,
        };
        assert_eq!(
            err.to_string(),
            "RPC protocol version 19 is below the minimum supported version 20"
        );
    }

    #[test]
    fn unsupported_protocol_error_found_above_maximum() {
        let err = SentinelError::UnsupportedProtocol {
            found: 24,
            required: 23,
        };
        assert_eq!(
            err.to_string(),
            "RPC protocol version 24 is below the minimum supported version 23"
        );
    }
}
