//! Bounded traversal of the existing SBCAP001/REC1 format.
use super::*;

pub(super) const IO_CHUNK_BYTES: usize = 64 * 1024;
const MAX_METADATA_BYTES: usize = 1024 * 1024;

struct VerifiedBody<R> {
    inner: R,
    hash: Sha256,
    bytes: u64,
}

impl<R: Read> Read for VerifiedBody<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.hash.update(&buffer[..count]);
        self.bytes += count as u64;
        Ok(count)
    }
}

/// The visitor must treat any output as unverified until this function succeeds.
/// Bodies are hashed and drained in bounded chunks, including unselected frames.
pub(super) fn visit_segment(
    path: &Path,
    recover_tail: bool,
    mut visitor: impl FnMut(&BodyRecord, &mut dyn Read) -> Result<()>,
) -> Result<()> {
    let mut options = OpenOptions::new();
    options.read(true).write(recover_tail);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let mut file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(BodyLogError::new("capture segment is not a regular file"));
    }
    if recover_tail {
        set_private_file(path)?;
    }
    let file_len = file.metadata()?.len();
    if file_len < CAPTURE_SEGMENT_MAGIC.len() as u64 {
        return Err(BodyLogError::new("capture segment header incomplete"));
    }
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic)?;
    if &magic != CAPTURE_SEGMENT_MAGIC {
        return Err(BodyLogError::new("capture segment magic mismatch"));
    }
    let mut buffer = [0u8; IO_CHUNK_BYTES];
    loop {
        let frame_start = file.stream_position()?;
        if frame_start == file_len {
            return Ok(());
        }
        if file_len.saturating_sub(frame_start) < CAPTURE_RECORD_HEADER_BYTES {
            if recover_tail {
                truncate_partial_segment_tail(&file, frame_start)?;
                return Ok(());
            }
            return Err(BodyLogError::new(
                "capture segment has incomplete record header",
            ));
        }
        let mut magic = [0u8; 4];
        file.read_exact(&mut magic)?;
        if &magic != CAPTURE_RECORD_MAGIC {
            return Err(BodyLogError::new("capture segment record magic mismatch"));
        }
        let compressed_len = read_u64_be(&mut file)?;
        let payload_len = read_u64_be(&mut file)?;
        let checksum = read_u32_be(&mut file)?;
        if compressed_len > CAPTURE_RECORD_MAX_BYTES
            || !(4..=CAPTURE_RECORD_MAX_BYTES).contains(&payload_len)
        {
            return Err(BodyLogError::new(
                "capture segment record exceeds safety limit",
            ));
        }
        let compressed_start = file.stream_position()?;
        if file_len.saturating_sub(compressed_start) < compressed_len {
            if recover_tail {
                truncate_partial_segment_tail(&file, frame_start)?;
                return Ok(());
            }
            return Err(BodyLogError::new(
                "capture segment record body is incomplete",
            ));
        }
        // Verify encoded bytes before decoding. A forged expanded-size header
        // cannot make the decoder allocate or emit an unbounded payload.
        let mut crc = crc32fast::Hasher::new();
        let mut remaining = compressed_len;
        while remaining != 0 {
            let wanted = remaining.min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..wanted])?;
            crc.update(&buffer[..wanted]);
            remaining -= wanted as u64;
        }
        if crc.finalize() != checksum {
            return Err(BodyLogError::new(
                "capture segment record checksum mismatch",
            ));
        }
        file.seek(std::io::SeekFrom::Start(compressed_start))?;
        {
            let compressed = (&mut file).take(compressed_len);
            let mut decoder = zstd::stream::read::Decoder::new(compressed)?;
            decoder.window_log_max(23)?;
            let mut payload = decoder.take(payload_len + 1);
            let metadata_len = read_u32_be(&mut payload)? as usize;
            if metadata_len > MAX_METADATA_BYTES || 4 + metadata_len as u64 > payload_len {
                return Err(BodyLogError::new(
                    "capture segment metadata exceeds safety limit",
                ));
            }
            let mut metadata = vec![0u8; metadata_len];
            payload.read_exact(&mut metadata)?;
            let record: BodyRecord = serde_json::from_slice(&metadata)?;
            let expected_body_bytes = payload_len - 4 - metadata_len as u64;
            if record.body_bytes != expected_body_bytes {
                return Err(BodyLogError::new("capture segment body integrity mismatch"));
            }
            let mut body = VerifiedBody {
                inner: (&mut payload).take(expected_body_bytes),
                hash: Sha256::new(),
                bytes: 0,
            };
            visitor(&record, &mut body)?;
            loop {
                let count = body.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
            }
            if body.bytes != record.body_bytes
                || format!("{:x}", body.hash.finalize()) != record.body_sha256
            {
                return Err(BodyLogError::new("capture segment body integrity mismatch"));
            }
            if payload.read(&mut buffer[..1])? != 0 {
                return Err(BodyLogError::new("capture segment payload length mismatch"));
            }
        }
        file.seek(std::io::SeekFrom::Start(compressed_start + compressed_len))?;
    }
}

pub(super) fn hash_file(path: &Path) -> Result<(String, u64)> {
    // Custody is adjudicated by each caller's existing owner. Hashing keeps
    // its established read-only path behavior while bounding the byte buffer.
    let mut file = fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(BodyLogError::new("capture artifact is not a regular file"));
    }
    let mut buffer = [0u8; IO_CHUNK_BYTES];
    let mut hash = Sha256::new();
    let mut bytes = 0u64;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hash.update(&buffer[..count]);
        bytes += count as u64;
    }
    Ok((format!("{:x}", hash.finalize()), bytes))
}

