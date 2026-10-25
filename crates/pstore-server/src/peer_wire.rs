//! Bounded MessagePack frames for encrypted HTTP peer bodies.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::io::Write;
use std::sync::Arc;
#[cfg(test)]
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(crate) const MAX_FRAME: usize = 32 * 1024 * 1024;
pub(crate) const WIRE_BUDGET: usize = 64 * 1024 * 1024;
pub(crate) const VERSION: u16 = 1;

/// Neither framing errors nor their Debug form contain request data.
#[derive(Debug, thiserror::Error)]
pub(crate) enum WireError {
    #[error("peer I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid or oversized peer frame")]
    InvalidFrame,
    #[error("peer transport is closed")]
    Closed,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request<B = serde_json::Value> {
    pub(crate) version: u16,
    pub(crate) id: u64,
    pub(crate) tenant: String,
    pub(crate) body: B,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reply<B = serde_json::Value> {
    pub(crate) version: u16,
    pub(crate) id: u64,
    pub(crate) status: u16,
    pub(crate) body: B,
}

/// Retain admission until dispatch finishes using the decoded message.
pub(crate) struct Frame {
    bytes: Vec<u8>,
    _permit: OwnedSemaphorePermit,
}

/// Admits a single frame split across HTTP DATA chunks. No Content-Length trust.
#[derive(Default)]
pub(crate) struct Accumulator {
    prefix: Vec<u8>,
    bytes: Vec<u8>,
    size: Option<usize>,
    permit: Option<OwnedSemaphorePermit>,
}

impl Accumulator {
    pub(crate) async fn push(
        &mut self,
        mut chunk: &[u8],
        budget: Arc<Semaphore>,
    ) -> Result<(), WireError> {
        if self.size.is_none() {
            let count = (4 - self.prefix.len()).min(chunk.len());
            let (prefix, rest) = chunk.split_at(count);
            self.prefix.extend_from_slice(prefix);
            chunk = rest;
            if self.prefix.len() < 4 {
                return Ok(());
            }
            let prefix: [u8; 4] = self
                .prefix
                .as_slice()
                .try_into()
                .map_err(|_| WireError::InvalidFrame)?;
            let size = u32::from_be_bytes(prefix);
            if size == 0 || size as usize > MAX_FRAME {
                return Err(WireError::InvalidFrame);
            }
            self.permit = Some(
                budget
                    .acquire_many_owned(size)
                    .await
                    .map_err(|_| WireError::Closed)?,
            );
            self.size = Some(size as usize);
            self.bytes.reserve_exact(size as usize);
        }
        if chunk.len()
            > self
                .size
                .unwrap_or_default()
                .saturating_sub(self.bytes.len())
        {
            return Err(WireError::InvalidFrame);
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    pub(crate) fn finish(self) -> Result<Frame, WireError> {
        if self.size != Some(self.bytes.len()) {
            return Err(WireError::InvalidFrame);
        }
        Ok(Frame {
            bytes: self.bytes,
            _permit: self.permit.ok_or(WireError::InvalidFrame)?,
        })
    }
}

impl Frame {
    pub(crate) fn decode<T: DeserializeOwned>(&self) -> Result<T, WireError> {
        let mut decoder = rmp_serde::Deserializer::new(std::io::Cursor::new(&self.bytes));
        decoder.set_max_depth(64);
        let value = T::deserialize(&mut decoder).map_err(|_| WireError::InvalidFrame)?;
        if decoder.position() != self.bytes.len() as u64 {
            return Err(WireError::InvalidFrame);
        }
        Ok(value)
    }
}

/// Read the length before allocating, then admit bytes before reading the payload.
#[cfg(test)]
pub(crate) async fn read_frame<R: AsyncRead + Unpin>(
    reader: &mut R,
    budget: Arc<Semaphore>,
) -> Result<Option<Frame>, WireError> {
    let mut first = [0];
    if reader.read(&mut first).await? == 0 {
        return Ok(None);
    }
    let mut rest = [0; 3];
    reader.read_exact(&mut rest).await?;
    let [a] = first;
    let [b, c, d] = rest;
    let size = u32::from_be_bytes([a, b, c, d]);
    if size == 0 || size as usize > MAX_FRAME {
        return Err(WireError::InvalidFrame);
    }
    let permit = budget
        .acquire_many_owned(size)
        .await
        .map_err(|_| WireError::Closed)?;
    let mut bytes = vec![0; size as usize];
    reader.read_exact(&mut bytes).await?;
    Ok(Some(Frame {
        bytes,
        _permit: permit,
    }))
}

struct Bounded(Vec<u8>);

impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > (MAX_FRAME + 4).saturating_sub(self.0.len()) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "oversized peer frame",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Serialize a complete frame once, including its length, for one TLS write.
pub(crate) fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, WireError> {
    let mut output = Bounded(vec![0; 4]);
    value
        .serialize(&mut rmp_serde::Serializer::new(&mut output).with_struct_map())
        .map_err(|_| WireError::InvalidFrame)?;
    let size =
        u32::try_from(output.0.len().saturating_sub(4)).map_err(|_| WireError::InvalidFrame)?;
    output
        .0
        .get_mut(..4)
        .ok_or(WireError::InvalidFrame)?
        .copy_from_slice(&size.to_be_bytes());
    Ok(output.0)
}

#[cfg(test)]
mod tests;
