//! Transport im selben Prozess für Tests der höheren Schichten, ohne Bluetooth.

use tokio::sync::mpsc;

use crate::{ConnectionState, Transport, TransportError, TransportStatus};

const QUEUE_LEN: usize = 64;

pub struct MemoryTransport {
    tx: Option<mpsc::Sender<Vec<u8>>>,
    rx: mpsc::Receiver<Vec<u8>>,
    state: ConnectionState,
    max_frame_len: usize,
}

impl MemoryTransport {
    /// Erzeugt zwei verbundene Endpunkte.
    pub fn pair(max_frame_len: usize) -> (Self, Self) {
        let (a_tx, b_rx) = mpsc::channel(QUEUE_LEN);
        let (b_tx, a_rx) = mpsc::channel(QUEUE_LEN);
        let make = |tx, rx| Self { tx: Some(tx), rx, state: ConnectionState::Connected, max_frame_len };
        (make(a_tx, a_rx), make(b_tx, b_rx))
    }

    // Ein getrennter In-Memory-Endpunkt kann nicht neu aufgebaut werden.
    fn resume(&self) -> Result<(), TransportError> {
        if self.state == ConnectionState::Connected {
            Ok(())
        } else {
            Err(TransportError::Closed)
        }
    }
}

impl Transport for MemoryTransport {
    async fn connect(&mut self) -> Result<(), TransportError> {
        self.resume()
    }

    async fn disconnect(&mut self) -> Result<(), TransportError> {
        self.tx = None;
        self.rx.close();
        self.state = ConnectionState::Disconnected;
        Ok(())
    }

    async fn send(&mut self, frame: &[u8]) -> Result<(), TransportError> {
        if self.state != ConnectionState::Connected {
            return Err(TransportError::NotConnected);
        }
        if frame.len() > self.max_frame_len {
            return Err(TransportError::FrameTooLarge { max: self.max_frame_len });
        }
        let Some(tx) = &self.tx else {
            return Err(TransportError::NotConnected);
        };
        if tx.send(frame.to_vec()).await.is_err() {
            self.state = ConnectionState::Disconnected;
            return Err(TransportError::Closed);
        }
        Ok(())
    }

    async fn receive(&mut self, buf: &mut [u8]) -> Result<usize, TransportError> {
        if self.state != ConnectionState::Connected {
            return Err(TransportError::NotConnected);
        }
        let Some(frame) = self.rx.recv().await else {
            self.state = ConnectionState::Disconnected;
            return Err(TransportError::Closed);
        };
        if frame.len() > buf.len() {
            return Err(TransportError::BufferTooSmall { needed: frame.len() });
        }
        buf[..frame.len()].copy_from_slice(&frame);
        Ok(frame.len())
    }

    fn get_status(&self) -> TransportStatus {
        TransportStatus { state: self.state, max_frame_len: self.max_frame_len }
    }

    async fn reconnect(&mut self) -> Result<(), TransportError> {
        self.resume()
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn frames_keep_boundaries_in_both_directions() {
        let (mut a, mut b) = MemoryTransport::pair(512);
        let mut buf = [0u8; 512];

        a.send(&[1, 2, 3]).await.unwrap();
        a.send(&[4]).await.unwrap();
        assert_eq!(b.receive(&mut buf).await, Ok(3));
        assert_eq!(&buf[..3], &[1, 2, 3]);
        assert_eq!(b.receive(&mut buf).await, Ok(1));

        b.send(&[9, 9]).await.unwrap();
        assert_eq!(a.receive(&mut buf).await, Ok(2));
    }

    #[tokio::test]
    async fn rejects_oversized_frame_and_small_buffer() {
        let (mut a, mut b) = MemoryTransport::pair(4);
        assert_eq!(a.send(&[0; 5]).await, Err(TransportError::FrameTooLarge { max: 4 }));

        a.send(&[1, 2, 3, 4]).await.unwrap();
        let mut tiny = [0u8; 2];
        assert_eq!(b.receive(&mut tiny).await, Err(TransportError::BufferTooSmall { needed: 4 }));
    }

    #[tokio::test]
    async fn peer_drop_is_reported_as_closed() {
        let (mut a, b) = MemoryTransport::pair(16);
        drop(b);
        assert_eq!(a.send(&[1]).await, Err(TransportError::Closed));
        assert_eq!(a.get_status().state, ConnectionState::Disconnected);
        assert_eq!(a.send(&[1]).await, Err(TransportError::NotConnected));
        assert_eq!(a.reconnect().await, Err(TransportError::Closed));
    }

    #[tokio::test]
    async fn receive_reports_closed_when_peer_disconnects() {
        let (mut a, mut b) = MemoryTransport::pair(16);
        a.disconnect().await.unwrap();
        let mut buf = [0u8; 16];
        assert_eq!(b.receive(&mut buf).await, Err(TransportError::Closed));
        assert_eq!(a.receive(&mut buf).await, Err(TransportError::NotConnected));
    }

    #[tokio::test]
    async fn works_through_the_trait_across_tasks() {
        async fn echo_once<T: Transport + 'static>(mut t: T) {
            let mut buf = [0u8; 16];
            let n = t.receive(&mut buf).await.unwrap();
            t.send(&buf[..n]).await.unwrap();
        }

        let (mut a, b) = MemoryTransport::pair(16);
        let task = tokio::spawn(echo_once(b));
        a.send(&[7, 8]).await.unwrap();
        let mut buf = [0u8; 16];
        let n = tokio::time::timeout(Duration::from_secs(2), a.receive(&mut buf)).await.unwrap().unwrap();
        assert_eq!(&buf[..n], &[7, 8]);
        task.await.unwrap();
    }
}
