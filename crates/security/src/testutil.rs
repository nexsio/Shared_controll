use transport::MemoryTransport;

use crate::{accept, initiate_pairing, Accepted, Identity, MemoryTrustStore, SecureSession};

/// Zwei gekoppelte Sitzungen (a, b) für Tests.
pub(crate) async fn sessions() -> (SecureSession, SecureSession) {
    let (mut ta, mut tb) = MemoryTransport::pair(512);
    let (ida, idb) = (Identity::generate().unwrap(), Identity::generate().unwrap());
    let (mut store_a, mut store_b) = (MemoryTrustStore::default(), MemoryTrustStore::default());

    let (pa, pb) = tokio::join!(initiate_pairing(&mut ta, &ida), accept(&mut tb, &idb, &store_b));
    let Accepted::Pairing(pb) = pb.unwrap() else { panic!("Pairing erwartet") };
    (pa.unwrap().confirm(&mut store_a), pb.confirm(&mut store_b))
}
