use std::os::unix::prelude::*;
use std::path::{Path, PathBuf};

use clap::Parser;

use tokio::process::Command;
use tokio::{
    sync::watch,
    time::{sleep, Duration},
};

use netlink_packet_route::constants::{
    RTNLGRP_IPV4_IFADDR, RTNLGRP_IPV4_ROUTE, RTNLGRP_IPV6_IFADDR, RTNLGRP_IPV6_ROUTE,
};
use netlink_proto::sys::{AsyncSocket, TokioSocket};

use netlink_dispatcher::route_socket::RouteSocket;

#[derive(Parser, Debug)]
struct Cli {
    /// If socket-activated, exit after the specified timeout.
    #[clap(long, default_value_t = Duration::from_secs(30).into())]
    inactivity_timeout: humantime::Duration,

    /// Wait the specified time before running the scripts (debouncing).
    #[clap(long, default_value_t = Duration::from_secs(5).into())]
    settle_timeout: humantime::Duration,

    #[clap(long, default_value = "/etc/netlink-dispatcher/scripts.d")]
    scripts_directory: PathBuf,
}

fn set_cloexec(fd: RawFd) {
    use nix::fcntl;
    let flags = fcntl::fcntl(fd, fcntl::FcntlArg::F_GETFD).unwrap();
    let flags = unsafe { fcntl::FdFlag::from_bits_unchecked(flags) };
    fcntl::fcntl(
        fd,
        fcntl::FcntlArg::F_SETFD(flags | fcntl::FdFlag::FD_CLOEXEC),
    )
    .unwrap();
}

fn get_activated_socket() -> Option<TokioSocket> {
    let mut fds = listenfd::ListenFd::from_env();

    match fds.take_raw_fd(0) {
        Ok(Some(sock)) => {
            // TODO verify it's a Netlink socket? (ListenFd::take_custom
            // doesn't work propertly here as it can be either SOCK_RAW
            // or SOCK_DGRAM)
            let mut sock = unsafe { TokioSocket::from_raw_fd(sock) };
            set_cloexec(sock.socket_mut().as_raw_fd());
            sock.socket_mut()
                .set_non_blocking(true)
                .expect("setting socket to non-blocking should never fail");
            Some(sock)
        }
        Ok(None) => None,
        Err(err) => {
            log::warn!("Failed to retrieve socket: {}", err);
            None
        }
    }
}

async fn scripts_runner(
    mut rx: watch::Receiver<()>,
    settle_timeout: Duration,
    scripts_directory: &Path,
) {
    while rx.changed().await.is_ok() {
        log::debug!("Got update, waiting for settle");
        sleep(settle_timeout).await;
        rx.borrow_and_update(); // mark as seen
        if let Err(e) = run_scripts(scripts_directory).await {
            log::error!("Error while running scripts: {}", e);
        }
    }
}

async fn run_scripts(directory: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let status = Command::new("run-parts")
        .stdin(std::process::Stdio::null())
        .arg(directory)
        .status()
        .await?;

    if status.success() {
        Ok(())
    } else {
        Err(format!("run-parts returned {}", status).into())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();

    env_logger::init();

    let (mut sock, socket_activated) = match get_activated_socket() {
        Some(sock) => {
            log::info!("Got socket from socket activation");
            (RouteSocket::new_from_socket(sock), true)
        }
        None => {
            let mut sock = RouteSocket::new()?;
            let mut nl_groups: u32 = 0;
            for group in [
                RTNLGRP_IPV4_ROUTE,
                RTNLGRP_IPV6_ROUTE,
                RTNLGRP_IPV4_IFADDR,
                RTNLGRP_IPV6_IFADDR,
            ] {
                nl_groups |= 1 << (group - 1);
                sock.add_membership(group)
                    .expect("add_membership shouldn't fail");
            }
            log::info!("Socket created and bound");
            log::debug!(
                "nl_groups bitmask: {} (0x{:x}) (for socket unit)",
                nl_groups,
                nl_groups
            );
            (sock, false)
        }
    };

    let (tx, rx) = watch::channel(());

    let handle = tokio::spawn(async move {
        scripts_runner(rx, *cli.settle_timeout, &cli.scripts_directory).await;
    });

    loop {
        tokio::select! {
            res = sock.next_message() => {
                if let Some((message, _)) = res {
                    log::debug!("Received message: {:?}", message);
                    use netlink_packet_route::NetlinkPayload::*;
                    match message.payload {
                        Done => {
                            log::error!("Unexpected Done netlink packet");
                        },
                        Error(err) => {
                            log::error!("Unexpected Error netlink packet: {:?}", err);
                        },
                        Ack(ack) => {
                            log::warn!("Unexpected Ack netlink packet: {:?}", ack);
                        },
                        Noop => (),
                        Overrun(_) => {
                            log::warn!("Unexpected Overrun netlink packet");
                        },
                        InnerMessage(msg) => {
                            log::debug!("Inner message: {:?}", msg);
                            use netlink_packet_route::RtnlMessage::*;
                            match msg {
                                NewRoute(route_message) => {
                                    log::info!("Route added {:?} via {:?}", route_message.destination_prefix(), route_message.gateway());
                                    tx.send(()).expect("scripts_runner should still be working");
                                },
                                DelRoute(route_message) => {
                                    log::info!("Route removed {:?} via {:?}", route_message.destination_prefix(), route_message.gateway());
                                    tx.send(()).expect("scripts_runner should still be working");
                                },
                                NewAddress(address_message) => {
                                    log::info!("Address added {:?}", address_message);
                                    tx.send(()).expect("scripts_runner should still be working");
                                },
                                DelAddress(address_message) => {
                                    log::info!("Address deleted {:?}", address_message);
                                    tx.send(()).expect("scripts_runner should still be working");
                                }
                                _ => {
                                    log::debug!("Received other message {:?}", msg);
                                }
                            }
                        },
                    }
                } else {
                    log::error!("Stream unexpectedly stopped");
                    break;
                }
            },
            _ = sleep(*cli.inactivity_timeout), if socket_activated  => {
                log::info!("Inactivity timeout");
                break;
            },
        }
    }

    drop(tx);

    handle
        .await
        .expect("scripts_runner should finish successfully");

    Ok(())
}
