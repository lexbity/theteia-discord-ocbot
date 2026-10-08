//! Structured JSON logging with a secret-scrubbing writer (NFR-9, R-12).
//!
//! The tracing `fmt` subscriber writes one JSON object per line. Rather than
//! implement a `FormatEvent` (whose `Writer` type cannot be captured), the
//! scrubber sits at the `MakeWriter` boundary: it buffers the bytes of a single
//! log line, applies the [`Redactor`], and only then hands them to the real
//! writer. No secret ever reaches stderr — including errors formatted into
//! event fields.

use std::io;
use std::sync::Arc;

use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::MakeWriter;

use ocbot_core::Redactor;

/// Install the process-wide JSON subscriber, scrubbing every line with
/// `redactor`.
pub fn init(redactor: Arc<Redactor>) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .json()
        .with_writer(scrub_writer(std::io::stderr, redactor))
        .try_init();
}

/// Wrap a `MakeWriter` so every complete log line is redacted first.
pub fn scrub_writer<W>(inner: W, redactor: Arc<Redactor>) -> ScrubMakeWriter<W> {
    ScrubMakeWriter { inner, redactor }
}

/// A `MakeWriter` that scrubs each line produced by its writer.
pub struct ScrubMakeWriter<W> {
    inner: W,
    redactor: Arc<Redactor>,
}

impl<'a, W> MakeWriter<'a> for ScrubMakeWriter<W>
where
    W: MakeWriter<'a>,
{
    type Writer = ScrubWriter<W::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        ScrubWriter {
            inner: self.inner.make_writer(),
            redactor: self.redactor.clone(),
            buffer: Vec::new(),
        }
    }
}

/// A line-buffering, scrubbing [`io::Write`].
pub struct ScrubWriter<W: io::Write> {
    inner: W,
    redactor: Arc<Redactor>,
    buffer: Vec<u8>,
}

impl<W: io::Write> ScrubWriter<W> {
    /// Write out every complete line currently buffered.
    fn drain_lines(&mut self) -> io::Result<()> {
        while let Some(position) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=position).collect();
            self.emit(&line)?;
        }
        Ok(())
    }

    /// Write out a trailing partial line (the fmt layer always ends with `\n`,
    /// but a writer may be dropped mid-line).
    fn flush_buffer(&mut self) -> io::Result<()> {
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            self.emit(&line)?;
        }
        Ok(())
    }

    /// Scrub (or pass through) one line and forward it.
    fn emit(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.redactor.is_active() {
            let text = String::from_utf8_lossy(bytes);
            let scrubbed = self.redactor.scrub(&text);
            self.inner.write_all(scrubbed.as_bytes())
        } else {
            self.inner.write_all(bytes)
        }
    }
}

impl<W: io::Write> io::Write for ScrubWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        self.drain_lines()?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flush_buffer()?;
        self.inner.flush()
    }
}

impl<W: io::Write> Drop for ScrubWriter<W> {
    fn drop(&mut self) {
        let _ = self.flush_buffer();
    }
}
