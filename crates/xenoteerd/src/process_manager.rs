//! IPC boundary to the separately supervised application process broker.
//!
//! This module deliberately contains no process-spawn primitive. The daemon's
//! UID can only request operations through the peer-credential-authenticated
//! Unix socket owned by `xenoteer-processd`.

#[allow(unused_imports)]
pub(crate) use xenoteer_processd::{BrokerClient, BrokerClientError};
