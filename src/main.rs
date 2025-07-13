use std::{
    net::Ipv4Addr,
    path::{Path, PathBuf},
    thread::JoinHandle,
};

use clap::Parser;

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

    //Config options
    // How many times TFTP should duplicate each packet, can help on lossy links
    #[arg(long, default_value_t = 1)]
    tftp_duplicate_packets: u8,
}

fn start_tftpd(port: u16, server_path: &Path, tftp_duplicate_packets: u8) -> JoinHandle<()> {
    if port > 0 {
        let tftp_port = port;
        let tftp_path = server_path.to_path_buf();
        std::thread::spawn(move || {
            //Start tftp server
            let server_ip = Ipv4Addr::new(0, 0, 0, 0); //Listen on all interfaces by default
            let config = tftpd::Config {
                ip_address: std::net::IpAddr::V4(server_ip),
                port: tftp_port,
                directory: tftp_path.clone(),
                single_port: true,
                read_only: true,
                receive_directory: tftp_path.clone(), // We are read only, so doesn't matter
                send_directory: tftp_path.clone(),
                duplicate_packets: tftp_duplicate_packets,
                overwrite: false,
                clean_on_error: true,
                max_retries: 10,
                rollover: tftpd::Rollover::DontCare,
            };

            let mut server = tftpd::Server::new(&config).unwrap_or_else(|err| {
                eprintln!(
                    "Problem creating server on {}:{}: {err}",
                    config.ip_address, config.port
                );
                std::process::exit(1)
            });

            println!(
                "Running TFTP Server on {}:{} in {}",
                config.ip_address,
                config.port,
                config.directory.display()
            );

            server.listen();
        })
    } else {
        //No-op, give back a blank thread handle
        std::thread::spawn(move || {})
    }
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let server_path = args.path;

    let server_ip = Ipv4Addr::new(0, 0, 0, 0); //Listen on all interfaces by default

    // Spawn tftp server if enabled
    let tftp_thread: JoinHandle<()> =
        start_tftpd(args.tftp, &server_path, args.tftp_duplicate_packets);

    //Start HTTP server if requested, and if so block waiting for it to exit
    if args.http > 0 {
        println!(
            "Running HTTP Server on {}:{} in {}",
            server_ip,
            args.http,
            server_path.display()
        );
        warp::serve(warp::fs::dir(server_path))
            .run((server_ip, args.http))
            .await;
    }
    //If tftp was started, wait for it to finish
    if args.tftp > 0 {
        tftp_thread.join().unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    use std::net::{SocketAddr, SocketAddrV4, UdpSocket};
    use std::thread;
    use std::time::Duration;
    use tempfile::tempdir;
    use tftp_client::download;

    // Helper to find a free port
    fn get_free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn test_tftp_serves_file_and_missing_file() {
        // Create a temp directory and file
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("testfile.txt");
        let file_contents = b"Hello, TFTP!";
        let mut file = File::create(&file_path).unwrap();
        file.write_all(file_contents).unwrap();
        file.sync_all().unwrap();

        // Pick a free port for TFTP
        let tftp_port = get_free_port();

        // Start TFTP server in a background thread
        let _ = start_tftpd(
            tftp_port,
            dir.path(),
            1, // no duplication
        );

        // Give the server a moment to start
        thread::sleep(Duration::from_millis(300));

        // Connect with tftp_client and fetch the file
        //
        let socket = UdpSocket::bind("0.0.0.0:0").unwrap();
        let server = SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, tftp_port));

        let timeout = Duration::from_millis(100);
        let max_timeout = Duration::from_secs(5);
        let retries = 8;
        // Download the file

        let received = download(
            "/testfile.txt",
            &socket,
            server,
            timeout,
            max_timeout,
            retries,
        )
        .unwrap();

        assert_eq!(received, file_contents);

        // Try to download a missing file
        let missing = download("/missing", &socket, server, timeout, max_timeout, retries);
        assert!(missing.is_err(), "Expected error for missing file");
    }
    #[tokio::test]
    async fn test_http_serves_file_and_missing_file() {
        use reqwest::StatusCode;
        use tokio::fs::File as TokioFile;
        use tokio::io::AsyncWriteExt;

        // Create a temp directory and file
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("testfile.txt");
        let file_contents = b"Hello, HTTP!";
        let mut file = TokioFile::create(&file_path).await.unwrap();
        file.write_all(file_contents).await.unwrap();
        file.sync_all().await.unwrap();

        // Pick a free port for HTTP
        let http_port = get_free_port();

        // Start HTTP server in a background task
        let server_path = dir.path().to_path_buf();
        let server_ip = Ipv4Addr::LOCALHOST;
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let http_handle = tokio::spawn(async move {
            let routes = warp::fs::dir(server_path);
            warp::serve(routes)
                .bind_with_graceful_shutdown((server_ip, http_port), async {
                    shutdown_rx.await.ok();
                })
                .1
                .await;
        });

        // Give the server a moment to start
        tokio::time::sleep(Duration::from_millis(300)).await;

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

        // Shutdown the server
        let _ = shutdown_tx.send(());
        let _ = http_handle.await;
    }
}
