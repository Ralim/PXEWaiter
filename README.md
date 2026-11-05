# PXEWaiter

When you need to run TFTP+HTTP servers to PXE boot machines on the network, in a super minimal manner.
This is designed to serve iPXE for a LAN that its running on. Either run it on a host machine or in a container with host networking 😀

## Usage

Create the directory you want to share to be available; for example here we will use `pxe`
Then run `pxewaiter` to serve that directory.

```sh
mkdir -p pxe
echo "test" > pxe/test
pxe_waiter -p "pxe/" --http 8080 --tftp 69
# Then elsewhere run
curl -vvv http://localhost:8080/test
```

Each request is logged with the client's source address and the folder it is served from, e.g. `[HTTP] 192.168.1.50:51234 requested /test from pxe/`.

## Source IP designation

Serve different files to different clients based on their source IP by passing `--source-ip-designation` or setting `SOURCE_IP_DESIGNATION=true` (also accepts `1`/`yes`/`on`).

The served path must then contain one folder per IPv4 network, named `a.b.c.d_z` for `a.b.c.d/z` (e.g. `10.32.0.0_16` for `10.32.0.0/16`), plus an optional `default` folder for clients that match no network.

- A client only sees the contents of its own folder, at the root of the server. For example `pxe/10.32.8.0_24/demo` is fetched by `10.32.8.3` as `http://server/demo` or `tftp://server/demo`.
- If several networks contain the client, the most specific (longest prefix) one wins.
- Clients matching no network get the `default` folder; without one their requests fail as not found.
- Folder names must be canonical: no host bits set (`10.32.8.0_16` is ignored) and no leading zeros.
- Anything else in the served path (files or other folders) is never served.

Folders are scanned once at startup and logged, so restart after adding or removing folders (their contents can change at any time):

```sh
mkdir -p pxe/10.32.0.0_16 pxe/10.32.8.0_24 pxe/default
pxe_waiter -p pxe/ --http 8080 --tftp 69 --source-ip-designation
# Source IP designation folders in pxe/:
#   pxe/10.32.8.0_24 serves 10.32.8.0/24
#   pxe/10.32.0.0_16 serves 10.32.0.0/16
#   pxe/default serves clients matching no other folder
```

In a container, pass `-e SOURCE_IP_DESIGNATION=true`. Host networking is required for this feature, as NAT hides the client's real source IP.

## Containers

Run the container with host networking (`--network=host` / `network_mode: host`).
TFTP sends each transfer from a new ephemeral port, which breaks with published ports, and NAT can hide the client's real source IP.

```sh
mkdir -p pxe
podman-compose -f docker-compose-demo.yaml up --build
# or
podman build -f Dockerfile.demo -t pxe_waiter:demo .
podman run --rm --network=host -v ./pxe:/pxe:ro,Z pxe_waiter:demo
```

Rootless containers cannot bind ports below 1024 by default; either run rootful, lower `net.ipv4.ip_unprivileged_port_start` on the host, or pass different ports (e.g. `-p /pxe --tftp 6969 --http 8080`).

## Kudos

This project is 99.999% built on the amazing work of other people. It's just a small wrapper that glues them together.

- <https://github.com/oblique/async-tftp-rs>
- <https://github.com/tokio-rs/tokio>
