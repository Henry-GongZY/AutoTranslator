//! Pluggable local transport: Windows named pipe or Unix domain socket.

/// Where the core listens. Never expose either variant to the network.
#[derive(Debug, Clone)]
pub enum Endpoint {
    Pipe(String),
    UnixSocket(String),
}

impl Endpoint {
    pub fn describe(&self) -> &str {
        match self {
            Endpoint::Pipe(name) => name.as_str(),
            Endpoint::UnixSocket(path) => path.as_str(),
        }
    }

    pub fn default_for_platform() -> Self {
        if cfg!(windows) {
            Endpoint::Pipe(translator_protocol::DEFAULT_PIPE_NAME.to_string())
        } else {
            Endpoint::UnixSocket(translator_protocol::DEFAULT_SOCKET_PATH.to_string())
        }
    }
}

#[cfg(windows)]
mod imp {
    use super::Endpoint;
    use std::io;
    use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

    pub type Stream = NamedPipeServer;

    pub struct Listener {
        name: String,
        /// Instance waiting for the next client.
        pending: Option<NamedPipeServer>,
    }

    pub fn bind(endpoint: &Endpoint) -> io::Result<Listener> {
        let name = endpoint.describe().to_string();
        let first = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&name)?;
        Ok(Listener {
            name,
            pending: Some(first),
        })
    }

    impl Listener {
        pub async fn accept(&mut self) -> io::Result<NamedPipeServer> {
            let server = self.pending.take().ok_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "named pipe listener already closed")
            })?;
            server.connect().await?;
            // Prepare the next instance so the client can reconnect.
            self.pending = Some(ServerOptions::new().create(&self.name)?);
            Ok(server)
        }
    }
}

#[cfg(unix)]
mod imp {
    use super::Endpoint;
    use std::io;
    use tokio::net::{UnixListener, UnixStream};

    pub type Stream = UnixStream;

    pub struct Listener {
        inner: UnixListener,
        path: String,
    }

    pub fn bind(endpoint: &Endpoint) -> io::Result<Listener> {
        let path = endpoint.describe().to_string();
        if let Ok(meta) = std::fs::metadata(&path) {
            if !meta.is_dir() {
                let _ = std::fs::remove_file(&path);
            }
        }
        let inner = UnixListener::bind(&path)?;
        // Best effort: keep the socket reachable only by the current user.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        }
        Ok(Listener { inner, path })
    }

    impl Listener {
        pub async fn accept(&mut self) -> io::Result<UnixStream> {
            let (stream, _) = self.inner.accept().await?;
            Ok(stream)
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub use imp::{bind, Listener, Stream};
