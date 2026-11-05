use std::{
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
};

use async_tftp::{
    packet,
    server::{
        Handler, TftpServer, TftpServerBuilder,
        handlers::{DirHandler, DirHandlerMode},
    },
};
use clap::Parser;
use warp::{Filter, Reply, filters::path::FullPath};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to serve
    #[arg(short, long)]
    path: PathBuf,

    // PORTS, setting one to non-zero will enable that service
    // tftp port
    #[arg(long, default_value_t = 69)]
    tftp: u16,
    // http port
    #[arg(long, default_value_t = 80)]
    http: u16,
}

/// Protocol an incoming request arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Protocol {
    Tftp,
    Http,
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Protocol::Tftp => f.write_str("TFTP"),
            Protocol::Http => f.write_str("HTTP"),
        }
    }
}

/// A file request from a client, captured before the file is served.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ClientRequest {
    protocol: Protocol,
    /// Remote address of the client; `None` only if the transport has no socket address.
    source: Option<SocketAddr>,
    /// Requested path exactly as the client sent it.
    path: String,
}

impl fmt::Display for ClientRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.source {
            Some(source) => write!(f, "[{}] {} requested {}", self.protocol, source, self.path),
            None => write!(f, "[{}] <unknown> requested {}", self.protocol, self.path),
        }
    }
}

/// Callback invoked for every TFTP read request and every HTTP request.
type RequestHook = Arc<dyn Fn(&ClientRequest) + Send + Sync>;

fn log_request_hook() -> RequestHook {
    Arc::new(|request: &ClientRequest| println!("{request}"))
}

/// TFTP handler that reports each request to a [`RequestHook`] then serves from a directory.
struct HookedDirHandler {
    inner: DirHandler,
    hook: RequestHook,
}

impl Handler for HookedDirHandler {
    type Reader = <DirHandler as Handler>::Reader;
    type Writer = <DirHandler as Handler>::Writer;

    async fn read_req_open(
        &mut self,
        client: &SocketAddr,
        path: &Path,
    ) -> Result<(Self::Reader, Option<u64>), packet::Error> {
        (self.hook)(&ClientRequest {
            protocol: Protocol::Tftp,
            source: Some(*client),
            path: path.to_string_lossy().into_owned(),
        });
        self.inner.read_req_open(client, path).await
    }

    async fn write_req_open(
        &mut self,
        client: &SocketAddr,
        path: &Path,
        size: Option<u64>,
    ) -> Result<Self::Writer, packet::Error> {
        // Inner handler is read-only and rejects all writes.
        self.inner.write_req_open(client, path, size).await
    }
}

/// Binds a read-only TFTP server for `server_path` on `addr`.
async fn build_tftpd(
    addr: SocketAddr,
    server_path: &Path,
    hook: RequestHook,
) -> async_tftp::Result<TftpServer<HookedDirHandler>> {
    let handler = HookedDirHandler {
        inner: DirHandler::new(server_path, DirHandlerMode::ReadOnly)?,
        hook,
    };
    TftpServerBuilder::with_handler(handler)
        .bind(addr)
        .build()
        .await
}

/// HTTP routes serving `server_path`, reporting each request to `hook` before lookup.
fn http_routes(
    server_path: PathBuf,
    hook: RequestHook,
) -> impl Filter<Extract = (impl Reply,), Error = warp::Rejection> + Clone {
    warp::addr::remote()
        .and(warp::path::full())
        .map(move |source: Option<SocketAddr>, path: FullPath| {
            hook(&ClientRequest {
                protocol: Protocol::Http,
                source,
                path: path.as_str().to_owned(),
            });
        })
        .untuple_one()
        .and(warp::fs::dir(server_path))
}

async fn run_tftpd(server_ip: IpAddr, port: u16, server_path: &Path, hook: RequestHook) {
    let addr = SocketAddr::new(server_ip, port);
    let server = build_tftpd(addr, server_path, hook)
        .await
        .unwrap_or_else(|err| {
            eprintln!("Problem creating TFTP server on {addr}: {err}");
            std::process::exit(1)
        });

    println!(
        "Running TFTP Server on {} in {}",
        server.listen_addr().unwrap_or(addr),
        server_path.display()
    );

    if let Err(err) = server.serve().await {
        eprintln!("TFTP server on {addr} stopped: {err}");
        std::process::exit(1)
    }
}

async fn run_httpd(server_ip: IpAddr, port: u16, server_path: PathBuf, hook: RequestHook) {
    println!(
        "Running HTTP Server on {}:{} in {}",
        server_ip,
        port,
        server_path.display()
    );
    warp::serve(http_routes(server_path, hook))
        .run((server_ip, port))
        .await;
}

