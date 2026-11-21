//! Integration tests for the RPC protocol-version capability guard.

use sentinel_core::adapters::version_guard::{
    RpcCapabilities, VersionInfo, MAX_SUPPORTED_PROTOCOL_VERSION, MIN_SUPPORTED_PROTOCOL_VERSION,
};

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
fn default_capabilities_and_version_info() {
    let caps = RpcCapabilities::default();
    assert_eq!(caps.min, MIN_SUPPORTED_PROTOCOL_VERSION);
    assert_eq!(caps.max, MAX_SUPPORTED_PROTOCOL_VERSION);
    let vi = VersionInfo::default();
    assert_eq!(vi.protocol_version, None);
}
