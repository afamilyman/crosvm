// Copyright 2020 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A wrapped IO source that uses FdExecutor to drive asynchronous completion. Used from
//! `IoSourceExt::new` when uring isn't available in the kernel.

use async_trait::async_trait;
use std::ops::{Deref, DerefMut};
use std::os::unix::io::AsRawFd;
use std::sync::Arc;

use libc::O_NONBLOCK;
use sys_util::{self, add_fd_flags};
use thiserror::Error as ThisError;

use crate::fd_executor::{self, FdExecutor};
use crate::uring_mem::{BackingMemory, BorrowedIoVec, MemRegion};
use crate::{AsyncError, AsyncResult};
use crate::{IoSourceExt, ReadAsync, WriteAsync};

#[derive(ThisError, Debug)]
pub enum Error {
    /// An error occurred attempting to register a waker with the executor.
    #[error("An error occurred attempting to register a waker with the executor: {0}.")]
    AddingWaker(fd_executor::Error),
    /// An executor error occurred.
    #[error("An executor error occurred: {0}")]
    Executor(fd_executor::Error),
    /// An error occurred when executing fallocate synchronously.
    #[error("An error occurred when executing fallocate synchronously: {0}")]
    Fallocate(sys_util::Error),
    /// An error occurred when executing fsync synchronously.
    #[error("An error occurred when executing fsync synchronously: {0}")]
    Fsync(sys_util::Error),
    /// An error occurred when reading the FD.
    ///
    #[error("An error occurred when reading the FD: {0}.")]
    Read(sys_util::Error),
    /// Can't seek file.
    #[error("An error occurred when seeking the FD: {0}.")]
    Seeking(sys_util::Error),
    /// An error occurred when setting the FD non-blocking.
    #[error("An error occurred setting the FD non-blocking: {0}.")]
    SettingNonBlocking(sys_util::Error),
    /// An error occurred when writing the FD.
    #[error("An error occurred when writing the FD: {0}.")]
    Write(sys_util::Error),
}
pub type Result<T> = std::result::Result<T, Error>;

/// Async wrapper for an IO source that uses the FD executor to drive async operations.
/// Used by `IoSourceExt::new` when uring isn't available.
pub struct PollSource<F: AsRawFd> {
    source: F,
    ex: FdExecutor,
}

impl<F: AsRawFd> PollSource<F> {
    /// Create a new `PollSource` from the given IO source.
    pub fn new(f: F, ex: &FdExecutor) -> Result<Self> {
        let fd = f.as_raw_fd();
        add_fd_flags(fd, O_NONBLOCK).map_err(Error::SettingNonBlocking)?;
        Ok(Self {
            source: f,
            ex: ex.clone(),
        })
    }

    /// Return the inner source.
    pub fn into_source(self) -> F {
        self.source
    }
}

impl<F: AsRawFd> Deref for PollSource<F> {
    type Target = F;

    fn deref(&self) -> &Self::Target {
        &self.source
    }
}

