mod designation;

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
use clap::{Parser, builder::BoolishValueParser};
use designation::{Router, Scan};
use warp::{Filter, Reply, filters::path::FullPath};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to serve
    #[arg(short, long)]
    path: PathBuf,

    /// Serve each client only from the folder under PATH named after its network
    /// (`a.b.c.d_z` for a.b.c.d/z), falling back to a `default` folder
    #[arg(long, env = "SOURCE_IP_DESIGNATION", value_parser = BoolishValueParser::new())]
    source_ip_designation: bool,

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
    /// Folder the request is served from; `None` if no folder serves this client.
    folder: Option<PathBuf>,
}

impl fmt::Display for ClientRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] ", self.protocol)?;
        match self.source {
            Some(source) => write!(f, "{source}")?,
            None => f.write_str("<unknown>")?,
        }
        write!(f, " requested {}", self.path)?;
        match &self.folder {
            Some(folder) => write!(f, " from {}", folder.display()),
            None => f.write_str(" (no matching folder)"),
        }
    }
}

/// Callback invoked for every TFTP read request and every HTTP request.
type RequestHook = Arc<dyn Fn(&ClientRequest) + Send + Sync>;

fn log_request_hook() -> RequestHook {
    Arc::new(|request: &ClientRequest| println!("{request}"))
}

/// Picks the folder serving `source` and reports the request to `hook`.
///
/// Returns the index of the chosen folder in [`Router::folders`].
fn route_request(
    router: &Router,
    hook: &RequestHook,
    protocol: Protocol,
    source: Option<SocketAddr>,
    path: String,
) -> Option<usize> {
    let index = router.resolve(source.map(|source| source.ip()));
    hook(&ClientRequest {
        protocol,
        source,
        path,
        folder: index.map(|index| router.folders()[index].path.clone()),
    });
    index
}

/// TFTP handler that serves each client from the directory its [`Router`] picks.
struct RoutedDirHandler {
    router: Arc<Router>,
    /// One handler per entry in [`Router::folders`].
    dirs: Vec<DirHandler>,
    hook: RequestHook,
}

impl Handler for RoutedDirHandler {
    type Reader = <DirHandler as Handler>::Reader;
    type Writer = <DirHandler as Handler>::Writer;

    async fn read_req_open(
        &mut self,
        client: &SocketAddr,
        path: &Path,
    ) -> Result<(Self::Reader, Option<u64>), packet::Error> {
        let index = route_request(
            &self.router,
            &self.hook,
            Protocol::Tftp,
            Some(*client),
            path.to_string_lossy().into_owned(),
        );
        match index {
            Some(index) => self.dirs[index].read_req_open(client, path).await,
            None => Err(packet::Error::FileNotFound),
        }
    }

    async fn write_req_open(
        &mut self,
        _client: &SocketAddr,
        _path: &Path,
        _size: Option<u64>,
    ) -> Result<Self::Writer, packet::Error> {
        // The server is read-only.
        Err(packet::Error::IllegalOperation)
    }
}

/// Binds a read-only TFTP server for the folders of `router` on `addr`.
async fn build_tftpd(
    addr: SocketAddr,
    router: Arc<Router>,
    hook: RequestHook,
) -> async_tftp::Result<TftpServer<RoutedDirHandler>> {
    let dirs = router
        .folders()
        .iter()
        .map(|folder| DirHandler::new(&folder.path, DirHandlerMode::ReadOnly))
        .collect::<async_tftp::Result<_>>()?;
    let handler = RoutedDirHandler { router, dirs, hook };
    TftpServerBuilder::with_handler(handler)
        .bind(addr)
        .build()
        .await
}

/// HTTP routes serving each client from the folder `router` picks,
/// reporting each request to `hook` before lookup.
fn http_routes(
    router: Arc<Router>,
    hook: RequestHook,
) -> impl Filter<Extract = (impl Reply,), Error = warp::Rejection> + Clone {
    let report = {
        let router = router.clone();
        warp::addr::remote()
            .and(warp::path::full())
            .map(move |source: Option<SocketAddr>, path: FullPath| {
                route_request(
                    &router,
                    &hook,
                    Protocol::Http,
                    source,
                    path.as_str().to_owned(),
                );
            })
            .untuple_one()
    };

    // One branch per folder; a branch only proceeds for clients routed to its folder.
    let folders = router
        .folders()
        .iter()
        .enumerate()
        .map(|(index, folder)| {
            let router = router.clone();
            warp::addr::remote()
                .and_then(move |source: Option<SocketAddr>| {
                    let selected = router.resolve(source.map(|source| source.ip())) == Some(index);
                    async move {
                        if selected {
                            Ok(())
                        } else {
                            Err(warp::reject::not_found())
                        }
                    }
                })
                .untuple_one()
                .and(warp::fs::dir(folder.path.clone()))
                .boxed()
        })
        .reduce(|first, second| first.or(second).unify().boxed())
        .expect("a router always has at least one folder");

    report.and(folders)
}

