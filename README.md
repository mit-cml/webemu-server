# Webemu-server a special purpose webserver

“wserve” is a special purpose webserver for serving up the
html/css/javascript for the web emulator.

It is a static binary and runs in a “scratch” container where it is
the only binary. The binary itself contains all of the pages to serve
built into its binary.

To build, first put the heirarchy you want served in the “ui”
sub-directory. and then build with:

```sh
cargo build --target x86_64-unknown-linux-musl --release
docker build -t <whatever> .
```

Where <whatever> is whatever you want to name the container image.

By default the binary listens on port 8080. We assume that it is
behind a reverse web proxy that handles SSL/TLS negotiation and
proxies to port 8080.

Note that it catches HANGUP and TERMINATE signals and gracefully shuts
down, so it is amenable to being run as pid 1, which it would be in a
scratch container.

Included is *webemu.yaml* an example yaml file for serving under
k8s.

Dockerfile.debug builds a version with debug prints for helping
diagnosing issues.
