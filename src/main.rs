mod player;
mod protocol;
mod recorder;
mod rpc;

use std::io;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> io::Result<()> {
    let mut args = std::env::args_os().skip(1);
    match (args.next(), args.next(), args.next()) {
        (None, None, None) => run_stdio().await,
        (Some(flag), None, None) if flag == "--stdio" => run_stdio().await,
        (Some(flag), Some(path), None) if flag == "--socket" => run_socket(path.into()).await,
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "usage: replay-sidecar [--stdio | --socket PATH]",
        )),
    }
}

async fn run_stdio() -> io::Result<()> {
    rpc::serve(
        tokio::io::stdin(),
        tokio::io::stdout(),
        std::future::pending(),
    )
    .await
}

#[cfg(not(unix))]
async fn run_socket(_path: PathBuf) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Unix sockets require a Unix platform",
    ))
}

#[cfg(unix)]
async fn run_socket(path: PathBuf) -> io::Result<()> {
    use std::fs::{self, Permissions};
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use tokio::net::UnixListener;
    use tokio::signal::unix::{SignalKind, signal};
    use tokio::sync::watch;
    use tokio::task::JoinSet;

    struct SocketPath {
        path: PathBuf,
        device: u64,
        inode: u64,
    }

    impl Drop for SocketPath {
        fn drop(&mut self) {
            if self
                .path
                .symlink_metadata()
                .is_ok_and(|meta| meta.dev() == self.device && meta.ino() == self.inode)
            {
                let _ = fs::remove_file(&self.path);
            }
        }
    }

    fn report(result: Result<io::Result<()>, tokio::task::JoinError>) {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("socket client: {error}"),
            Err(error) => eprintln!("socket client task: {error}"),
        }
    }

    let listener = UnixListener::bind(&path)?;
    let meta = path.symlink_metadata()?;
    let socket = SocketPath {
        path,
        device: meta.dev(),
        inode: meta.ino(),
    };
    fs::set_permissions(&socket.path, Permissions::from_mode(0o600))?;
    // SAFETY: geteuid takes no arguments and has no preconditions.
    let uid = unsafe { libc::geteuid() };
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut terminate = signal(SignalKind::terminate())?;
    let (shutdown, _) = watch::channel(());
    let mut clients = JoinSet::new();
    let result = loop {
        tokio::select! {
            _ = interrupt.recv() => break Ok(()),
            _ = terminate.recv() => break Ok(()),
            Some(result) = clients.join_next() => report(result),
            connection = listener.accept() => {
                let (stream, _) = match connection {
                    Ok(connection) => connection,
                    Err(error) => break Err(error),
                };
                if stream.peer_cred().is_ok_and(|peer| peer.uid() == uid) {
                    let (input, output) = stream.into_split();
                    let mut done = shutdown.subscribe();
                    clients.spawn(rpc::serve(input, output, async move {
                        let _ = done.changed().await;
                    }));
                }
            }
        }
    };
    drop(listener);
    shutdown.send_replace(());
    while let Some(result) = clients.join_next().await {
        report(result);
    }
    result
}
