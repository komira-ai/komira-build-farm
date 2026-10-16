//! The body of [`ObjectStore::put_stream`](crate::ObjectStore::put_stream): an object's
//! bytes in order, in pieces of any size, so a writer never holds the object whole.

use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use futures::stream::BoxStream;

use crate::ObjectStoreError;

/// An object's bytes, in order, in pieces of any size. An `Err` piece ends the write:
/// nothing is stored, and that error is the write's.
pub type BodyStream = BoxStream<'static, Result<Bytes, ObjectStoreError>>;

/// The most bytes [`gather`] reserves before the first piece arrives: a body declared
/// larger grows its buffer as its pieces come.
const GATHER_RESERVE: u64 = 16 << 20;

/// The body's `len` bytes, gathered whole: what a store with no streamed write does.
///
/// # Errors
/// A piece's error, or [`ObjectStoreError::Body`] when the body gives more or fewer
/// than `len` bytes.
pub async fn gather(mut body: BodyStream, len: u64) -> Result<Bytes, ObjectStoreError> {
    let reserve = usize::try_from(len.min(GATHER_RESERVE)).unwrap_or(0);
    let mut out = BytesMut::with_capacity(reserve);
    while let Some(piece) = body.next().await {
        let piece = piece?;
        if out.len() as u64 + piece.len() as u64 > len {
            return Err(too_long(len));
        }
        out.extend_from_slice(&piece);
    }
    if (out.len() as u64) < len {
        return Err(too_short(len, out.len() as u64));
    }
    Ok(out.freeze())
}

/// A body that gave more bytes than its declared `len`.
pub(crate) fn too_long(len: u64) -> ObjectStoreError {
    ObjectStoreError::Body(format!("the body gave more than its {len} bytes"))
}

/// A body that ended after `got` of its declared `len` bytes.
pub(crate) fn too_short(len: u64, got: u64) -> ObjectStoreError {
    ObjectStoreError::Body(format!("the body ended after {got} of its {len} bytes"))
}

/// Pieces of a body taken in parts of a given size, whatever sizes its pieces came in.
pub(crate) struct Parts {
    body: BodyStream,
    /// What the last piece had left over past the part it ended.
    carry: Bytes,
    /// Bytes taken so far.
    taken: u64,
    len: u64,
}

impl Parts {
    pub(crate) fn new(body: BodyStream, len: u64) -> Self {
        Self {
            body,
            carry: Bytes::new(),
            taken: 0,
            len,
        }
    }

    /// The next `size` bytes of the body (fewer for the last part), gathered into one
    /// buffer; `None` once all `len` bytes were taken and the body has ended.
    ///
    /// # Errors
    /// A piece's error, or [`ObjectStoreError::Body`] when the body gives more or fewer
    /// than `len` bytes.
    pub(crate) async fn next(&mut self, size: u64) -> Result<Option<Bytes>, ObjectStoreError> {
        let want = size.min(self.len - self.taken);
        if want == 0 {
            // Every byte was taken: the body must end here, with nothing left over.
            if !self.carry.is_empty() {
                return Err(too_long(self.len));
            }
            while let Some(piece) = self.body.next().await {
                if !piece?.is_empty() {
                    return Err(too_long(self.len));
                }
            }
            return Ok(None);
        }
        // `want` is at most `size`, a part size that fits in memory.
        let want = want as usize;
        let mut part = BytesMut::with_capacity(want);
        loop {
            let need = want - part.len();
            if self.carry.len() >= need {
                part.extend_from_slice(&self.carry.split_to(need));
                break;
            }
            part.extend_from_slice(&self.carry);
            self.carry = match self.body.next().await {
                Some(piece) => piece?,
                None => {
                    let got = self.taken + part.len() as u64;
                    return Err(too_short(self.len, got));
                }
            };
        }
        self.taken += want as u64;
        Ok(Some(part.freeze()))
    }
}

#[cfg(test)]
mod tests {
    use futures::stream;

    use super::*;

    fn body(pieces: &[&'static [u8]]) -> BodyStream {
        let pieces: Vec<Result<Bytes, ObjectStoreError>> =
            pieces.iter().map(|p| Ok(Bytes::from_static(p))).collect();
        stream::iter(pieces).boxed()
    }

    fn parts(pieces: &[&'static [u8]], len: u64, size: u64) -> Result<Vec<Bytes>, String> {
        futures::executor::block_on(async {
            let mut parts = Parts::new(body(pieces), len);
            let mut out = Vec::new();
            while let Some(part) = parts.next(size).await.map_err(|e| e.to_string())? {
                out.push(part);
            }
            Ok(out)
        })
    }

    /// Catches: parts cut where the pieces end rather than at the part size, bytes lost
    /// or repeated across a piece that spans two parts, an empty piece taken for the
    /// end, and a last part padded to the part size.
    #[test]
    fn parts_are_cut_at_the_part_size_whatever_the_pieces() {
        let got = parts(&[b"abc", b"", b"defgh", b"ij"], 10, 4).unwrap();
        assert_eq!(got, [&b"abcd"[..], b"efgh", b"ij"]);
        let one = parts(&[b"abcdefghij"], 10, 4).unwrap();
        assert_eq!(one, [&b"abcd"[..], b"efgh", b"ij"]);
        let exact = parts(&[b"ab", b"cd"], 4, 4).unwrap();
        assert_eq!(exact, [&b"abcd"[..]]);
        let trailing = parts(&[b"abcd", b"", b""], 4, 4).unwrap();
        assert_eq!(
            trailing,
            [&b"abcd"[..]],
            "empty pieces after the end are no bytes"
        );
    }

    /// Catches: a body longer or shorter than declared written as if it were whole
    /// (a truncated object in the store under a key that names its length).
    #[test]
    fn a_body_of_the_wrong_length_is_refused() {
        let long = parts(&[b"abcd", b"e"], 4, 4).unwrap_err();
        assert!(long.contains("more than its 4 bytes"), "{long}");
        let long_in_carry = parts(&[b"abcde"], 4, 2).unwrap_err();
        assert!(long_in_carry.contains("more than"), "{long_in_carry}");
        let short = parts(&[b"abc"], 4, 2).unwrap_err();
        assert!(short.contains("ended after 3 of its 4 bytes"), "{short}");
        let gathered = futures::executor::block_on(gather(body(&[b"ab", b"c"]), 3));
        assert_eq!(gathered.unwrap(), Bytes::from_static(b"abc"));
        let over = futures::executor::block_on(gather(body(&[b"ab", b"cd"]), 3));
        assert!(matches!(over, Err(ObjectStoreError::Body(_))), "{over:?}");
        let under = futures::executor::block_on(gather(body(&[b"ab"]), 3));
        assert!(matches!(under, Err(ObjectStoreError::Body(_))), "{under:?}");
    }

    /// Catches: a piece's error swallowed (the write completed over a failed read).
    #[test]
    fn a_failed_piece_fails_the_write() {
        let failing: BodyStream = stream::iter(vec![
            Ok(Bytes::from_static(b"ab")),
            Err(ObjectStoreError::Body("the source broke".into())),
        ])
        .boxed();
        let got = futures::executor::block_on(async {
            let mut parts = Parts::new(failing, 4);
            parts.next(4).await
        });
        let why = got.err().map(|e| e.to_string());
        assert_eq!(
            why.as_deref(),
            Some("the body of a streamed write failed: the source broke")
        );
    }
}
