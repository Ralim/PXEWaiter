# PXEWaiter

When you need to run TFTP+HTTP servers to PXE boot machines on the network, in a super minimal manner.
This is designed to serve iPXE for a LAN that its running on. Either run it on a host machine or in docker; doesnt matter which 😀

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

## Kudos

This project is 99.999% built on the amazing work of other people. It's just a small wrapper that glues them together.

- <https://github.com/altugbakan/rs-tftpd>
- <https://github.com/tokio-rs/tokio>
