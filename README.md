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

Each request is logged with the client's source address, e.g. `[HTTP] 192.168.1.50:51234 requested /test`.

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
