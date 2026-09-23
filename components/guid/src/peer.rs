use anyhow::{Context, Result};
use vsock::VsockStream;

/// CID пира берём у ядра, а не из данных гостя — это и есть аутентификация спейса.
pub fn peer_cid(stream: &VsockStream) -> Result<u32> {
    let addr = stream.peer_addr().context("не читается peer_addr vsock")?;
    Ok(addr.cid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use vsock::{VsockAddr, VsockListener, VsockStream, VMADDR_CID_LOCAL};

    // требует загруженного модуля vsock_loopback
    #[test]
    fn reads_peer_cid_of_accepted_connection() {
        let listener = VsockListener::bind(&VsockAddr::new(VMADDR_CID_LOCAL, 9999)).unwrap();
        thread::spawn(|| {
            let _ = VsockStream::connect(&VsockAddr::new(VMADDR_CID_LOCAL, 9999));
        });
        let (stream, _) = listener.accept().unwrap();
        assert_eq!(peer_cid(&stream).unwrap(), VMADDR_CID_LOCAL);
    }
}
