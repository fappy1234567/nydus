// Copyright 2020 Ant Group. All rights reserved.
// Copyright (C) 2021 Alibaba Cloud. All rights reserved.
//
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;

use nydus_utils::metrics::{BackendMetrics, ERROR_HOLDER};
use vm_memory::VolatileSlice;

use crate::utils::copyv;
use crate::StorageError;

#[cfg(feature = "backend-localfs")]
pub mod localfs;
#[cfg(feature = "backend-oss")]
pub mod oss;
#[cfg(feature = "backend-registry")]
pub mod registry;
#[cfg(any(feature = "backend-oss", feature = "backend-registry"))]
pub mod request;

/// Error codes related to storage backend operations.
#[derive(Debug)]
pub enum BackendError {
    /// Unsupported operation.
    Unsupported(String),
    /// Failed to copy data from/into blob.
    CopyData(StorageError),
    #[cfg(feature = "backend-registry")]
    /// Error from Registry storage backend.
    Registry(self::registry::RegistryError),
    #[cfg(feature = "backend-localfs")]
    /// Error from LocalFs storage backend.
    LocalFs(self::localfs::LocalFsError),
    #[cfg(feature = "backend-oss")]
    /// Error from OSS storage backend.
    Oss(self::oss::OssError),
}

/// Specialized `Result` for storage backends.
pub type BackendResult<T> = std::result::Result<T, BackendError>;

/// Configuration information for network proxy.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ProxyConfig {
    url: String,
    ping_url: String,
    fallback: bool,
    check_interval: u64,
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            ping_url: String::new(),
            fallback: true,
            check_interval: 5,
        }
    }
}

/// Generic configuration for storage backends.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CommonConfig {
    proxy: ProxyConfig,
    timeout: u64,
    connect_timeout: u64,
    retry_limit: u8,
}

impl Default for CommonConfig {
    fn default() -> Self {
        Self {
            proxy: ProxyConfig::default(),
            timeout: 5,
            connect_timeout: 5,
            retry_limit: 0,
        }
    }
}

/// Trait to read data from a blob file on storage backends.
pub trait BlobReader: Send + Sync {
    /// Get size of the blob file.
    fn blob_size(&self) -> BackendResult<u64>;

    /// Try to read a range of data from the blob file into the provided buffer.
    ///
    /// Try to read data of range [offset, offset + buf.len()) from the blob file, and returns:
    /// - bytes of data read, which may be smaller than buf.len()
    /// - error code if error happens
    fn try_read(&self, buf: &mut [u8], offset: u64) -> BackendResult<usize>;

    /// Read a range of data from the blob file into the provided buffer.
    ///
    /// Read data of range [offset, offset + buf.len()) from the blob file, and returns:
    /// - bytes of data read, which may be smaller than buf.len()
    /// - error code if error happens
    ///
    /// It will try `BlobBackend::retry_limit()` times at most and return the first successfully
    /// read data.
    fn read(&self, buf: &mut [u8], offset: u64) -> BackendResult<usize> {
        let mut retry_count = self.retry_limit();
        let begin_time = self.metrics().begin();

        loop {
            match self.try_read(buf, offset) {
                Ok(size) => {
                    self.metrics().end(&begin_time, buf.len(), false);
                    return Ok(size);
                }
                Err(err) => {
                    if retry_count > 0 {
                        warn!(
                            "Read from backend failed: {:?}, retry count {}",
                            err, retry_count
                        );
                        retry_count -= 1;
                    } else {
                        self.metrics().end(&begin_time, buf.len(), true);
                        ERROR_HOLDER
                            .lock()
                            .unwrap()
                            .push(&format!("{:?}", err))
                            .unwrap_or_else(|_| error!("Failed when try to hold error"));
                        return Err(err);
                    }
                }
            }
        }
    }

    /// Read a range of data from the blob file into the provided buffers.
    ///
    /// Read data of range [offset, offset + max_size) from the blob file, and returns:
    /// - bytes of data read, which may be smaller than max_size
    /// - error code if error happens
    ///
    /// It will try `BlobBackend::retry_limit()` times at most and return the first successfully
    /// read data.
    fn readv(&self, bufs: &[VolatileSlice], offset: u64, max_size: usize) -> BackendResult<usize> {
        if bufs.len() == 1 && max_size >= bufs[0].len() {
            let buf = unsafe { std::slice::from_raw_parts_mut(bufs[0].as_ptr(), bufs[0].len()) };
            self.read(buf, offset)
        } else {
            // Use std::alloc to avoid zeroing the allocated buffer.
            let size = bufs.iter().fold(0usize, move |size, s| size + s.len());
            let size = std::cmp::min(size, max_size);
            let mut data = Vec::with_capacity(size);
            unsafe { data.set_len(size) };

            self.read(blob_id, data, offset)?;
            copyv(&[&data], bufs, offset, result, 0, 0)
                .map(|r| r.0)
                .map_err(BackendError::CopyData)
        }
    }