/// Resolves on Ctrl-C, or SIGTERM on unix (sent by container runtimes to PID 1, which has no default handler).
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let server_path = args.path;
    let hook = log_request_hook();

    let server_ip = IpAddr::V4(Ipv4Addr::UNSPECIFIED); //Listen on all interfaces by default

    // Each service is enabled by a non-zero port; run whichever are enabled until they exit.
    let tftp = async {
        if args.tftp > 0 {
            run_tftpd(server_ip, args.tftp, &server_path, hook.clone()).await;
        }
    };
    let http = async {
        if args.http > 0 {
            run_httpd(server_ip, args.http, server_path.clone(), hook.clone()).await;
        }
    };
    tokio::select! {
        _ = async { tokio::join!(tftp, http) } => {}
        _ = shutdown_signal() => println!("Shutting down"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::net::{SocketAddrV4, UdpSocket};
    use std::sync::Mutex;
    use std::time::Duration;
    use tempfile::{TempDir, tempdir};
    use tftp_client::download;

    // Helper to find a free port
    fn get_free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn dir_with_file(name: &str, contents: &[u8]) -> TempDir {
        let dir = tempdir().unwrap();
        let mut file = File::create(dir.path().join(name)).unwrap();
        file.write_all(contents).unwrap();
        file.sync_all().unwrap();
        dir
    }

    /// Hook that records every request it sees.
    fn recording_hook() -> (RequestHook, Arc<Mutex<Vec<ClientRequest>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let hook: RequestHook = Arc::new(move |request: &ClientRequest| {
            sink.lock().unwrap().push(request.clone());
        });
        (hook, seen)
    }

    #[test]
    fn client_request_display() {
        let request = ClientRequest {
            protocol: Protocol::Tftp,
            source: Some("10.0.0.5:2000".parse().unwrap()),
            path: "/ipxe.efi".to_owned(),
        };
        assert_eq!(
            request.to_string(),
            "[TFTP] 10.0.0.5:2000 requested /ipxe.efi"
        );

        let request = ClientRequest {
            protocol: Protocol::Http,
            source: None,
            path: "/boot.ipxe".to_owned(),
        };
        assert_eq!(request.to_string(), "[HTTP] <unknown> requested /boot.ipxe");
    }

    #[tokio::test]
    async fn test_tftp_serves_file_and_missing_file() {
        let file_contents = b"Hello, TFTP!";
        let dir = dir_with_file("testfile.txt", file_contents);
        let (hook, seen) = recording_hook();

        let server = build_tftpd((Ipv4Addr::LOCALHOST, 0).into(), dir.path(), hook)
            .await
            .unwrap();
        let server_addr = server.listen_addr().unwrap();
        let server_task = tokio::spawn(server.serve());

        let (received, missing, client_addr) = tokio::task::spawn_blocking(move || {
            let socket = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
            let client_addr = socket.local_addr().unwrap();
            let timeout = Duration::from_millis(100);
            let max_timeout = Duration::from_secs(5);
            let retries = 8;

            let received = download(
                "/testfile.txt",
                &socket,
                server_addr,
                timeout,
                max_timeout,
                retries,
            );
            let missing = download(
                "/missing",
                &socket,
                server_addr,
                timeout,
                max_timeout,
                retries,
            );
            (received, missing, client_addr)
        })
        .await
        .unwrap();

        assert_eq!(received.unwrap(), file_contents);
        assert!(missing.is_err(), "Expected error for missing file");

        let seen = seen.lock().unwrap().clone();
        let paths: Vec<_> = seen.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, ["/testfile.txt", "/missing"]);
        for request in &seen {
            assert_eq!(request.protocol, Protocol::Tftp);
            assert_eq!(request.source, Some(client_addr));
        }

        server_task.abort();
    }

    #[tokio::test]
    async fn test_http_serves_file_and_missing_file() {
        use reqwest::StatusCode;

        let file_contents = b"Hello, HTTP!";
        let dir = dir_with_file("testfile.txt", file_contents);
        let (hook, seen) = recording_hook();

        // Pick a free port for HTTP
        let http_port = get_free_port();

        // Start HTTP server in a background task
        let routes = http_routes(dir.path().to_path_buf(), hook);
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = warp::serve(routes)
            .bind((Ipv4Addr::LOCALHOST, http_port))
            .await
            .graceful(async {
                shutdown_rx.await.ok();
            });
        let http_handle = tokio::spawn(server.run());

        // Connect with reqwest and fetch the file
        let url = format!("http://127.0.0.1:{}/testfile.txt", http_port);
        let resp = reqwest::get(&url).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.bytes().await.unwrap();
        assert_eq!(&body[..], file_contents);

        // Try to download a missing file
        let missing_url = format!("http://127.0.0.1:{}/missing", http_port);
        let missing_resp = reqwest::get(&missing_url).await.unwrap();
        assert_eq!(missing_resp.status(), StatusCode::NOT_FOUND);

        let seen = seen.lock().unwrap().clone();
        let paths: Vec<_> = seen.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, ["/testfile.txt", "/missing"]);
        for request in &seen {
            assert_eq!(request.protocol, Protocol::Http);
            let source = request.source.expect("HTTP request should have a source");
            assert_eq!(source.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        }

        // Shutdown the server
        let _ = shutdown_tx.send(());
        let _ = http_handle.await;
    }
}
