use chrono::Duration;

use libp2p_identity::Keypair;
use rust_ipns::Record;

fn main() -> std::io::Result<()> {
    let keypair = Keypair::generate_ed25519();

    let record = Record::new(
        &keypair,
        b"/path/cid",
        Duration::try_seconds(60).unwrap(),
        0,
        0,
    )?;

    let peer_id = keypair.public().to_peer_id();
    record.verify(peer_id)?;

    Ok(())
}
