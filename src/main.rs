use std::{net::Ipv4Addr, path::PathBuf, thread::JoinHandle};

use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Name of the person to greet
    #[arg(short, long)]
    path: PathBuf,

    // PORTS, setting one to non-zero will enable that service
    // tftp port
    #[arg(long, default_value_t = 0)]
    tftp: u16,
    // http port
    #[arg(long, default_value_t = 0)]
    http: u16,
}

fn start_tftpd(port: u16, server_path: &PathBuf) -> JoinHandle<()> {
    if port > 0 {
        let tftp_port = port;
        let tftp_path = server_path.clone();
        std::thread::spawn(move || {
            //Start tftp server
            let server_ip = Ipv4Addr::new(0, 0, 0, 0); //Listen on all interfaces by default
            let config = tftpd::Config {
                ip_address: server_ip,
                port: tftp_port,
                directory: tftp_path,
                single_port: false,
                read_only: true,
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
        //No-op
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
    tftp_thread = start_tftpd(args.tftp, &server_path);

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
