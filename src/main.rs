use std::{net::Ipv4Addr, path::PathBuf, thread::JoinHandle};

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

fn start_tftpd(port: u16, server_path: &PathBuf, tftp_duplicate_packets: u8) -> JoinHandle<()> {
    if port > 0 {
        let tftp_port = port;
        let tftp_path = server_path.clone();
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
                overwrite: false, // We are read only, so doesn't matter
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
    let tftp_thread: JoinHandle<()>;
    let server_ip = Ipv4Addr::new(0, 0, 0, 0); //Listen on all interfaces by default

    // Spawn tftp server if enabled
    tftp_thread = start_tftpd(args.tftp, &server_path, args.tftp_duplicate_packets);

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