#[derive(Default)]
pub(super) struct SegmentSummary {
    pub record_count: u64,
    pub body_bytes: u64,
    pub first_observed_at_unix_ms: Option<i64>,
    pub last_observed_at_unix_ms: Option<i64>,
}

pub(super) fn segment_summary(path: &Path, recover_tail: bool) -> Result<SegmentSummary> {
    let mut summary = SegmentSummary::default();
    visit_segment(path, recover_tail, |record, _| {
        summary.record_count += 1;
        summary.body_bytes = summary.body_bytes.saturating_add(record.body_bytes);
        summary
            .first_observed_at_unix_ms
            .get_or_insert(record.observed_at_unix_ms);
        summary.last_observed_at_unix_ms = Some(record.observed_at_unix_ms);
        Ok(())
    })?;
    Ok(summary)
}

pub(super) trait FrameDestination: Write + Seek {
    fn reset(&mut self) -> std::io::Result<()>;
}
impl FrameDestination for std::io::Cursor<Vec<u8>> {
    fn reset(&mut self) -> std::io::Result<()> {
        self.get_mut().clear();
        self.set_position(0);
        Ok(())
    }
}
impl FrameDestination for fs::File {
    fn reset(&mut self) -> std::io::Result<()> {
        self.set_len(0)?;
        self.seek(std::io::SeekFrom::Start(0))?;
        Ok(())
    }
}

struct LimitedMetadata(Vec<u8>);
impl Write for LimitedMetadata {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > IO_CHUNK_BYTES {
            return Err(std::io::Error::other("capture metadata exceeds 64 KiB"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(super) fn metadata_json(value: &impl Serialize) -> Result<Vec<u8>> {
    let mut metadata = LimitedMetadata(Vec::new());
    serde_json::to_writer(&mut metadata, value)?;
    Ok(metadata.0)
}

struct EncodedWriter<W> {
    output: W,
    crc: crc32fast::Hasher,
    bytes: u64,
}
impl<W: Write> Write for EncodedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.bytes.saturating_add(bytes.len() as u64) > CAPTURE_RECORD_MAX_BYTES {
            return Err(std::io::Error::other("capture record exceeds safety limit"));
        }
        let written = self.output.write(bytes)?;
        self.crc.update(&bytes[..written]);
        self.bytes += written as u64;
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

/// Preserve REC1's encoded metadata and CRC while streaming the body. Both
/// memory and file-backed callers use this codec; retries rewind the same source.
pub(super) fn encode_frame(
    record: &mut BodyRecord,
    body: &mut (impl Read + Seek),
    body_bytes: u64,
    output: &mut impl FrameDestination,
) -> Result<u64> {
    if body_bytes > CAPTURE_RECORD_MAX_BYTES {
        return Err(BodyLogError::new(
            "capture segment record exceeds safety limit",
        ));
    }
    let mut frame_bytes = 0;
    for _ in 0..4 {
        let metadata = metadata_json(record)?;
        let payload_bytes = 4u64 + metadata.len() as u64 + body_bytes;
        if payload_bytes > CAPTURE_RECORD_MAX_BYTES {
            return Err(BodyLogError::new(
                "capture segment record exceeds safety limit",
            ));
        }
        output.reset()?;
        output.write_all(&[0u8; CAPTURE_RECORD_HEADER_BYTES as usize])?;
        body.seek(std::io::SeekFrom::Start(0))?;
        let encoded = EncodedWriter {
            output: &mut *output,
            crc: crc32fast::Hasher::new(),
            bytes: 0,
        };
        let mut encoder = zstd::stream::write::Encoder::new(encoded, ZSTD_LEVEL)?;
        encoder.write_all(&(metadata.len() as u32).to_be_bytes())?;
        encoder.write_all(&metadata)?;
        let mut total = 0u64;
        let mut body_hash = Sha256::new();
        let mut buffer = [0u8; IO_CHUNK_BYTES];
        loop {
            let count = body.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            total = total
                .checked_add(count as u64)
                .ok_or_else(|| BodyLogError::new("capture body length overflow"))?;
            if total > body_bytes {
                return Err(BodyLogError::new("capture source length changed"));
            }
            body_hash.update(&buffer[..count]);
            encoder.write_all(&buffer[..count])?;
        }
        if total != body_bytes || format!("{:x}", body_hash.finalize()) != record.body_sha256 {
            return Err(BodyLogError::new("capture source length changed"));
        }
        let encoded = encoder.finish()?;
        let compressed_bytes = encoded.bytes;
        let checksum = encoded.crc.finalize();
        output.seek(std::io::SeekFrom::Start(0))?;
        output.write_all(CAPTURE_RECORD_MAGIC)?;
        output.write_all(&compressed_bytes.to_be_bytes())?;
        output.write_all(&payload_bytes.to_be_bytes())?;
        output.write_all(&checksum.to_be_bytes())?;
        frame_bytes = CAPTURE_RECORD_HEADER_BYTES + compressed_bytes;
        output.seek(std::io::SeekFrom::Start(frame_bytes))?;
        if record.compressed_bytes == compressed_bytes {
            break;
        }
        record.compressed_bytes = compressed_bytes;
    }
    Ok(frame_bytes)
}
