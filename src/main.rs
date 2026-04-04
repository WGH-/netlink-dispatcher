use std::os::fd::FromRawFd as _;
use std::os::unix::prelude::*;
use std::path::{Path, PathBuf};

use futures::stream::StreamExt;

use tokio::process::Command;
use tokio::{
    sync::watch,
    time::{sleep, Duration},
};

use netlink_proto::sys::TokioSocket;
use rtnetlink::sys::protocols::NETLINK_ROUTE;
use rtnetlink::sys::{Socket, SocketAddr};
use rtnetlink::MulticastGroup;

use clap::Parser;

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

fn get_activated_socket() -> Option<RawFd> {
    let mut fds = listenfd::ListenFd::from_env();

    match fds.take_raw_fd(0) {
        Ok(Some(sock)) => {
            set_cloexec(sock);
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

    let (raw_sock, socket_activated) = match get_activated_socket() {
        Some(sock) => {
            log::info!("Got socket from socket activation");
            (unsafe { Socket::from_raw_fd(sock) }, true)
        }
        None => {
            let mut sock = Socket::new(NETLINK_ROUTE)?;
            let addr = SocketAddr::new(0, 0);
            sock.bind(&addr).unwrap();

            for group in &[
                MulticastGroup::Ipv4Route,
                MulticastGroup::Ipv6Route,
                MulticastGroup::Ipv4Ifaddr,
                MulticastGroup::Ipv6Ifaddr,
            ] {
                sock.add_membership(*group as u32)?;
            }
            log::info!("Socket created and bound");
            (sock, false)
        }
    };

    // XXX https://github.com/rust-netlink/netlink-sys/pull/40
    let sock = unsafe { TokioSocket::from_raw_fd(raw_sock.as_raw_fd()) };
    std::mem::forget(raw_sock);

    let (tx, rx) = watch::channel(());

    let handle = tokio::spawn(async move {
        scripts_runner(rx, *cli.settle_timeout, &cli.scripts_directory).await;
    });

    let (conn, _handle, mut messages) = rtnetlink::from_socket(sock);
    tokio::spawn(conn);

    loop {
        tokio::select! {
            res = messages.next() => {
                if let Some((message, _)) = res {
                    log::debug!("Received message: {:?}", message);

                    use rtnetlink::packet_core::NetlinkPayload::*;
                    match message.payload {
                        Done(_) => {
                            log::error!("Unexpected Done netlink packet");
                        },
                        Error(err) => {
                            log::error!("Unexpected Error netlink packet: {:?}", err);
                        },
                        Noop => (),
                        Overrun(_) => {
                            log::warn!("Unexpected Overrun netlink packet");
                        },
                        InnerMessage(msg) => {
                            log::debug!("Inner message: {:?}", msg);

                            use rtnetlink::packet_route::RouteNetlinkMessage::*;
                            match msg {
                                NewRoute(route_message) => {
                                    log::info!("Route added {:?}", route_message);
                                    tx.send(()).expect("scripts_runner should still be working");
                                },
                                DelRoute(route_message) => {
                                    log::info!("Route removed {:?}", route_message);
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
                        other => {
                            log::warn!("Unexpected netlink packet {:?}", other);
                        }
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