/// Scans `root` for designation folders and logs what each one serves.
fn scan_designation_folders(root: &Path) -> Router {
    let Scan { router, ignored } = Router::scan(root).unwrap_or_else(|err| {
        eprintln!(
            "Problem reading source IP designation folders in {}: {err}",
            root.display()
        );
        std::process::exit(1)
    });
    println!("Source IP designation folders in {}:", root.display());
    for folder in router.folders() {
        println!("  {} serves {}", folder.path.display(), folder.clients);
    }
    for path in ignored {
        println!(
            "  {} ignored: not a `a.b.c.d_z` or `{}` folder",
            path.display(),
            designation::DEFAULT_FOLDER
        );
    }
    router
}

async fn run_tftpd(
    server_ip: IpAddr,
    port: u16,
    server_path: &Path,
    router: Arc<Router>,
    hook: RequestHook,
) {
    let addr = SocketAddr::new(server_ip, port);
    let server = build_tftpd(addr, router, hook).await.unwrap_or_else(|err| {
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

async fn run_httpd(
    server_ip: IpAddr,
    port: u16,
    server_path: &Path,
    router: Arc<Router>,
    hook: RequestHook,
) {
    println!(
        "Running HTTP Server on {}:{} in {}",
        server_ip,
        port,
        server_path.display()
    );
    warp::serve(http_routes(router, hook))
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
    let router = Arc::new(if args.source_ip_designation {
        scan_designation_folders(&server_path)
    } else {
        Router::single(server_path.clone())
    });

    let server_ip = IpAddr::V4(Ipv4Addr::UNSPECIFIED); //Listen on all interfaces by default

    // Each service is enabled by a non-zero port; run whichever are enabled until they exit.
    let tftp = async {
        if args.tftp > 0 {
            run_tftpd(
                server_ip,
                args.tftp,
                &server_path,
                router.clone(),
                hook.clone(),
            )
            .await;
        }
    };
    let http = async {
        if args.http > 0 {
            run_httpd(
                server_ip,
                args.http,
                &server_path,
                router.clone(),
                hook.clone(),
            )
            .await;
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
    use clap::CommandFactory;
    use reqwest::StatusCode;
    use std::fs;
    use std::net::UdpSocket;
    use std::sync::Mutex;
    use std::time::Duration;
    use tempfile::{TempDir, tempdir};
    use tftp_client::download;
    use tokio::{sync::oneshot, task::JoinHandle};

    // Helper to find a free port
    fn get_free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// Temp dir holding `files`, given as `(relative path, contents)`.
    fn tree<'a>(files: impl IntoIterator<Item = &'a (&'a str, &'a [u8])>) -> TempDir {
        let dir = tempdir().unwrap();
        for (path, contents) in files {
            let path = dir.path().join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, contents).unwrap();
        }
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

    /// Starts a TFTP server on loopback.
    async fn spawn_tftpd(
        router: Arc<Router>,
        hook: RequestHook,
    ) -> (SocketAddr, JoinHandle<async_tftp::Result<()>>) {
        let server = build_tftpd((Ipv4Addr::LOCALHOST, 0).into(), router, hook)
            .await
            .unwrap();
        let addr = server.listen_addr().unwrap();
        (addr, tokio::spawn(server.serve()))
    }

    /// Downloads each of `paths` over TFTP from a loopback client.
    ///
    /// Returns each file's contents (`None` on error) and the client's address.
    async fn tftp_fetch(
        server_addr: SocketAddr,
        paths: &[&str],
    ) -> (Vec<Option<Vec<u8>>>, SocketAddr) {
        let paths: Vec<String> = paths.iter().map(|path| path.to_string()).collect();
        tokio::task::spawn_blocking(move || {
            let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let client_addr = socket.local_addr().unwrap();
            let timeout = Duration::from_millis(100);
            let max_timeout = Duration::from_secs(5);
            let retries = 8;
            let received = paths
                .iter()
                .map(|path| {
                    download(path, &socket, server_addr, timeout, max_timeout, retries).ok()
                })
                .collect();
            (received, client_addr)
        })
        .await
        .unwrap()
    }

    struct HttpServer {
        port: u16,
        shutdown: oneshot::Sender<()>,
        handle: JoinHandle<()>,
    }

    impl HttpServer {
        async fn spawn(router: Arc<Router>, hook: RequestHook) -> Self {
            let port = get_free_port();
            let (shutdown, shutdown_rx) = oneshot::channel::<()>();
            let server = warp::serve(http_routes(router, hook))
                .bind((Ipv4Addr::LOCALHOST, port))
                .await
                .graceful(async {
                    shutdown_rx.await.ok();
                });
            let handle = tokio::spawn(server.run());
            Self {
                port,
                shutdown,
                handle,
            }
        }

        /// Fetches `path` (which must start with `/`) from loopback.
        async fn get(&self, path: &str) -> (StatusCode, Vec<u8>) {
            let resp = reqwest::get(format!("http://127.0.0.1:{}{path}", self.port))
                .await
                .unwrap();
            let status = resp.status();
            (status, resp.bytes().await.unwrap().to_vec())
        }

        async fn stop(self) {
            let _ = self.shutdown.send(());
            let _ = self.handle.await;
        }
    }

    #[test]
    fn args() {
        Args::command().debug_assert();
        let args =
            Args::try_parse_from(["pxe_waiter", "-p", "pxe", "--source-ip-designation"]).unwrap();
        assert!(args.source_ip_designation);
        assert_eq!(args.path, PathBuf::from("pxe"));
    }

    #[test]
    fn client_request_display() {
        let request = ClientRequest {
            protocol: Protocol::Tftp,
            source: Some("10.0.0.5:2000".parse().unwrap()),
            path: "/ipxe.efi".to_owned(),
            folder: Some("/pxe/10.0.0.0_24".into()),
        };
        assert_eq!(
            request.to_string(),
            "[TFTP] 10.0.0.5:2000 requested /ipxe.efi from /pxe/10.0.0.0_24"
        );

        let request = ClientRequest {
            protocol: Protocol::Http,
            source: None,
            path: "/boot.ipxe".to_owned(),
            folder: None,
        };
        assert_eq!(
            request.to_string(),
            "[HTTP] <unknown> requested /boot.ipxe (no matching folder)"
        );
    }

    #[tokio::test]
    async fn test_tftp_serves_file_and_missing_file() {
        let file_contents = b"Hello, TFTP!";
        let dir = tree(&[("testfile.txt", &file_contents[..])]);
        let (hook, seen) = recording_hook();

        let router = Arc::new(Router::single(dir.path()));
        let (server_addr, server_task) = spawn_tftpd(router, hook).await;
        let (received, client_addr) = tftp_fetch(server_addr, &["/testfile.txt", "/missing"]).await;

        assert_eq!(received, [Some(file_contents.to_vec()), None]);

        let seen = seen.lock().unwrap().clone();
        let paths: Vec<_> = seen.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, ["/testfile.txt", "/missing"]);
        for request in &seen {
            assert_eq!(request.protocol, Protocol::Tftp);
            assert_eq!(request.source, Some(client_addr));
            assert_eq!(request.folder.as_deref(), Some(dir.path()));
        }

        server_task.abort();
    }

    #[tokio::test]
    async fn test_http_serves_file_and_missing_file() {
        let file_contents = b"Hello, HTTP!";
        let dir = tree(&[("testfile.txt", &file_contents[..])]);
        let (hook, seen) = recording_hook();

        let server = HttpServer::spawn(Arc::new(Router::single(dir.path())), hook).await;
        assert_eq!(
            server.get("/testfile.txt").await,
            (StatusCode::OK, file_contents.to_vec())
        );
        assert_eq!(server.get("/missing").await.0, StatusCode::NOT_FOUND);

        let seen = seen.lock().unwrap().clone();
        let paths: Vec<_> = seen.iter().map(|r| r.path.as_str()).collect();
        assert_eq!(paths, ["/testfile.txt", "/missing"]);
        for request in &seen {
            assert_eq!(request.protocol, Protocol::Http);
            let source = request.source.expect("HTTP request should have a source");
            assert_eq!(source.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
            assert_eq!(request.folder.as_deref(), Some(dir.path()));
        }

        server.stop().await;
    }

    /// Files for designation tests; each folder holds a unique file and a shared `boot.ipxe`.
    const DESIGNATION_FILES: &[(&str, &[u8])] = &[
        ("127.0.0.0_8/boot.ipxe", b"loopback"),
        ("127.0.0.0_8/only-loopback", b"only loopback"),
        ("10.0.0.0_8/boot.ipxe", b"ten"),
        ("10.0.0.0_8/only-ten", b"only ten"),
        ("default/boot.ipxe", b"default"),
        ("default/only-default", b"only default"),
        ("root.txt", b"root"),
    ];

    /// Paths a loopback client requests in designation tests, relative to its folder.
    const DESIGNATION_REQUESTS: &[&str] = &[
        "boot.ipxe",
        "only-loopback",
        "only-ten",
        "only-default",
        "root.txt",
        "default/boot.ipxe",
        "10.0.0.0_8/boot.ipxe",
    ];

    /// Each case lists the folders present and which one should serve a loopback client.
    const DESIGNATION_CASES: &[(&[&str], Option<&str>)] = &[
        (
            &["127.0.0.0_8", "10.0.0.0_8", "default"],
            Some("127.0.0.0_8"),
        ),
        (&["10.0.0.0_8", "default"], Some("default")),
        (&["10.0.0.0_8"], None),
    ];

    /// Designation root containing only the `folders` subset of [`DESIGNATION_FILES`].
    fn designation_tree(folders: &[&str]) -> (TempDir, Arc<Router>) {
        let dir = tree(DESIGNATION_FILES.iter().filter(|(path, _)| {
            path.split_once('/')
                .is_none_or(|(folder, _)| folders.contains(&folder))
        }));
        let router = Arc::new(Router::scan(dir.path()).unwrap().router);
        (dir, router)
    }

    /// Contents a client served from `served` should receive for `request`.
    fn expected_contents(served: Option<&str>, request: &str) -> Option<Vec<u8>> {
        let full_path = format!("{}/{request}", served?);
        DESIGNATION_FILES
            .iter()
            .find(|(path, _)| *path == full_path)
            .map(|(_, contents)| contents.to_vec())
    }

    #[tokio::test]
    async fn test_tftp_source_ip_designation() {
        for &(folders, served) in DESIGNATION_CASES {
            let (dir, router) = designation_tree(folders);
            let (hook, seen) = recording_hook();
            let (server_addr, server_task) = spawn_tftpd(router, hook).await;

            let mut requests = DESIGNATION_REQUESTS.to_vec();
            requests.push("../default/boot.ipxe");
            let (received, _) = tftp_fetch(server_addr, &requests).await;

            let mut expected: Vec<_> = DESIGNATION_REQUESTS
                .iter()
                .map(|request| expected_contents(served, request))
                .collect();
            expected.push(None);
            assert_eq!(received, expected, "folders {folders:?}");

            let expected_folder = served.map(|served| dir.path().join(served));
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), requests.len());
            for request in &seen {
                assert_eq!(request.folder, expected_folder, "folders {folders:?}");
            }

            server_task.abort();
        }
    }

    #[tokio::test]
    async fn test_http_source_ip_designation() {
        for &(folders, served) in DESIGNATION_CASES {
            let (dir, router) = designation_tree(folders);
            let (hook, seen) = recording_hook();
            let server = HttpServer::spawn(router, hook).await;

            for request in DESIGNATION_REQUESTS {
                let expected = match expected_contents(served, request) {
                    Some(contents) => (StatusCode::OK, contents),
                    None => (StatusCode::NOT_FOUND, Vec::new()),
                };
                let (status, body) = server.get(&format!("/{request}")).await;
                let body = if status == StatusCode::OK {
                    body
                } else {
                    Vec::new()
                };
                assert_eq!(
                    (status, body),
                    expected,
                    "{request} with folders {folders:?}"
                );
            }
            // An encoded `/` keeps the `..` segment intact on the client side.
            assert_eq!(
                server.get("/..%2Fdefault/boot.ipxe").await.0,
                StatusCode::NOT_FOUND
            );

            let expected_folder = served.map(|served| dir.path().join(served));
            let seen = seen.lock().unwrap().clone();
            assert_eq!(seen.len(), DESIGNATION_REQUESTS.len() + 1);
            for request in &seen {
                assert_eq!(request.folder, expected_folder, "folders {folders:?}");
            }

            server.stop().await;
        }
    }
}
