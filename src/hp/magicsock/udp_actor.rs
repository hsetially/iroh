use std::{
    collections::VecDeque,
    io::{self, IoSliceMut},
    mem::MaybeUninit,
    net::SocketAddr,
    pin::Pin,
    sync::{atomic::Ordering, Arc},
    task::{Context, Poll},
};

use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use quinn::AsyncUdpSocket;
use tokio::sync::mpsc;
use tracing::{debug, trace, warn};

use crate::hp::{disco, netcheck, stun};

use super::{
    conn::{Inner, Network},
    rebinding_conn::RebindingUdpConn,
};

pub(super) enum UdpActorMessage {
    Shutdown,
}

#[derive(Debug)]
pub(super) enum NetworkReadResult {
    Error(io::Error),
    Ok {
        source: NetworkSource,
        meta: quinn_udp::RecvMeta,
        bytes: Bytes,
    },
}

#[derive(Debug)]
pub(super) enum NetworkSource {
    Ipv4,
    Ipv6,
    Derp,
}

pub(super) enum IpPacket {
    Disco {
        source: [u8; disco::KEY_LEN],
        sealed_box: Bytes,
        src: SocketAddr,
    },
    Forward(NetworkReadResult),
}

pub(super) struct UdpActor {
    conn: Arc<Inner>,
    pconn4: RebindingUdpConn,
    pconn6: Option<RebindingUdpConn>,
    recv_buf: Box<[u8]>,
    out_buffer: VecDeque<(Bytes, Network, quinn_udp::RecvMeta)>,
}

impl UdpActor {
    pub fn new(
        udp_state: &quinn_udp::UdpState,
        conn: Arc<Inner>,
        pconn4: RebindingUdpConn,
        pconn6: Option<RebindingUdpConn>,
    ) -> Self {
        // 1480 MTU size based on default from quinn
        let target_recv_buf_len = 1480 * udp_state.gro_segments() * quinn_udp::BATCH_SIZE;
        let recv_buf = vec![0u8; target_recv_buf_len];

        UdpActor {
            conn,
            pconn4,
            pconn6,
            recv_buf: recv_buf.into(),
            out_buffer: Default::default(),
        }
    }

