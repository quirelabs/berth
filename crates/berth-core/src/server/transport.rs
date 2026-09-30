//! Local endpoints: a Unix socket, or a named pipe on Windows.

#[cfg(unix)]
pub use unix::{Listener, Stream, connect};
#[cfg(windows)]
pub use windows::{Listener, Stream, connect};

#[cfg(unix)]
mod unix {
    use std::io;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use tokio::net::{UnixListener, UnixStream};

    pub type Stream = UnixStream;

    pub struct Listener {
        inner: UnixListener,
        path: PathBuf,
        /// Whoever may connect: the owner of the socket file, which is this
        /// process's user because it just created it.
        owner: u32,
    }

    impl Listener {
        /// Listen at `path`, replacing a socket file nothing answers on.
        /// Another process answering there is an error: one listener per
        /// endpoint.
        pub async fn bind(path: &Path) -> io::Result<Self> {
            if path.exists() {
                if UnixStream::connect(path).await.is_ok() {
                    return Err(io::Error::new(
                        io::ErrorKind::AddrInUse,
                        format!("something already answers at {}", path.display()),
                    ));
                }
                std::fs::remove_file(path)?;
            }
            let inner = UnixListener::bind(path)?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            let owner = std::fs::metadata(path)?.uid();
            Ok(Self {
                inner,
                path: path.to_path_buf(),
                owner,
            })
        }

        /// The next connection from this listener's user. Anyone else is
        /// dropped before a byte is read or written.
        pub async fn accept(&mut self) -> io::Result<Stream> {
            loop {
                let (stream, _) = self.inner.accept().await?;
                let peer = stream.peer_cred().map(|c| c.uid()).ok();
                if super::peer_allowed(peer, self.owner) {
                    return Ok(stream);
                }
                tracing::warn!(
                    ?peer,
                    owner = self.owner,
                    "refused a connection from another user"
                );
            }
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    pub async fn connect(path: &Path) -> io::Result<Stream> {
        UnixStream::connect(path).await
    }
}

/// Whether a peer may connect: only the endpoint owner's own user. A peer
/// whose user cannot be read is refused.
#[cfg_attr(windows, allow(dead_code))]
pub(crate) fn peer_allowed(peer: Option<u32>, owner: u32) -> bool {
    peer == Some(owner)
}

#[cfg(windows)]
mod windows {
    //! Named pipes, with a DACL that grants the current user and no one
    //! else, remote clients rejected, and the first instance claimed so
    //! another process cannot squat on the name first.

    use std::ffi::c_void;
    use std::io;
    use std::path::Path;

    use tokio::net::windows::named_pipe::{
        ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
    };
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
    use windows_sys::Win32::Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    };
    use windows_sys::Win32::Security::{
        GetTokenInformation, PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        TokenUser,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// Either end of a pipe. Reads and writes go through the inner handle.
    pub enum Stream {
        Server(NamedPipeServer),
        Client(NamedPipeClient),
    }

    impl tokio::io::AsyncRead for Stream {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            match self.get_mut() {
                Stream::Server(s) => std::pin::Pin::new(s).poll_read(cx, buf),
                Stream::Client(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            }
        }
    }

    impl tokio::io::AsyncWrite for Stream {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            match self.get_mut() {
                Stream::Server(s) => std::pin::Pin::new(s).poll_write(cx, buf),
                Stream::Client(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            }
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            match self.get_mut() {
                Stream::Server(s) => std::pin::Pin::new(s).poll_flush(cx),
                Stream::Client(s) => std::pin::Pin::new(s).poll_flush(cx),
            }
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            match self.get_mut() {
                Stream::Server(s) => std::pin::Pin::new(s).poll_shutdown(cx),
                Stream::Client(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            }
        }
    }

    /// A security descriptor from SDDL, freed with the value.
    struct Descriptor(PSECURITY_DESCRIPTOR);

    // SAFETY: the descriptor is immutable once built and only read by the
    // pipe calls that take it.
    unsafe impl Send for Descriptor {}
    unsafe impl Sync for Descriptor {}

    impl Drop for Descriptor {
        fn drop(&mut self) {
            // SAFETY: allocated by ConvertStringSecurityDescriptor..., which
            // documents LocalFree as its release.
            unsafe { LocalFree(self.0) };
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// The current user's SID as a string, from the process token.
    fn user_sid() -> io::Result<String> {
        // SAFETY: every pointer passed is to a local of the documented type
        // and size; handles and buffers are released on every path.
        unsafe {
            let mut token: HANDLE = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut len = 0u32;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
            let mut buf = vec![0u8; len as usize];
            let ok = GetTokenInformation(
                token,
                TokenUser,
                buf.as_mut_ptr().cast::<c_void>(),
                len,
                &mut len,
            );
            CloseHandle(token);
            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            let user = &*(buf.as_ptr().cast::<TOKEN_USER>());
            let mut text: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut text) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut end = 0;
            while *text.add(end) != 0 {
                end += 1;
            }
            let sid = String::from_utf16_lossy(std::slice::from_raw_parts(text, end));
            LocalFree(text.cast());
            Ok(sid)
        }
    }

    /// Full access for the current user, protected from inheritance, nobody
    /// else listed.
    fn owner_only() -> io::Result<Descriptor> {
        let sddl = wide(&format!("D:P(A;;GA;;;{})", user_sid()?));
        let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
        // SAFETY: `sddl` is NUL-terminated and outlives the call.
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Descriptor(descriptor))
    }

    pub struct Listener {
        name: String,
        descriptor: Descriptor,
        next: NamedPipeServer,
    }

    fn instance(name: &str, descriptor: &Descriptor, first: bool) -> io::Result<NamedPipeServer> {
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        // SAFETY: `attributes` and the descriptor it points to live across
        // the call, which copies what it needs.
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    name,
                    (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
                )
        }
    }

    impl Listener {
        pub async fn bind(path: &Path) -> io::Result<Self> {
            let name = path.to_string_lossy().to_string();
            let descriptor = owner_only()?;
            let next = instance(&name, &descriptor, true)?;
            Ok(Self {
                name,
                descriptor,
                next,
            })
        }

        pub async fn accept(&mut self) -> io::Result<Stream> {
            self.next.connect().await?;
            let fresh = instance(&self.name, &self.descriptor, false)?;
            Ok(Stream::Server(std::mem::replace(&mut self.next, fresh)))
        }
    }

    pub async fn connect(path: &Path) -> io::Result<Stream> {
        let name = path.to_string_lossy().to_string();
        // All instances busy is momentary: the server makes the next one
        // as soon as it accepts.
        for _ in 0..50 {
            match ClientOptions::new().open(&name) {
                Ok(client) => return Ok(Stream::Client(client)),
                Err(e) if e.raw_os_error() == Some(231) => {
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "every pipe instance stayed busy",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::peer_allowed;

    #[test]
    fn only_the_owner_may_connect() {
        assert!(peer_allowed(Some(501), 501));
        assert!(!peer_allowed(Some(502), 501));
        assert!(
            !peer_allowed(Some(0), 501),
            "not even root, through the socket"
        );
        assert!(!peer_allowed(None, 501), "an unknown peer is refused");
    }
}
