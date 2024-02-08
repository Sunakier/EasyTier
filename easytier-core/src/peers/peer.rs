use std::sync::Arc;

use dashmap::DashMap;

use tokio::{
    select,
    sync::{mpsc, Mutex},
    task::JoinHandle,
};
use tokio_util::bytes::Bytes;
use tracing::Instrument;
use uuid::Uuid;

use super::peer_conn::PeerConn;
use crate::common::{
    error::Error,
    global_ctx::{ArcGlobalCtx, GlobalCtxEvent},
};
use crate::rpc::PeerConnInfo;

type ArcPeerConn = Arc<Mutex<PeerConn>>;
type ConnMap = Arc<DashMap<Uuid, ArcPeerConn>>;

pub struct Peer {
    pub peer_node_id: uuid::Uuid,
    conns: ConnMap,
    global_ctx: ArcGlobalCtx,

    packet_recv_chan: mpsc::Sender<Bytes>,

    close_event_sender: mpsc::Sender<Uuid>,
    close_event_listener: JoinHandle<()>,

    shutdown_notifier: Arc<tokio::sync::Notify>,
}

impl Peer {
    pub fn new(
        peer_node_id: uuid::Uuid,
        packet_recv_chan: mpsc::Sender<Bytes>,
        global_ctx: ArcGlobalCtx,
    ) -> Self {
        let conns: ConnMap = Arc::new(DashMap::new());
        let (close_event_sender, mut close_event_receiver) = mpsc::channel(10);
        let shutdown_notifier = Arc::new(tokio::sync::Notify::new());

        let conns_copy = conns.clone();
        let shutdown_notifier_copy = shutdown_notifier.clone();
        let global_ctx_copy = global_ctx.clone();
        let close_event_listener = tokio::spawn(
            async move {
                loop {
                    select! {
                        ret = close_event_receiver.recv() => {
                            if ret.is_none() {
                                break;
                            }
                            let ret = ret.unwrap();
                            tracing::warn!(
                                ?peer_node_id,
                                ?ret,
                                "notified that peer conn is closed",
                            );

                            if let Some((_, conn)) = conns_copy.remove(&ret) {
                                global_ctx_copy.issue_event(GlobalCtxEvent::PeerConnRemoved(
                                    conn.lock().await.get_conn_info(),
                                ));
                            }
                        }

                        _ = shutdown_notifier_copy.notified() => {
                            close_event_receiver.close();
                            tracing::warn!(?peer_node_id, "peer close event listener notified");
                        }
                    }
                }
                tracing::info!("peer {} close event listener exit", peer_node_id);
            }
            .instrument(tracing::info_span!(
                "peer_close_event_listener",
                ?peer_node_id,
            )),
        );

        Peer {
            peer_node_id,
            conns: conns.clone(),
            packet_recv_chan,
            global_ctx,

            close_event_sender,
            close_event_listener,

            shutdown_notifier,
        }
    }

    pub async fn add_peer_conn(&self, mut conn: PeerConn) {
        conn.set_close_event_sender(self.close_event_sender.clone());
        conn.start_recv_loop(self.packet_recv_chan.clone());
        conn.start_pingpong();
        self.global_ctx
            .issue_event(GlobalCtxEvent::PeerConnAdded(conn.get_conn_info()));
        self.conns
            .insert(conn.get_conn_id(), Arc::new(Mutex::new(conn)));
    }

    pub async fn send_msg(&self, msg: Bytes) -> Result<(), Error> {
        let Some(conn) = self.conns.iter().next() else {
            return Err(Error::PeerNoConnectionError(self.peer_node_id));
        };

        let conn_clone = conn.clone();
        drop(conn);
        conn_clone.lock().await.send_msg(msg).await?;

        Ok(())
    }

    pub async fn close_peer_conn(&self, conn_id: &Uuid) -> Result<(), Error> {
        let has_key = self.conns.contains_key(conn_id);
        if !has_key {
            return Err(Error::NotFound);
        }
        self.close_event_sender.send(conn_id.clone()).await.unwrap();
        Ok(())
    }

    pub async fn list_peer_conns(&self) -> Vec<PeerConnInfo> {
        let mut conns = vec![];
        for conn in self.conns.iter() {
            // do not lock here, otherwise it will cause dashmap deadlock
            conns.push(conn.clone());
        }

        let mut ret = Vec::new();
        for conn in conns {
            ret.push(conn.lock().await.get_conn_info());
        }
        ret
    }
}

// pritn on drop
impl Drop for Peer {
    fn drop(&mut self) {
        self.shutdown_notifier.notify_one();
        tracing::info!("peer {} drop", self.peer_node_id);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::{sync::mpsc, time::timeout};

    use crate::{
        common::{config_fs::ConfigFs, global_ctx::GlobalCtx, netns::NetNS},
        peers::peer_conn::PeerConn,
        tunnels::ring_tunnel::create_ring_tunnel_pair,
    };

    use super::Peer;

    #[tokio::test]
    async fn close_peer() {
        let (local_packet_send, _local_packet_recv) = mpsc::channel(10);
        let (remote_packet_send, _remote_packet_recv) = mpsc::channel(10);
        let global_ctx = Arc::new(GlobalCtx::new(
            "test",
            ConfigFs::new("/tmp/easytier-test"),
            NetNS::new(None),
        ));
        let local_peer = Peer::new(uuid::Uuid::new_v4(), local_packet_send, global_ctx.clone());
        let remote_peer = Peer::new(uuid::Uuid::new_v4(), remote_packet_send, global_ctx.clone());

        let (local_tunnel, remote_tunnel) = create_ring_tunnel_pair();
        let mut local_peer_conn =
            PeerConn::new(local_peer.peer_node_id, global_ctx.clone(), local_tunnel);
        let mut remote_peer_conn =
            PeerConn::new(remote_peer.peer_node_id, global_ctx.clone(), remote_tunnel);

        assert!(!local_peer_conn.handshake_done());
        assert!(!remote_peer_conn.handshake_done());

        let (a, b) = tokio::join!(
            local_peer_conn.do_handshake_as_client(),
            remote_peer_conn.do_handshake_as_server()
        );
        a.unwrap();
        b.unwrap();

        let local_conn_id = local_peer_conn.get_conn_id();

        local_peer.add_peer_conn(local_peer_conn).await;
        remote_peer.add_peer_conn(remote_peer_conn).await;

        assert_eq!(local_peer.list_peer_conns().await.len(), 1);
        assert_eq!(remote_peer.list_peer_conns().await.len(), 1);

        let close_handler =
            tokio::spawn(async move { local_peer.close_peer_conn(&local_conn_id).await });

        // wait for remote peer conn close
        timeout(std::time::Duration::from_secs(5), async {
            while (&remote_peer).list_peer_conns().await.len() != 0 {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .unwrap();

        println!("wait for close handler");
        close_handler.await.unwrap().unwrap();
    }
}
