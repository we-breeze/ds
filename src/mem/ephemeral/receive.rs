//! Direct, length-bounded AsyncRead into an arena allocation (or its fallback).
//! The unsafe initialized-length boundary is contained in brz-ds.

use std::io;
use std::mem::MaybeUninit;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};

use super::{EphemeralBytesMut, Storage};

impl EphemeralBytesMut {
    /// Append at most `maximum` bytes directly from `source`, without a scratch
    /// buffer or a payload copy. Only a successful read advances the length.
    ///
    /// A Pending/error result does not publish any newly initialized bytes.
    /// Cancelling the enclosing future therefore leaves the visible prefix valid.
    /// With no remaining capacity (or maximum == 0), returns Ok(0) without polling.
    pub fn poll_read_from<R: AsyncRead + ?Sized>(
        &mut self,
        cx: &mut Context<'_>,
        mut source: Pin<&mut R>,
        maximum: usize,
    ) -> Poll<io::Result<usize>> {
        let available = maximum.min(self.remaining());
        if available == 0 {
            return Poll::Ready(Ok(0));
        }
        let offset = self.len;
        let result = {
            let spare = match self.storage_mut() {
                Storage::Arena(allocation) => {
                    // SAFETY: &mut self exclusively borrows this move-only
                    // allocation. The bounded cursor/ticket owns a disjoint range.
                    // Expose only spare capacity as MaybeUninit, never as &[u8].
                    unsafe {
                        std::slice::from_raw_parts_mut(
                            allocation
                                .as_mut_ptr()
                                .add(offset)
                                .cast::<MaybeUninit<u8>>(),
                            available,
                        )
                    }
                }
                Storage::Heap(bytes) => &mut bytes.spare_capacity_mut()[..available],
            };
            let mut output = ReadBuf::uninit(spare);
            let pointer = output.filled().as_ptr();
            match source.as_mut().poll_read(cx, &mut output) {
                Poll::Ready(Ok(())) => {
                    // AsyncRead is a safe trait. A broken implementation can
                    // replace ReadBuf; never trust bytes from a different buffer
                    // as initialization of our allocation (same guard as read_buf).
                    if output.filled().as_ptr() != pointer || output.filled().len() > available {
                        Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "AsyncRead replaced the destination buffer",
                        )))
                    } else {
                        Poll::Ready(Ok(output.filled().len()))
                    }
                }
                Poll::Ready(Err(error)) => Poll::Ready(Err(error)),
                Poll::Pending => Poll::Pending,
            }
        };
        if let Poll::Ready(Ok(count)) = &result {
            let end = offset + *count;
            if let Storage::Heap(bytes) = self.storage_mut() {
                // SAFETY: ReadBuf::filled guarantees [offset, end) initialized;
                // the existing prefix was initialized before this call.
                unsafe { bytes.set_len(end) };
            }
            self.len = end;
        }
        result
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;
    use crate::EphemeralBytesArena;

    struct Input {
        data: &'static [u8],
        pending: bool,
        fail: bool,
    }
    impl AsyncRead for Input {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            out: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.pending {
                self.pending = false;
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }
            if self.fail {
                return Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()));
            }
            let n = self.data.len().min(out.remaining());
            out.put_slice(&self.data[..n]);
            self.data = &self.data[n..];
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn direct_read_checks_bounds_pending_eof_and_heap_fallback() {
        let arena = EphemeralBytesArena::new(16);
        for capacity in [8, 32] {
            let mut frame = arena.alloc(capacity);
            frame.extend_from_slice(b"xy");
            let pointer = frame.as_slice().as_ptr();
            let mut input = Input {
                data: b"abc",
                pending: true,
                fail: false,
            };
            let mut cx = Context::from_waker(std::task::Waker::noop());
            assert!(
                frame
                    .poll_read_from(&mut cx, Pin::new(&mut input), 2)
                    .is_pending()
            );
            assert_eq!(frame.as_slice(), b"xy");
            assert!(matches!(
                frame.poll_read_from(&mut cx, Pin::new(&mut input), 2),
                Poll::Ready(Ok(2))
            ));
            assert_eq!(frame.as_slice(), b"xyab");
            assert_eq!(frame.as_slice().as_ptr(), pointer);
            assert!(matches!(
                frame.poll_read_from(&mut cx, Pin::new(&mut input), 9),
                Poll::Ready(Ok(1))
            ));
            assert!(matches!(
                frame.poll_read_from(&mut cx, Pin::new(&mut input), 9),
                Poll::Ready(Ok(0))
            ));
            input.fail = true;
            assert!(matches!(
                frame.poll_read_from(&mut cx, Pin::new(&mut input), 9),
                Poll::Ready(Err(_))
            ));
            assert_eq!(frame.freeze().as_slice(), b"xyabc");
        }
    }

    struct ReplacesBuffer(Option<&'static mut [u8]>);
    impl AsyncRead for ReplacesBuffer {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            out: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let replacement = self.0.take().expect("only one poll");
            *out = ReadBuf::new(replacement);
            out.advance(1);
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn a_safe_but_broken_reader_cannot_publish_uninitialized_arena_bytes() {
        let arena = EphemeralBytesArena::new(16);
        let mut frame = arena.alloc(8);
        frame.extend_from_slice(b"xy");
        let allocation = Box::into_raw(Box::new([b'z'; 1]));
        let result = {
            // SAFETY (test fixture only): the allocation is kept live until
            // source and the temporary ReadBuf have both been dropped below.
            let replacement: &'static mut [u8] = unsafe { &mut *allocation };
            let mut source = ReplacesBuffer(Some(replacement));
            let mut cx = Context::from_waker(std::task::Waker::noop());
            frame.poll_read_from(&mut cx, Pin::new(&mut source), 4)
        };
        // SAFETY: recover the original Box exactly once; no borrowed output
        // survives the rejected read. Avoid leaking memory in the Miri fixture.
        unsafe { drop(Box::from_raw(allocation)) };
        assert!(matches!(result, Poll::Ready(Err(_))));
        assert_eq!(frame.as_slice(), b"xy");
    }
}

#[cfg(all(test, not(loom)))]
mod failure_tests {
    use super::*;
    use crate::EphemeralBytesArena;

    struct WritesThenFails(u8);
    impl AsyncRead for WritesThenFails {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            out: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            out.put_slice(b"z");
            match self.0 {
                0 => Poll::Pending,
                1 => Poll::Ready(Err(io::ErrorKind::Interrupted.into())),
                _ => panic!("reader panic after initialization"),
            }
        }
    }

    #[test]
    fn failed_or_cancelled_reads_do_not_publish_initialized_suffix() {
        for capacity in [8, 32] {
            let arena = EphemeralBytesArena::new(16);
            let mut frame = arena.alloc(capacity);
            frame.extend_from_slice(b"xy");
            let mut cx = Context::from_waker(std::task::Waker::noop());
            for mode in 0..3 {
                let mut source = WritesThenFails(mode);
                // Zero maximum must not poll even a panicking source.
                assert!(matches!(
                    frame.poll_read_from(&mut cx, Pin::new(&mut source), 0),
                    Poll::Ready(Ok(0))
                ));
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    frame.poll_read_from(&mut cx, Pin::new(&mut source), 1)
                }));
                assert_eq!(result.is_err(), mode == 2);
                assert_eq!(frame.as_slice(), b"xy");
            }
            let mut valid = &b"abc"[..];
            assert!(matches!(
                frame.poll_read_from(&mut cx, Pin::new(&mut valid), 3),
                Poll::Ready(Ok(3))
            ));
            assert_eq!(frame.as_slice(), b"xyabc");
        }
    }
}
