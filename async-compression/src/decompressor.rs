// Copyright (c) 2004-present, Facebook, Inc.
// All Rights Reserved.
//
// This software may be used and distributed according to the terms of the
// GNU General Public License version 2 or any later version.

//! Non-blocking, buffered compression and decompression

use std::fmt::{self, Debug, Formatter};
use std::io::{self, BufRead, Read};

use bzip2::bufread::BzDecoder;
use flate2::bufread::GzDecoder;
use tokio_io::AsyncRead;

use raw::RawDecoder;

pub struct Decompressor<'a, R>
where
    R: AsyncRead + BufRead + 'a + Send,
{
    d_type: DecompressorType,
    inner: Box<RawDecoder<R> + 'a + Send>,
}

#[derive(Clone, Copy, Debug)]
pub enum DecompressorType {
    Bzip2,
    Gzip,
    Zstd,
}

impl<'a, R> Decompressor<'a, R>
where
    R: AsyncRead + BufRead + 'a + Send,
{
    pub fn new(r: R, dt: DecompressorType) -> Self {
        Decompressor {
            d_type: dt,
            inner: match dt {
                DecompressorType::Bzip2 => Box::new(BzDecoder::new(r)),
                DecompressorType::Gzip => Box::new(GzDecoder::new(r)),
                // TODO: The zstd crate is not safe for decompressing Read input, because it is
                // overconsuming it
                DecompressorType::Zstd => unimplemented!(),
            },
        }
    }

    #[inline]
    pub fn get_ref(&self) -> &R {
        self.inner.get_ref()
    }

    #[inline]
    pub fn get_mut(&mut self) -> &mut R {
        self.inner.get_mut()
    }

    #[inline]
    pub fn into_inner(self) -> R {
        self.inner.into_inner()
    }
}

impl<'a, R: AsyncRead + BufRead + 'a + Send> Read for Decompressor<'a, R> {
    #[inline]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<'a, R: AsyncRead + BufRead + 'a + Send> AsyncRead for Decompressor<'a, R> {}

impl<'a, R: AsyncRead + BufRead + 'a + Send> Debug for Decompressor<'a, R> {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.debug_struct("Decompressor")
            .field("decoder_type", &self.d_type)
            .finish()
    }
}
