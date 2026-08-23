//! Reader-adapter integration tests (sync Read and futures-io AsyncRead).

use graphy_turtle::{Options, TurtleParser};

const TTL: &str = "@prefix ex: <http://x/> .\nex:s ex:p ex:o , \"v\" .\n";

#[test]
fn sync_read_adapter() {
    // A reader that returns one byte at a time exercises resumption hard.
    struct TrickleReader<'a>(&'a [u8]);
    impl std::io::Read for TrickleReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            match self.0.split_first() {
                Some((&b, rest)) => {
                    buf[0] = b;
                    self.0 = rest;
                    Ok(1)
                }
                None => Ok(0),
            }
        }
    }
    let mut p = TurtleParser::new(Options::default()).unwrap();
    let mut n = 0;
    p.read_from(TrickleReader(TTL.as_bytes()), |_q| n += 1)
        .unwrap();
    assert_eq!(n, 2);
}

#[cfg(feature = "async")]
#[test]
fn async_read_adapter() {
    use std::pin::Pin;
    use std::task::{Context, Poll, Waker};

    use graphy_turtle::NQuadsParser;

    /// AsyncRead over a slice, alternating Pending/Ready to exercise polling.
    struct SliceAsyncRead<'a> {
        data: &'a [u8],
        pending_next: bool,
    }
    impl futures_io::AsyncRead for SliceAsyncRead<'_> {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            if self.pending_next {
                self.pending_next = false;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            self.pending_next = true;
            let n = self.data.len().min(buf.len()).min(7);
            buf[..n].copy_from_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Poll::Ready(Ok(n))
        }
    }

    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut fut = std::pin::pin!(fut);
        loop {
            if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                return v;
            }
        }
    }

    let nq = "<http://x/s> <http://x/p> \"a\" <http://x/g> .\n<http://x/s> <http://x/p> \"b\" .\n";
    let mut p = NQuadsParser::new(Options::default()).unwrap();
    let mut n = 0;
    block_on(p.read_from_async(
        SliceAsyncRead {
            data: nq.as_bytes(),
            pending_next: false,
        },
        |_q| n += 1,
    ))
    .unwrap();
    assert_eq!(n, 2);
}