    /// Give hints to prefetch blob data range.
    fn prefetch_blob_data_range(&self, ra_offset: u32, ra_size: u32) -> BackendResult<()>;

    /// Get metrics object.
    fn metrics(&self) -> &BackendMetrics;

    /// Get maximum number of times to retry when encountering IO errors.
    fn retry_limit(&self) -> u8 {
        0
    }
}

pub trait BlobWrite: Send + Sync {
    /// Get maximum number of times to retry when encountering IO errors.
    fn retry_limit(&self) -> u8 {
        0
    }

    /// Write data from buffer into the blob file.
    fn write(&self, buf: &[u8], offset: u64) -> BackendResult<usize>;
}

/// Trait to access blob files on backend storages, such as OSS, registry, local fs etc.
pub trait BlobBackend: Send + Sync {
    /// Destroy the `BlobBackend` storage object.
    fn release(&self);

    /// Get metrics object.
    fn metrics(&self) -> &BackendMetrics;

    /// Get a blob reader object to access blod `blob_id`.
    fn get_reader(&self, blob_id: &str) -> BackendResult<Arc<dyn BlobReader>>;

    /// Get a blob writer object to access blod `blob_id`.
    fn get_writer(&self, blob_id: &str) -> BackendResult<Arc<dyn BlobWrite>>;

    /// Get size of the blob file.
    fn blob_size(&self, blob_id: &str) -> BackendResult<u64> {
        self.get_reader(blob_id)?.blob_size()
    }

    /// Try to read a range of data from the blob file into the provided buffer.
    ///
    /// Try to read data of range [offset, offset + buf.len()) from the blob file, and returns:
    /// - bytes of data read, which may be smaller than buf.len()
    /// - error code if error happens
    fn try_read(&self, blob_id: &str, buf: &mut [u8], offset: u64) -> BackendResult<usize> {
        self.get_reader(blob_id)?.try_read(buf, offset)
    }

    /// Read a range of data from the blob file into the provided buffer.
    ///
    /// Read data of range [offset, offset + buf.len()) from the blob file, and returns:
    /// - bytes of data read, which may be smaller than buf.len()
    /// - error code if error happens
    ///
    /// It will try `BlobBackend::retry_limit()` times at most and return the first successfully
    /// read data.
    fn read(&self, blob_id: &str, buf: &mut [u8], offset: u64) -> BackendResult<usize> {
        self.get_reader(blob_id)?.read(buf, offset)
    }

    /// Read a range of data from the blob file into the provided buffers.
    ///
    /// Read data of range [offset, offset + max_size) from the blob file, and returns:
    /// - bytes of data read, which may be smaller than max_size
    /// - error code if error happens
    ///
    /// It will try `BlobBackend::retry_limit()` times at most and return the first successfully
    /// read data.
    fn readv(
        &self,
        blob_id: &str,
        bufs: &[VolatileSlice],
        offset: u64,
        max_size: usize,
    ) -> BackendResult<usize> {
        self.get_reader(blob_id)?.readv(bufs, offset, max_size)
    }

    /// Give hints to prefetch blob data range.
    fn prefetch_blob_data_range(
        &self,
        blob_id: &str,
        ra_offset: u32,
        ra_size: u32,
    ) -> BackendResult<()>;

    /// Write data from buffer into the blob file.
    fn write(&self, blob_id: &str, buf: &[u8], offset: u64) -> BackendResult<usize> {
        self.get_writer(blob_id)?.write(buf, offset)
    }
}

#[cfg(any(feature = "backend-oss", feature = "backend-registry"))]
/// Get default http scheme for network connection.
fn default_http_scheme() -> String {
    "https".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(any(feature = "backend-oss", feature = "backend-registry"))]
    #[test]
    fn test_default_http_scheme() {
        assert_eq!(default_http_scheme(), "https");
    }

    #[test]
    fn test_common_config() {
        let config = CommonConfig::default();

        assert_eq!(config.timeout, 5);
        assert_eq!(config.connect_timeout, 5);
        assert_eq!(config.retry_limit, 0);
        assert_eq!(config.proxy.check_interval, 5);
        assert_eq!(config.proxy.fallback, true);
        assert_eq!(config.proxy.ping_url, "");
        assert_eq!(config.proxy.url, "");
    }
}
