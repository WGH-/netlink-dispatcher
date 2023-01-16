ARG IMAGE=debian:11

FROM $IMAGE AS build

SHELL ["/bin/bash", "-c"]

# install all of the apt packages
RUN set -eux; \
    export DEBIAN_FRONTEND=noninteractive; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
# for installing rustup and cargo-deb
        curl ca-certificates gcc libc6-dev \
# cargo-deb can use dpkg-shlibdeps to autogenerate dependencies
        dpkg-dev \
    ; \
    apt clean; \
    rm -rf /var/lib/apt/lists/*;

# install Rust and cargo-deb
RUN set -eux; \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s - --default-toolchain 1.66 -y; \
    source $HOME/.cargo/env; \
    rustc --version; \
    cargo --version; \
    cargo install --version 1.41.1 cargo-deb;

ADD . /usr/src/netlink-dispatcher/
RUN set -eux; \
    cd /usr/src/netlink-dispatcher/; \
    source $HOME/.cargo/env; \
    cargo deb; \
    cp target/debian/*.deb /var/tmp/; \
    cargo clean

FROM scratch
COPY --from=build /var/tmp/*.deb  .