#[async_trait(?Send)]
impl<F: AsRawFd> ReadAsync for PollSource<F> {
    /// Reads from the iosource at `file_offset` and fill the given `vec`.
    async fn read_to_vec<'a>(
        &'a self,
        file_offset: u64,
        mut vec: Vec<u8>,
    ) -> AsyncResult<(usize, Vec<u8>)> {
        loop {
            // Safe because this will only modify `vec` and we check the return value.
            let res = unsafe {
                libc::pread64(
                    self.source.as_raw_fd(),
                    vec.as_mut_ptr() as *mut libc::c_void,
                    vec.len(),
                    file_offset as libc::off64_t,
                )
            };

            if res >= 0 {
                return Ok((res as usize, vec));
            }

            match sys_util::Error::last() {
                e if e.errno() == libc::EWOULDBLOCK => {
                    let op = self
                        .ex
                        .wait_readable(&self.source)
                        .map_err(Error::AddingWaker)?;
                    op.await.map_err(Error::Executor)?;
                }
                e => return Err(Error::Read(e).into()),
            }
        }
    }

    /// Reads to the given `mem` at the given offsets from the file starting at `file_offset`.
    async fn read_to_mem<'a>(
        &'a self,
        file_offset: u64,
        mem: Arc<dyn BackingMemory + Send + Sync>,
        mem_offsets: &'a [MemRegion],
    ) -> AsyncResult<usize> {
        let mut iovecs = mem_offsets
            .iter()
            .filter_map(|&mem_vec| mem.get_iovec(mem_vec).ok())
            .collect::<Vec<BorrowedIoVec>>();

        loop {
            // Safe because we trust the kernel not to write path the length given and the length is
            // guaranteed to be valid from the pointer by io_slice_mut.
            let res = unsafe {
                libc::preadv64(
                    self.source.as_raw_fd(),
                    iovecs.as_mut_ptr() as *mut _,
                    iovecs.len() as i32,
                    file_offset as libc::off64_t,
                )
            };

            if res >= 0 {
                return Ok(res as usize);
            }

            match sys_util::Error::last() {
                e if e.errno() == libc::EWOULDBLOCK => {
                    let op = self
                        .ex
                        .wait_readable(&self.source)
                        .map_err(Error::AddingWaker)?;
                    op.await.map_err(Error::Executor)?;
                }
                e => return Err(Error::Read(e).into()),
            }
        }
    }

    /// Wait for the FD of `self` to be readable.
    async fn wait_readable(&self) -> AsyncResult<()> {
        let op = self
            .ex
            .wait_readable(&self.source)
            .map_err(Error::AddingWaker)?;
        op.await.map_err(Error::Executor)?;
        Ok(())
    }

    async fn read_u64(&self) -> AsyncResult<u64> {
        let mut buf = 0u64.to_ne_bytes();
        loop {
            // Safe because this will only modify `buf` and we check the return value.
            let res = unsafe {
                libc::read(
                    self.source.as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                )
            };

            if res >= 0 {
                return Ok(u64::from_ne_bytes(buf));
            }

            match sys_util::Error::last() {
                e if e.errno() == libc::EWOULDBLOCK => {
                    let op = self
                        .ex
                        .wait_readable(&self.source)
                        .map_err(Error::AddingWaker)?;
                    op.await.map_err(Error::Executor)?;
                }
                e => return Err(Error::Read(e).into()),
            }
        }
    }
}

#[async_trait(?Send)]
impl<F: AsRawFd> WriteAsync for PollSource<F> {
    /// Writes from the given `vec` to the file starting at `file_offset`.
    async fn write_from_vec<'a>(
        &'a self,
        file_offset: u64,
        vec: Vec<u8>,
    ) -> AsyncResult<(usize, Vec<u8>)> {
        loop {
            // Safe because this will not modify any memory and we check the return value.
            let res = unsafe {
                libc::pwrite64(
                    self.source.as_raw_fd(),
                    vec.as_ptr() as *const libc::c_void,
                    vec.len(),
                    file_offset as libc::off64_t,
                )
            };

            if res >= 0 {
                return Ok((res as usize, vec));
            }

            match sys_util::Error::last() {
                e if e.errno() == libc::EWOULDBLOCK => {
                    let op = self
                        .ex
                        .wait_writable(&self.source)
                        .map_err(Error::AddingWaker)?;
                    op.await.map_err(Error::Executor)?;
                }
                e => return Err(Error::Write(e).into()),
            }
        }
    }

    /// Writes from the given `mem` from the given offsets to the file starting at `file_offset`.
    async fn write_from_mem<'a>(
        &'a self,
        file_offset: u64,
        mem: Arc<dyn BackingMemory + Send + Sync>,
        mem_offsets: &'a [MemRegion],
    ) -> AsyncResult<usize> {
        let iovecs = mem_offsets
            .iter()
            .map(|&mem_vec| mem.get_iovec(mem_vec))
            .filter_map(|r| r.ok())
            .collect::<Vec<BorrowedIoVec>>();

        loop {
            // Safe because we trust the kernel not to write path the length given and the length is
            // guaranteed to be valid from the pointer by io_slice_mut.
            let res = unsafe {
                libc::pwritev64(
                    self.source.as_raw_fd(),
                    iovecs.as_ptr() as *mut _,
                    iovecs.len() as i32,
                    file_offset as libc::off64_t,
                )
            };

            if res >= 0 {
                return Ok(res as usize);
            }

            match sys_util::Error::last() {
                e if e.errno() == libc::EWOULDBLOCK => {
                    let op = self
                        .ex
                        .wait_writable(&self.source)
                        .map_err(Error::AddingWaker)?;
                    op.await.map_err(Error::Executor)?;
                }
                e => return Err(Error::Write(e).into()),
            }
        }
    }

    /// See `fallocate(2)` for details.
    async fn fallocate(&self, file_offset: u64, len: u64, mode: u32) -> AsyncResult<()> {
        let ret = unsafe {
            libc::fallocate64(
                self.source.as_raw_fd(),
                mode as libc::c_int,
                file_offset as libc::off64_t,
                len as libc::off64_t,
            )
        };
        if ret == 0 {
            Ok(())
        } else {
            Err(AsyncError::Poll(Error::Fallocate(sys_util::Error::last())))
        }
    }

    /// Sync all completed write operations to the backing storage.
    async fn fsync(&self) -> AsyncResult<()> {
        let ret = unsafe { libc::fsync(self.source.as_raw_fd()) };
        if ret == 0 {
            Ok(())
        } else {
            Err(AsyncError::Poll(Error::Fsync(sys_util::Error::last())))
        }
    }
}

