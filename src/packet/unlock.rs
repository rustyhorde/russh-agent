// Copyright (c) 2020 russh-agent developers
//
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. All files in the project carrying such notice may not be copied,
// modified, or distributed except according to those terms.

//! `russh-agent` unlock request

use crate::{
    error::Result,
    packet::{IntoPacket, Packet, PacketKind},
    utils::put_string,
};
use bytes::{BufMut, Bytes, BytesMut};

#[derive(Clone, Debug)]
crate struct Unlock {
    passphrase: Bytes,
}

impl IntoPacket for Unlock {
    fn into_packet(&self) -> Result<Packet> {
        let mut pkt = Packet::default();

        let kind = PacketKind::Unlock;
        let _ = pkt.set_kind(kind.clone());

        let mut payload = BytesMut::new();
        payload.put_u8(kind.into());
        put_string(&mut payload, &self.passphrase)?;

        let _ = pkt.set_payload(payload.freeze());

        Ok(pkt)
    }
}

impl Unlock {
    crate fn new(passphrase: Bytes) -> Self {
        Self { passphrase }
    }
}

#[cfg(test)]
mod test {
    use super::Unlock;
    use crate::{
        error::Result,
        packet::{IntoPacket, Packet, PacketKind},
    };
    use bytes::Bytes;

    #[test]
    fn unlock() -> Result<()> {
        let lock = Unlock::new(Bytes::from_static(b"test"));
        let pkt = lock.into_packet()?;
        let mut expected = Packet::default();
        let _ = expected.set_kind(PacketKind::Unlock);
        let _ = expected.set_payload(Bytes::from_static(&[
            23, 0, 0, 0, 4, b't', b'e', b's', b't',
        ]));
        assert_eq!(pkt, expected);
        Ok(())
    }
}