    pub(super) async fn run(
        mut self,
        mut msg_receiver: mpsc::Receiver<UdpActorMessage>,
        stun_packet_channel: mpsc::Sender<netcheck::ActorMessage>,
        ip_sender: mpsc::Sender<IpPacket>,
    ) {
        loop {
            tokio::select! {
                biased;
                Some(msg) = msg_receiver.recv() => {
                    match msg {
                        UdpActorMessage::Shutdown => {
                            debug!("shutting down");
                            break;
                        }
                    }
                }
                msg = self.next() => {
                    match msg {
                        None => break,
                        Some(ip_msgs) => {
                            trace!("tick: ip_msgs");
                            match ip_msgs {
                                Ok((packet, network, meta)) => {
                                    // Classify packets

                                    // Stun?
                                    if stun::is(&packet) {
                                        let enable_stun_packets =
                                            self.conn.enable_stun_packets.load(Ordering::Relaxed);
                                        debug!("on_stun_receive, processing {}", enable_stun_packets);
                                        if enable_stun_packets {
                                            let msg = netcheck::ActorMessage::StunPacket(packet, meta.addr);
                                            stun_packet_channel.try_send(msg).ok();
                                        }
                                        continue;
                                    }
                                    // Disco?
                                    if let Some((source, sealed_box)) = disco::source_and_box(&packet) {
                                        if ip_sender
                                            .send(
                                                IpPacket::Disco {
                                                source,
                                                sealed_box: packet.slice_ref(sealed_box),
                                                src: meta.addr,
                                            })
                                            .await
                                            .is_err()
                                        {
                                            warn!("ip_sender gone");
                                            break;
                                        };
                                        continue;
                                    }

                                    // Foward
                                    let forward = match network {
                                        Network::Ipv4 => NetworkReadResult::Ok {
                                            source: NetworkSource::Ipv4,
                                            bytes: packet,
                                            meta,
                                        },
                                        Network::Ipv6 => NetworkReadResult::Ok {
                                            source: NetworkSource::Ipv6,
                                            bytes: packet,
                                            meta,
                                        },
                                    };

                                    if ip_sender.send(IpPacket::Forward(forward)).await.is_err() {
                                        warn!("ip_sender gone");
                                        break;
                                    }
                                }
                                Err(err) => {
                                    if ip_sender
                                        .send(IpPacket::Forward(NetworkReadResult::Error(err)))
                                        .await
                                        .is_err()
                                    {
                                        warn!("ip_sender gone");
                                        break;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn handle_packet(&mut self, packet: Bytes, network: Network, meta: quinn_udp::RecvMeta) {
        self.out_buffer.push_back((packet, network, meta));
    }
}

impl Stream for UdpActor {
    type Item = io::Result<(Bytes, Network, quinn_udp::RecvMeta)>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.conn.is_closed() {
            return Poll::Ready(None);
        }
        if let Some(res) = self.out_buffer.pop_front() {
            return Poll::Ready(Some(Ok(res)));
        }

        let mut metas = [quinn_udp::RecvMeta::default(); quinn_udp::BATCH_SIZE];
        let mut iovs = MaybeUninit::<[IoSliceMut; quinn_udp::BATCH_SIZE]>::uninit();
        let chunk_size = self.recv_buf.len() / quinn_udp::BATCH_SIZE;
        self.recv_buf
            .chunks_mut(chunk_size)
            .enumerate()
            .for_each(|(i, buf)| unsafe {
                iovs.as_mut_ptr()
                    .cast::<IoSliceMut>()
                    .add(i)
                    .write(IoSliceMut::new(buf));
            });
        let mut iovs = unsafe { iovs.assume_init() };

        if let Some(ref pconn6) = self.pconn6 {
            match pconn6.poll_recv(cx, &mut iovs, &mut metas) {
                Poll::Pending => {}
                Poll::Ready(Ok(msgs)) => {
                    for (mut meta, buf) in metas.into_iter().zip(iovs.iter()).take(msgs) {
                        let mut data: BytesMut = buf[0..meta.len].into();
                        let stride = meta.stride;
                        while !data.is_empty() {
                            let buf = data.split_to(stride.min(data.len())).freeze();
                            // set stride to len, as we are cutting it into pieces here
                            meta.len = buf.len();
                            meta.stride = buf.len();
                            self.handle_packet(buf, Network::Ipv6, meta);
                        }
                    }
                    if let Some(res) = self.out_buffer.pop_front() {
                        return Poll::Ready(Some(Ok(res)));
                    }
                }
                Poll::Ready(Err(err)) => {
                    return Poll::Ready(Some(Err(err)));
                }
            }
        }

        match self.pconn4.poll_recv(cx, &mut iovs, &mut metas) {
            Poll::Pending => {}
            Poll::Ready(Ok(msgs)) => {
                for (mut meta, buf) in metas.into_iter().zip(iovs.iter()).take(msgs) {
                    let mut data: BytesMut = buf[0..meta.len].into();
                    let stride = meta.stride;
                    while !data.is_empty() {
                        let buf = data.split_to(stride.min(data.len())).freeze();
                        // set stride to len, as we are cutting it into pieces here
                        meta.len = buf.len();
                        meta.stride = buf.len();
                        self.handle_packet(buf, Network::Ipv4, meta);
                    }
                }
                if let Some(res) = self.out_buffer.pop_front() {
                    return Poll::Ready(Some(Ok(res)));
                }
            }
            Poll::Ready(Err(err)) => {
                return Poll::Ready(Some(Err(err)));
            }
        }

        Poll::Pending
    }
}
