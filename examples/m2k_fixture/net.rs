use std::io::{self, Read, Write};

const MAX_BUFFERED: usize = 32 * 1024 * 1024;

/// CRLF line reader that keeps unread bytes across read timeouts.
pub struct LineConn<S> {
    pub stream: S,
    buf: Vec<u8>,
}

impl<S: Read + Write> LineConn<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            buf: Vec::new(),
        }
    }

    fn fill(&mut self) -> io::Result<usize> {
        if self.buf.len() > MAX_BUFFERED {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request too large",
            ));
        }
        let mut chunk = [0_u8; 16 * 1024];
        let read = self.stream.read(&mut chunk)?;
        self.buf.extend_from_slice(&chunk[..read]);
        Ok(read)
    }

    /// Returns the next line without its CRLF, or `None` at EOF.
    pub fn read_line(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            if let Some(position) = self.buf.windows(2).position(|pair| pair == b"\r\n") {
                let mut line = self.buf.drain(..position + 2).collect::<Vec<_>>();
                line.truncate(position);
                return Ok(Some(line));
            }
            if self.fill()? == 0 {
                return Ok(None);
            }
        }
    }

    pub fn read_bytes(&mut self, count: usize) -> io::Result<Vec<u8>> {
        while self.buf.len() < count {
            if self.fill()? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        Ok(self.buf.drain(..count).collect())
    }

    pub fn write(&mut self, bytes: impl AsRef<[u8]>) -> io::Result<()> {
        self.stream.write_all(bytes.as_ref())?;
        self.stream.flush()
    }
}

pub fn is_timeout(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}
