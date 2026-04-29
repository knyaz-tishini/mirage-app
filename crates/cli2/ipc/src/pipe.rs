//! Named-pipe (Windows) / Unix-socket (Linux/macOS) IPC transport.
//!
//! Worker is the **server** side (binds/listens).
//! Supervisor is the **client** side (connects).
//!
//! This arrangement makes the worker the durable endpoint: it keeps accepting
//! new supervisor connections across supervisor restarts.

// ─── Windows implementation ──────────────────────────────────────────────────

#[cfg(windows)]
mod imp {
    use std::io;

    use serde::{Serialize, de::DeserializeOwned};
    use tokio::io::{BufReader, BufWriter};
    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };

    use crate::{read_msg, write_msg};

    /// A connected IPC session (server- or client-side, type-erased).
    pub struct PipePair {
        reader: BufReader<Box<dyn tokio::io::AsyncRead + Send + Unpin>>,
        writer: BufWriter<Box<dyn tokio::io::AsyncWrite + Send + Unpin>>,
    }

    impl PipePair {
        fn from_server(server: NamedPipeServer) -> Self {
            let (r, w) = tokio::io::split(server);
            Self {
                reader: BufReader::new(Box::new(r)),
                writer: BufWriter::new(Box::new(w)),
            }
        }

        fn from_client(client: NamedPipeClient) -> Self {
            let (r, w) = tokio::io::split(client);
            Self {
                reader: BufReader::new(Box::new(r)),
                writer: BufWriter::new(Box::new(w)),
            }
        }

        pub async fn read_msg<T: DeserializeOwned>(&mut self) -> io::Result<Option<T>> {
            read_msg(&mut self.reader).await
        }

        pub async fn write_msg<T: Serialize + Send + Sync>(&mut self, msg: &T) -> io::Result<()> {
            write_msg(&mut self.writer, msg).await
        }
    }

    /// Server-side listener (worker). Holds the first instance to own the pipe name.
    pub struct PipeListener {
        path: String,
        /// First-instance handle kept alive so the OS doesn't release the pipe name.
        _owner: NamedPipeServer,
    }

    impl PipeListener {
        /// Claim the pipe name and start accepting connections.
        pub fn bind(path: &str) -> io::Result<Self> {
            let owner = ServerOptions::new()
                .first_pipe_instance(true)
                .create(path)
                .map_err(|e| io::Error::new(e.kind(), format!("bind {path}: {e}")))?;
            Ok(Self {
                path: path.to_string(),
                _owner: owner,
            })
        }

        /// Block until a supervisor connects; return the connected pair.
        pub async fn accept(&mut self) -> io::Result<PipePair> {
            let server = ServerOptions::new()
                .first_pipe_instance(false)
                .create(&self.path)?;
            server.connect().await?;
            Ok(PipePair::from_server(server))
        }
    }

    /// Connect to a worker's pipe. Retries up to `max_attempts` × `delay_ms`.
    pub async fn connect_with_retry(
        path: &str,
        max_attempts: u32,
        delay_ms: u64,
    ) -> io::Result<PipePair> {
        let delay = tokio::time::Duration::from_millis(delay_ms);
        let mut last_err = io::Error::new(io::ErrorKind::NotFound, "no attempts made");
        for attempt in 0..max_attempts {
            match connect(path).await {
                Ok(pair) => return Ok(pair),
                Err(e) => {
                    last_err = e;
                    if attempt + 1 < max_attempts {
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }
        Err(io::Error::new(
            last_err.kind(),
            format!("connect to {path} failed after {max_attempts} attempts: {last_err}"),
        ))
    }

    /// Single-attempt connect (used for orphan probing).
    pub async fn connect(path: &str) -> io::Result<PipePair> {
        let client = ClientOptions::new()
            .open(path)
            .map_err(|e| io::Error::new(e.kind(), format!("connect {path}: {e}")))?;
        Ok(PipePair::from_client(client))
    }
}

// ─── Unix implementation ─────────────────────────────────────────────────────

#[cfg(not(windows))]
mod imp {
    use std::io;

    use serde::{Serialize, de::DeserializeOwned};
    use tokio::io::{BufReader, BufWriter};
    use tokio::net::{UnixListener, UnixStream};

    use crate::{read_msg, write_msg};

    /// A connected IPC session.
    pub struct PipePair {
        reader: BufReader<tokio::io::ReadHalf<UnixStream>>,
        writer: BufWriter<tokio::io::WriteHalf<UnixStream>>,
    }

    impl PipePair {
        pub async fn read_msg<T: DeserializeOwned>(&mut self) -> io::Result<Option<T>> {
            read_msg(&mut self.reader).await
        }

        pub async fn write_msg<T: Serialize + Send + Sync>(&mut self, msg: &T) -> io::Result<()> {
            write_msg(&mut self.writer, msg).await
        }
    }

    /// Server-side listener (worker).
    pub struct PipeListener {
        listener: UnixListener,
    }

    impl PipeListener {
        /// Bind and start listening. Removes any stale socket file first.
        pub fn bind(path: &str) -> io::Result<Self> {
            use std::path::Path;
            let p = Path::new(path);
            if p.exists() {
                std::fs::remove_file(p)?;
            }
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let listener = UnixListener::bind(path)?;
            Ok(Self { listener })
        }

        /// Block until a supervisor connects; return the connected pair.
        pub async fn accept(&mut self) -> io::Result<PipePair> {
            let (stream, _addr) = self.listener.accept().await?;
            let (r, w) = tokio::io::split(stream);
            Ok(PipePair {
                reader: BufReader::new(r),
                writer: BufWriter::new(w),
            })
        }
    }

    /// Connect to a worker's socket. Retries up to `max_attempts` × `delay_ms`.
    pub async fn connect_with_retry(
        path: &str,
        max_attempts: u32,
        delay_ms: u64,
    ) -> io::Result<PipePair> {
        let delay = tokio::time::Duration::from_millis(delay_ms);
        let mut last_err = io::Error::new(io::ErrorKind::NotFound, "no attempts made");
        for attempt in 0..max_attempts {
            match connect(path).await {
                Ok(pair) => return Ok(pair),
                Err(e) => {
                    last_err = e;
                    if attempt + 1 < max_attempts {
                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }
        Err(io::Error::new(
            last_err.kind(),
            format!("connect to {path} failed after {max_attempts} attempts: {last_err}"),
        ))
    }

    /// Single-attempt connect (used for orphan probing).
    pub async fn connect(path: &str) -> io::Result<PipePair> {
        let stream = UnixStream::connect(path)
            .await
            .map_err(|e| io::Error::new(e.kind(), format!("connect {path}: {e}")))?;
        let (r, w) = tokio::io::split(stream);
        Ok(PipePair {
            reader: BufReader::new(r),
            writer: BufWriter::new(w),
        })
    }
}

// ─── public re-exports ───────────────────────────────────────────────────────

pub use imp::{PipeListener, PipePair, connect, connect_with_retry};
