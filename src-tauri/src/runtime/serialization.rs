//! Measure serialized UTF-8 without materializing an uncharged JSON buffer.
use serde::Serialize;
use std::io::{self, Write};

pub fn json_bytes<T: Serialize + ?Sized>(value: &T) -> Result<usize, serde_json::Error> {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .ok_or_else(|| io::Error::other("serialized size overflow"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut count = Counter(0);
    serde_json::to_writer(&mut count, value)?;
    Ok(count.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn counts_utf8_escaping_and_streamed_containers_without_an_output_buffer() {
        let values = serde_json::json!({
            "name": "한글😀\n\t\"\\\u{0000}",
            "array": [null, false, 9007199254740993_u64, {"empty": []}]
        });
        assert_eq!(
            json_bytes(&values).unwrap(),
            serde_json::to_vec(&values).unwrap().len()
        );
        assert_eq!(
            json_bytes("\n한글").unwrap(),
            serde_json::to_vec("\n한글").unwrap().len()
        );
        struct Stream;
        impl Serialize for Stream {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_seq(std::iter::repeat_n(0, 100_000))
            }
        }
        assert_eq!(json_bytes(&Stream).unwrap(), 200_001);
        struct Failure;
        impl Serialize for Failure {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("invalid fixture"))
            }
        }
        assert!(json_bytes(&Failure).is_err());
    }
}
