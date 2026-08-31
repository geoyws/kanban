use std::io::{self, Read};

pub(crate) const STREAM_LIMIT: usize = 1 << 20;

#[derive(Debug, PartialEq)]
pub(crate) struct BoundedOutput {
    pub(crate) bytes: Vec<u8>,
    pub(crate) overflowed: bool,
}

pub(crate) fn drain_bounded<R: Read>(mut reader: R) -> io::Result<BoundedOutput> {
    let mut bytes = Vec::new();
    let mut overflowed = false;
    let mut chunk = [0_u8; 8192];
    loop {
        let count = reader.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        let remaining = STREAM_LIMIT.saturating_sub(bytes.len());
        let retained = remaining.min(count);
        bytes.extend_from_slice(&chunk[..retained]);
        overflowed |= retained < count;
    }
    Ok(BoundedOutput { bytes, overflowed })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Error, ErrorKind};

    #[test]
    fn bounded_drain_handles_empty_and_short_streams() {
        assert_eq!(
            drain_bounded(Cursor::new(Vec::<u8>::new())).unwrap(),
            BoundedOutput {
                bytes: Vec::new(),
                overflowed: false,
            }
        );
        assert_eq!(
            drain_bounded(Cursor::new(b"hello")).unwrap(),
            BoundedOutput {
                bytes: b"hello".to_vec(),
                overflowed: false,
            }
        );
    }

    #[test]
    fn bounded_drain_accepts_exactly_the_limit() {
        let input = vec![b'a'; STREAM_LIMIT];
        let output = drain_bounded(Cursor::new(&input)).unwrap();
        assert_eq!(output.bytes, input);
        assert!(!output.overflowed);
    }

    #[test]
    fn bounded_drain_marks_one_byte_over_and_keeps_only_the_limit() {
        let input = vec![b'b'; STREAM_LIMIT + 1];
        let output = drain_bounded(Cursor::new(&input)).unwrap();
        assert_eq!(output.bytes, input[..STREAM_LIMIT]);
        assert!(output.overflowed);
    }

    #[test]
    fn bounded_drain_continues_through_a_much_larger_stream() {
        let input = vec![b'c'; STREAM_LIMIT * 3 + 17];
        let output = drain_bounded(Cursor::new(&input)).unwrap();
        assert_eq!(output.bytes.len(), STREAM_LIMIT);
        assert!(output.bytes.iter().all(|byte| *byte == b'c'));
        assert!(output.overflowed);
    }

    struct FailingReader {
        first: bool,
    }

    impl Read for FailingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.first {
                self.first = false;
                buffer[..3].copy_from_slice(b"abc");
                Ok(3)
            } else {
                Err(Error::new(ErrorKind::Other, "fixture failure"))
            }
        }
    }

    #[test]
    fn bounded_drain_propagates_reader_errors() {
        let error = drain_bounded(FailingReader { first: true }).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Other);
    }
}
