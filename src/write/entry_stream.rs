// Copyright (c) 2021 Harry [Majored] [hello@majored.pw]
// MIT License (https://github.com/Majored/rs-async-zip/blob/main/LICENSE)

use crate::error::Result;
use crate::spec::header::{CentralDirectoryHeader, GeneralPurposeFlag, LocalFileHeader};
use crate::write::compressed_writer::CompressedAsyncWriter;
use crate::write::offset_writer::OffsetAsyncWriter;
use crate::write::CentralDirectoryEntry;
use crate::write::{EntryOptions, ZipFileWriter};

use std::io::Error;
use std::pin::Pin;
use std::task::{Context, Poll};

use chrono::Utc;
use crc32fast::Hasher;
use tokio::io::{AsyncWrite, AsyncWriteExt};

/// An entry writer which supports the streaming of data (ie. the writing of unknown size or data at runtime).
///
/// # Note
/// - This writer cannot be manually constructed; instead, use [`ZipFileWriter::write_entry_stream()`].
/// - [`EntryStreamWriter::close()`] must be called before a stream writer goes out of scope.
/// - Utilities for working with [`AsyncWrite`] values are provided by [`AsyncWriteExt`].
pub struct EntryStreamWriter<'a, 'b, W: AsyncWrite + Unpin> {
    writer: OffsetAsyncWriter<CompressedAsyncWriter<'b, &'a mut W>>,
    cd_entries: &'b mut Vec<CentralDirectoryEntry>,
    options: EntryOptions,
    hasher: Hasher,
    lfh: LocalFileHeader,
    lfh_offset: usize,
    data_offset: usize,
}

impl<'a, 'b, W: AsyncWrite + Unpin> EntryStreamWriter<'a, 'b, W> {
    pub(crate) async fn from_raw(
        writer: &'b mut ZipFileWriter<'a, W>,
        options: EntryOptions,
    ) -> Result<EntryStreamWriter<'a, 'b, W>> {
        let lfh_offset = writer.writer.offset();
        let lfh = EntryStreamWriter::write_lfh(writer, &options).await?;
        let data_offset = writer.writer.offset();

        let cd_entries = &mut writer.cd_entries;
        let writer =
            OffsetAsyncWriter::from_raw(CompressedAsyncWriter::from_raw(&mut writer.writer, options.compression));

        Ok(EntryStreamWriter { writer, cd_entries, options, lfh, lfh_offset, data_offset, hasher: Hasher::new() })
    }

    async fn write_lfh(writer: &'b mut ZipFileWriter<'a, W>, options: &EntryOptions) -> Result<LocalFileHeader> {
        let (mod_time, mod_date) = crate::spec::date::chrono_to_zip_time(&Utc::now());

        let lfh = LocalFileHeader {
            compressed_size: 0,
            uncompressed_size: 0,
            compression: options.compression.to_u16(),
            crc: 0,
            extra_field_length: options.extra.len() as u16,
            file_name_length: options.filename.as_bytes().len() as u16,
            mod_time,
            mod_date,
            version: 0,
            flags: GeneralPurposeFlag { data_descriptor: true, encrypted: false },
        };

        writer.writer.write_all(&crate::spec::delimiter::LFHD.to_le_bytes()).await?;
        writer.writer.write_all(&lfh.to_slice()).await?;
        writer.writer.write_all(options.filename.as_bytes()).await?;
        writer.writer.write_all(&options.extra).await?;

        Ok(lfh)
    }

    /// Consumes this entry writer and completes all closing tasks.
    ///
    /// This includes:
    /// - Finalising the CRC32 hash value for the written data.
    /// - Calculating the compressed and uncompressed byte sizes.
    /// - Constructing a central directory header.
    /// - Pushing that central directory header to the [`ZipFileWriter`]'s store.
    ///
    /// Failiure to call this function before going out of scope would result in a corrupted ZIP file.
    pub async fn close(mut self) -> Result<()> {
        self.writer.shutdown().await?;

        let crc = self.hasher.finalize();
        let uncompressed_size = self.writer.offset() as u32;
        let inner_writer = self.writer.into_inner().into_inner();
        let compressed_size = (inner_writer.offset() - self.data_offset) as u32;

        inner_writer.write_all(&crate::spec::delimiter::DDD.to_le_bytes()).await?;
        inner_writer.write_all(&crc.to_le_bytes()).await?;
        inner_writer.write_all(&compressed_size.to_le_bytes()).await?;
        inner_writer.write_all(&uncompressed_size.to_le_bytes()).await?;

        let cdh = CentralDirectoryHeader {
            compressed_size,
            uncompressed_size,
            crc,
            v_made_by: 0,
            v_needed: 0,
            compression: self.lfh.compression,
            extra_field_length: self.lfh.extra_field_length,
            file_name_length: self.lfh.file_name_length,
            file_comment_length: self.options.comment.len() as u16,
            mod_time: self.lfh.mod_time,
            mod_date: self.lfh.mod_date,
            flags: self.lfh.flags,
            disk_start: 0,
            inter_attr: 0,
            exter_attr: 0,
            lh_offset: self.lfh_offset as u32,
        };

        self.cd_entries.push(CentralDirectoryEntry { header: cdh, opts: self.options });
        Ok(())
    }
}

impl<'a, 'b, W: AsyncWrite + Unpin> AsyncWrite for EntryStreamWriter<'a, 'b, W> {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context, buf: &[u8]) -> Poll<std::result::Result<usize, Error>> {
        let poll = Pin::new(&mut self.writer).poll_write(cx, buf);

        if let Poll::Ready(Ok(written)) = poll {
            self.hasher.update(&buf[0..written]);
        }

        poll
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<std::result::Result<(), Error>> {
        Pin::new(&mut self.writer).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<std::result::Result<(), Error>> {
        Pin::new(&mut self.writer).poll_shutdown(cx)
    }
}
