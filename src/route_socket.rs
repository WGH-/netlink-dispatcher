use futures::stream::StreamExt;

use netlink_packet_route::{NetlinkMessage, RtnlMessage};
use netlink_proto::{NetlinkCodec, NetlinkFramed};
use rtnetlink::sys::{protocols::NETLINK_ROUTE, AsyncSocket};
use rtnetlink::sys::{SocketAddr, TokioSocket};

pub struct RouteSocket {
    framed: NetlinkFramed<RtnlMessage, TokioSocket, NetlinkCodec>,
}

impl RouteSocket {
    pub fn new() -> std::io::Result<Self> {
        let mut sock = TokioSocket::new(NETLINK_ROUTE)?;
        sock.socket_mut().bind_auto()?;
        Ok(Self::new_from_socket(sock))
    }

    pub fn new_from_socket(sock: TokioSocket) -> Self {
        let framed = NetlinkFramed::new(sock);
        Self { framed }
    }

    pub fn add_membership(&mut self, group: u32) -> std::io::Result<()> {
        self.framed.get_mut().socket_mut().add_membership(group)
    }

    pub async fn next_message(&mut self) -> Option<(NetlinkMessage<RtnlMessage>, SocketAddr)> {
        self.framed.next().await
    }
}
