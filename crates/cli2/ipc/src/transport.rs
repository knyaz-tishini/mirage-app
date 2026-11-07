//! Transport-agnostic IPC traits.
//!
//! Implemented by [`crate::pipe::PipePair`].

use std::io;

use serde::{Serialize, de::DeserializeOwned};

/// Async reader of framed JSON-line messages.
pub trait IpcReader: Send + Unpin {
    fn read_msg<T: DeserializeOwned + Send>(
        &mut self,
    ) -> impl std::future::Future<Output = io::Result<Option<T>>> + Send;
}

/// Async writer of framed JSON-line messages.
pub trait IpcWriter: Send + Unpin {
    fn write_msg<T: Serialize + Send + Sync>(
        &mut self,
        msg: &T,
    ) -> impl std::future::Future<Output = io::Result<()>> + Send;
}