#[async_trait(?Send)]
impl<F: AsRawFd> IoSourceExt<F> for PollSource<F> {
    /// Yields the underlying IO source.
    fn into_source(self: Box<Self>) -> F {
        self.source
    }

    /// Provides a mutable ref to the underlying IO source.
    fn as_source_mut(&mut self) -> &mut F {
        &mut self.source
    }

    /// Provides a ref to the underlying IO source.
    fn as_source(&self) -> &F {
        &self.source
    }
}

impl<F: AsRawFd> DerefMut for PollSource<F> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.source
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{File, OpenOptions};
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn readvec() {
        async fn go(ex: &FdExecutor) {
            let f = File::open("/dev/zero").unwrap();
            let async_source = PollSource::new(f, ex).unwrap();
            let v = vec![0x55u8; 32];
            let v_ptr = v.as_ptr();
            let ret = async_source.read_to_vec(0, v).await.unwrap();
            assert_eq!(ret.0, 32);
            let ret_v = ret.1;
            assert_eq!(v_ptr, ret_v.as_ptr());
            assert!(ret_v.iter().all(|&b| b == 0));
        }

        let ex = FdExecutor::new().unwrap();
        ex.run_until(go(&ex)).unwrap();
    }

    #[test]
    fn writevec() {
        async fn go(ex: &FdExecutor) {
            let f = OpenOptions::new().write(true).open("/dev/null").unwrap();
            let async_source = PollSource::new(f, ex).unwrap();
            let v = vec![0x55u8; 32];
            let v_ptr = v.as_ptr();
            let ret = async_source.write_from_vec(0, v).await.unwrap();
            assert_eq!(ret.0, 32);
            let ret_v = ret.1;
            assert_eq!(v_ptr, ret_v.as_ptr());
        }

        let ex = FdExecutor::new().unwrap();
        ex.run_until(go(&ex)).unwrap();
    }

    #[test]
    fn fallocate() {
        async fn go(ex: &FdExecutor) {
            let dir = tempfile::TempDir::new().unwrap();
            let mut file_path = PathBuf::from(dir.path());
            file_path.push("test");

            let f = OpenOptions::new()
                .create(true)
                .write(true)
                .open(&file_path)
                .unwrap();
            let source = PollSource::new(f, ex).unwrap();
            source.fallocate(0, 4096, 0).await.unwrap();

            let meta_data = std::fs::metadata(&file_path).unwrap();
            assert_eq!(meta_data.len(), 4096);
        }

        let ex = FdExecutor::new().unwrap();
        ex.run_until(go(&ex)).unwrap();
    }
}
