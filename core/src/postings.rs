//! The v3 posting list: blocks of up to 128 postings, doc deltas bit-packed, term
//! frequencies folded into the block header and a 2-bit code stream.
//!
//! ```text
//! block := u8 header, packed doc deltas, [2-bit tf codes], [varint tf-1 escapes],
//!          [varint position deltas when the artifact carries positions]
//!
//! header bits 0..5   bit width of the packed doc deltas (0..=32)
//! header bits 6..7   tf mode:
//!                      0  every posting has tf == 1, no tf stream at all
//!                      1  2-bit codes (0..2 => tf 1..3, 3 => escape, varint follows)
//!                      2  every posting carries varint(tf-1)
//!                      3  reserved, corrupt
//! ```
//!
//! The deltas are gap-encoded exactly as before: the first posting of a list is an
//! absolute doc id, every following one is `doc - previous`. The tf stream is the
//! larger half of a prose corpus's postings (91% of postings have tf 1–4 on the
//! repository's 10k-document corpus), and one byte per posting for a value that
//! fits in two bits is the obvious waste. Mode 1 spends two bits on the common
//! case and a LEB128 escape on the tail; the builder picks the cheaper of mode 1
//! and mode 2 per block, so a block of long repeats still costs one byte per tf.
//!
//! **Positions (F6).** A build given `--positions` writes one run of token ordinals
//! after the tf stream of every block: `tf` varints per posting, in posting order.
//! The first value of a run is the absolute ordinal, every following one is
//! `position - previous - 1` (a term never repeats at the same ordinal, so the gap
//! is at least one). The header flag `FLAG_HAS_POSITIONS` says whether the stream is
//! there, so a reader either decodes it or never touches those bytes: a query
//! without a phrase pays exactly what it paid before. The ordinals are counted over
//! the whole token stream — stopwords included — so a phrase keeps its gaps when
//! the analyzer drops a word (`"index of the shard"` requires three tokens between
//! the two indexed ones). A corrupt run fails the same way a corrupt delta does: a
//! non-monotonic position, an ordinal above [`MAX_POSITION`], a truncated varint or
//! trailing bytes are an [`Error::OutOfBounds`], never a panic or an allocation the
//! size of a corrupt header.
//!
//! Every read is bounds-checked and every value is range-checked by the caller, so
//! a corrupt artifact is an [`Error::OutOfBounds`], never a panic or an allocation
//! the size of a corrupt header.
use alloc::vec::Vec;

use crate::error::Error;
use crate::varint::{encoded_len, read_u32, write_u32};

/// Postings per block. A constant of the format: the builder and the reader share it.
pub const BLOCK: usize = 128;
/// The largest term frequency a posting may carry.
pub const MAX_TF: u32 = 100_000;
/// The largest token ordinal a position may carry. A document text is bounded, so a
/// real ordinal is far below this; a corrupt stream that is not is refused before it
/// can be treated as an offset.
pub const MAX_POSITION: u32 = 1 << 28;

const WIDTH_MASK: u8 = 0x3f;
const TF_SHIFT: u8 = 6;
/// Every tf in the block is 1.
const TF_ALL_ONE: u8 = 0;
/// 2-bit codes, `3` escapes to a varint.
const TF_SMALL: u8 = 1;
/// varint(tf-1) for every posting.
const TF_VARINT: u8 = 2;
/// The largest tf a 2-bit code expresses directly.
const TF_SMALL_MAX: u32 = 3;

/// One decoded block: the posting fields plus the position runs, in one buffer.
///
/// The runs are a flat `Vec` with `starts[i]..starts[i + 1]` naming posting `i`, so
/// the decoder allocates once per list and not once per posting.
struct Block {
    docs: [u32; BLOCK],
    tfs: [u32; BLOCK],
    starts: [u32; BLOCK + 1],
    positions: Vec<u32>,
    len: usize,
}

impl Block {
    fn new() -> Self {
        Self {
            docs: [0; BLOCK],
            tfs: [0; BLOCK],
            starts: [0; BLOCK + 1],
            positions: Vec::new(),
            len: 0,
        }
    }

    fn run(&self, index: usize) -> &[u32] {
        let from = self.starts[index] as usize;
        let to = self.starts[index + 1] as usize;
        self.positions.get(from..to).unwrap_or(&[])
    }
}

/// Decode one block into `block`, advancing `pos` past its bytes.
///
/// `previous` is the last doc id already decoded, so the first delta of a list is
/// absolute and every later one is a gap. Positions are only read when
/// `with_positions` is set; a caller that does not want them never looks at those
/// bytes, which is what keeps a phrase-free query free.
fn decode_block(
    bytes: &[u8],
    pos: &mut usize,
    count: usize,
    with_positions: bool,
    previous: Option<u32>,
    block: &mut Block,
) -> Result<(), Error> {
    let header = *bytes.get(*pos).ok_or(Error::OutOfBounds)?;
    *pos += 1;
    let mode = header >> TF_SHIFT;
    let width = (header & WIDTH_MASK) as usize;
    if width > 32 || mode > TF_VARINT {
        return Err(Error::OutOfBounds);
    }
    let packed_len = (count * width).div_ceil(8);
    let end = (*pos).checked_add(packed_len).ok_or(Error::OutOfBounds)?;
    let deltas = bytes.get(*pos..end).ok_or(Error::OutOfBounds)?;
    *pos = end;
    let codes = if mode == TF_SMALL {
        let codes_len = (count * 2).div_ceil(8);
        let end = (*pos).checked_add(codes_len).ok_or(Error::OutOfBounds)?;
        let codes = bytes.get(*pos..end).ok_or(Error::OutOfBounds)?;
        *pos = end;
        Some(codes)
    } else {
        None
    };
    // A running bit buffer, refilled one byte at a time, decodes the values in
    // order: no per-value bit loop and no random access, which is what keeps
    // this at the speed of the LEB128 walk it replaced.
    let mask: u64 = if width == 32 {
        u32::MAX as u64
    } else {
        (1u64 << width) - 1
    };
    let mut delta_window: u64 = 0;
    let mut delta_bits: u32 = 0;
    let mut delta_at = 0usize;
    let mut code_window: u64 = 0;
    let mut code_bits: u32 = 0;
    let mut code_at = 0usize;
    let mut last = previous;
    for index in 0..count {
        let delta = if width == 0 {
            0
        } else {
            while delta_bits < width as u32 {
                delta_window |= (deltas.get(delta_at).copied().unwrap_or(0) as u64) << delta_bits;
                delta_at += 1;
                delta_bits += 8;
            }
            let value = (delta_window & mask) as u32;
            delta_window >>= width;
            delta_bits -= width as u32;
            value
        };
        let doc = match last {
            None => delta,
            Some(previous) => {
                if delta == 0 {
                    return Err(Error::OutOfBounds);
                }
                previous.checked_add(delta).ok_or(Error::OutOfBounds)?
            }
        };
        let tf = match mode {
            TF_ALL_ONE => 1,
            TF_SMALL => {
                let codes = codes.unwrap_or(&[]);
                while code_bits < 2 {
                    code_window |= (codes.get(code_at).copied().unwrap_or(0) as u64) << code_bits;
                    code_at += 1;
                    code_bits += 8;
                }
                let code = (code_window & 3) as u32;
                code_window >>= 2;
                code_bits -= 2;
                if code == TF_SMALL_MAX {
                    read_u32(bytes, pos)
                        .and_then(|value| value.checked_add(1))
                        .ok_or(Error::OutOfBounds)?
                } else {
                    code + 1
                }
            }
            _ => read_u32(bytes, pos)
                .and_then(|value| value.checked_add(1))
                .ok_or(Error::OutOfBounds)?,
        };
        if tf == 0 || tf > MAX_TF {
            return Err(Error::OutOfBounds);
        }
        block.docs[index] = doc;
        block.tfs[index] = tf;
        last = Some(doc);
    }
    block.len = count;
    block.positions.clear();
    if !with_positions {
        for slot in block.starts.iter_mut().take(count + 1) {
            *slot = 0;
        }
        return Ok(());
    }
    // The runs come after every tf byte, so the walk above has already consumed the
    // escapes; `tf` varints per posting, in posting order.
    for index in 0..count {
        block.starts[index] = block.positions.len() as u32;
        let mut previous_position: Option<u32> = None;
        for _ in 0..block.tfs[index] {
            let raw = read_u32(bytes, pos).ok_or(Error::OutOfBounds)?;
            let position = match previous_position {
                None => raw,
                Some(previous) => previous
                    .checked_add(raw.checked_add(1).ok_or(Error::OutOfBounds)?)
                    .ok_or(Error::OutOfBounds)?,
            };
            if position > MAX_POSITION {
                return Err(Error::OutOfBounds);
            }
            block.positions.push(position);
            previous_position = Some(position);
        }
    }
    block.starts[count] = block.positions.len() as u32;
    Ok(())
}

/// The shared block walk: `df` postings, positions decoded only when asked.
fn read_list_with(
    bytes: &[u8],
    df: u32,
    with_positions: bool,
    mut on_posting: impl FnMut(u32, u32, &[u32]) -> Result<(), Error>,
) -> Result<(), Error> {
    let total = df as usize;
    let mut done = 0usize;
    let mut pos = 0usize;
    let mut previous: Option<u32> = None;
    let mut block = Block::new();
    while done < total {
        let count = (total - done).min(BLOCK);
        decode_block(bytes, &mut pos, count, with_positions, previous, &mut block)?;
        for index in 0..count {
            let doc = block.docs[index];
            let tf = block.tfs[index];
            on_posting(doc, tf, block.run(index))?;
            previous = Some(doc);
        }
        done += count;
    }
    if pos != bytes.len() {
        return Err(Error::OutOfBounds);
    }
    Ok(())
}

/// Decode one posting list, calling `on_posting(doc, tf)` in ascending doc order.
///
/// `df` is the entry's document frequency, so the number of postings is known
/// before the walk starts and the decoder never needs a terminator. The list must
/// be consumed exactly: trailing bytes mean the section is corrupt. Positions are
/// not decoded even when the artifact carries them — use [`read_list_positions`].
pub fn read_list(
    bytes: &[u8],
    df: u32,
    mut on_posting: impl FnMut(u32, u32) -> Result<(), Error>,
) -> Result<(), Error> {
    read_list_with(bytes, df, false, |doc, tf, _| on_posting(doc, tf))
}

/// Decode one posting list, skipping the positions when the artifact carries them.
///
/// Ranking needs the term frequency, not the ordinals: on a positional artifact the
/// runs are walked and dropped, so the cost is one varint per occurrence and no
/// buffer. `with_positions` must come from the header — a reader that guesses wrong
/// either rejects a valid list (trailing bytes) or reads garbage as positions.
pub fn read_list_any(
    bytes: &[u8],
    df: u32,
    with_positions: bool,
    mut on_posting: impl FnMut(u32, u32) -> Result<(), Error>,
) -> Result<(), Error> {
    read_list_with(bytes, df, with_positions, |doc, tf, _| on_posting(doc, tf))
}

/// Decode one posting list with its token ordinals.
///
/// `on_posting(doc, tf, positions)` receives exactly `tf` ordinals, ascending. A
/// list written without positions has none to give; this entry point is only for an
/// artifact whose header says otherwise (see [`crate::format::FLAG_HAS_POSITIONS`]).
pub fn read_list_positions(
    bytes: &[u8],
    df: u32,
    mut on_posting: impl FnMut(u32, u32, &[u32]) -> Result<(), Error>,
) -> Result<(), Error> {
    read_list_with(bytes, df, true, |doc, tf, positions| {
        on_posting(doc, tf, positions)
    })
}

/// Append `count` values of `width` bits each, little-endian within the stream.
fn pack(values: &[u32], width: usize, out: &mut Vec<u8>) {
    let mut accumulator: u64 = 0;
    let mut bits = 0usize;
    for value in values {
        accumulator |= (*value as u64) << bits;
        bits += width;
        while bits >= 8 {
            out.push(accumulator as u8);
            accumulator >>= 8;
            bits -= 8;
        }
    }
    if bits > 0 {
        out.push(accumulator as u8);
    }
}

/// The bit width needed for `value` (0 for 0).
#[inline]
pub fn width_of(value: u32) -> u8 {
    (32 - value.leading_zeros()) as u8
}

/// Append one posting list in the v3 encoding, without positions.
///
/// The caller has already sorted the doc ids and paired them with a non-zero tf.
pub fn write_list(docs: &[u32], tfs: &[u32], out: &mut Vec<u8>) -> Result<(), Error> {
    write_list_with(docs, tfs, None, out)
}

/// Append one posting list with one position run per posting (F6).
///
/// `positions[i]` must hold exactly `tfs[i]` strictly ascending ordinals, each at
/// most [`MAX_POSITION`]. Anything else is [`Error::BadTermOrder`]: a build either
/// writes a valid phrase index or fails, it never writes one a reader has to guess
/// about.
pub fn write_list_positions(
    docs: &[u32],
    tfs: &[u32],
    positions: &[Vec<u32>],
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    if positions.len() != docs.len() {
        return Err(Error::BadCount);
    }
    write_list_with(docs, tfs, Some(positions), out)
}

fn write_list_with(
    docs: &[u32],
    tfs: &[u32],
    positions: Option<&[Vec<u32>]>,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    if docs.len() != tfs.len() {
        return Err(Error::BadCount);
    }
    let mut previous: Option<u32> = None;
    for (block_index, block) in docs.chunks(BLOCK).enumerate() {
        let start = block_index * BLOCK;
        let count = block.len();
        let mut deltas: [u32; BLOCK] = [0; BLOCK];
        let mut width = 0u8;
        let mut all_one = true;
        for (offset, doc) in block.iter().enumerate() {
            let delta = match previous {
                None => *doc,
                Some(previous) => {
                    let delta = doc.checked_sub(previous).ok_or(Error::BadTermOrder)?;
                    if delta == 0 {
                        return Err(Error::BadTermOrder);
                    }
                    delta
                }
            };
            deltas[offset] = delta;
            width = width.max(width_of(delta));
            if tfs[start + offset] != 1 {
                all_one = false;
            }
            previous = Some(*doc);
        }
        let block_tfs = tfs.get(start..start + count).ok_or(Error::BadCount)?;
        // Pick the tf encoding this block actually needs: the 2-bit stream plus the
        // escapes, or one varint per posting.
        let mut small_cost = (count * 2).div_ceil(8);
        let mut varint_cost = 0usize;
        for tf in block_tfs {
            if *tf == 0 || *tf > MAX_TF {
                return Err(Error::BadTermOrder);
            }
            varint_cost += encoded_len(tf - 1);
            if *tf > TF_SMALL_MAX {
                small_cost += encoded_len(tf - 1);
            }
        }
        let mode = if all_one {
            TF_ALL_ONE
        } else if small_cost <= varint_cost {
            TF_SMALL
        } else {
            TF_VARINT
        };
        out.push(width | (mode << TF_SHIFT));
        pack(&deltas[..count], width as usize, out);
        match mode {
            TF_ALL_ONE => {}
            TF_SMALL => {
                let codes: Vec<u32> = block_tfs
                    .iter()
                    .map(|tf| {
                        if *tf > TF_SMALL_MAX {
                            TF_SMALL_MAX
                        } else {
                            tf - 1
                        }
                    })
                    .collect();
                pack(&codes, 2, out);
                for tf in block_tfs {
                    if *tf > TF_SMALL_MAX {
                        write_u32(out, tf - 1);
                    }
                }
            }
            _ => {
                for tf in block_tfs {
                    write_u32(out, tf - 1);
                }
            }
        }
        if let Some(positions) = positions {
            for (offset, tf) in block_tfs.iter().enumerate() {
                let run = positions.get(start + offset).ok_or(Error::BadCount)?;
                if run.len() != *tf as usize {
                    return Err(Error::BadTermOrder);
                }
                let mut previous_position: Option<u32> = None;
                for position in run {
                    if *position > MAX_POSITION {
                        return Err(Error::BadTermOrder);
                    }
                    match previous_position {
                        None => write_u32(out, *position),
                        Some(previous) => {
                            if *position <= previous {
                                return Err(Error::BadTermOrder);
                            }
                            write_u32(out, *position - previous - 1);
                        }
                    }
                    previous_position = Some(*position);
                }
            }
        }
    }
    Ok(())
}

/// A forward cursor over one positional posting list.
///
/// A phrase walks every term's list once, in ascending doc order, so the cursor only
/// ever moves forward: [`Positions::at`] skips the blocks before the requested doc
/// and answers whether it is there. That is linear in the list, not in the corpus,
/// and it needs no index of its own.
pub struct Positions<'a> {
    bytes: &'a [u8],
    df: u32,
    pos: usize,
    done: u32,
    previous: Option<u32>,
    block: Block,
    index: usize,
}

impl<'a> Positions<'a> {
    /// A cursor over `bytes`, a list that holds `df` postings with positions.
    pub fn new(bytes: &'a [u8], df: u32) -> Result<Self, Error> {
        if df == 0 && !bytes.is_empty() {
            return Err(Error::OutOfBounds);
        }
        Ok(Self {
            bytes,
            df,
            pos: 0,
            done: 0,
            previous: None,
            block: Block::new(),
            index: 0,
        })
    }

    /// The positions of `doc`, or `None` when the list does not hold it.
    ///
    /// Calls must be in ascending doc order; a cursor that has passed a document
    /// cannot go back to it.
    pub fn at(&mut self, doc: u32) -> Result<Option<&[u32]>, Error> {
        loop {
            while self.index < self.block.len && self.block.docs[self.index] < doc {
                self.index += 1;
            }
            if self.index < self.block.len {
                if self.block.docs[self.index] != doc {
                    return Ok(None);
                }
                let index = self.index;
                return Ok(Some(self.block.run(index)));
            }
            if self.done >= self.df {
                return Ok(None);
            }
            let count = ((self.df - self.done) as usize).min(BLOCK);
            decode_block(
                self.bytes,
                &mut self.pos,
                count,
                true,
                self.previous,
                &mut self.block,
            )?;
            // The next block's first delta is a gap from this block's last doc, so
            // the cursor has to carry it across the boundary; a later list is longer
            // than one block as soon as a term is common.
            self.previous = (count > 0).then(|| self.block.docs[count - 1]);
            self.done += count as u32;
            self.index = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn decode(bytes: &[u8], df: u32) -> Result<Vec<(u32, u32)>, Error> {
        let mut seen = Vec::new();
        read_list(bytes, df, |doc, tf| {
            seen.push((doc, tf));
            Ok(())
        })?;
        Ok(seen)
    }

    fn decode_positions(bytes: &[u8], df: u32) -> Result<Vec<(u32, Vec<u32>)>, Error> {
        let mut seen = Vec::new();
        read_list_positions(bytes, df, |doc, _, positions| {
            seen.push((doc, positions.to_vec()));
            Ok(())
        })?;
        Ok(seen)
    }

    #[test]
    fn a_block_of_one_round_trips_through_the_bit_packer() {
        let mut bytes = Vec::new();
        write_list(&[7], &[3], &mut bytes).unwrap();
        assert_eq!(decode(&bytes, 1).unwrap(), [(7, 3)]);
    }

    #[test]
    fn all_one_frequencies_cost_no_tf_bytes() {
        let mut bytes = Vec::new();
        write_list(&[1, 4, 9], &[1, 1, 1], &mut bytes).unwrap();
        assert_eq!(bytes[0] >> TF_SHIFT, TF_ALL_ONE);
        assert_eq!(decode(&bytes, 3).unwrap(), [(1, 1), (4, 1), (9, 1)]);
    }

    #[test]
    fn the_2_bit_mode_carries_the_common_frequencies_and_escapes_the_tail() {
        let tfs = [1, 2, 3, 40, 1, 4];
        let mut bytes = Vec::new();
        write_list(&[0, 1, 2, 3, 4, 5], &tfs, &mut bytes).unwrap();
        assert_eq!(bytes[0] >> TF_SHIFT, TF_SMALL);
        assert_eq!(
            decode(&bytes, tfs.len() as u32).unwrap(),
            [(0, 1), (1, 2), (2, 3), (3, 40), (4, 1), (5, 4)]
        );
        // A block whose frequencies are all large picks the varint mode instead:
        // two bits per posting would only add escape after escape.
        let mut long = Vec::new();
        write_list(&[0, 1], &[40, 50], &mut long).unwrap();
        assert_eq!(long[0] >> TF_SHIFT, TF_VARINT);
        assert_eq!(decode(&long, 2).unwrap(), [(0, 40), (1, 50)]);
    }

    #[test]
    fn widths_follow_the_largest_gap() {
        assert_eq!(width_of(0), 0);
        assert_eq!(width_of(1), 1);
        assert_eq!(width_of(255), 8);
        assert_eq!(width_of(256), 9);
        assert_eq!(width_of(u32::MAX), 32);
    }

    #[test]
    fn a_reserved_mode_or_width_is_rejected_not_guessed() {
        // Header 0: width 0, all-one mode, one posting: doc 0 / tf 1 is legal.
        assert_eq!(decode(&[0x00], 1).unwrap(), [(0, 1)]);
        // Mode 3 is reserved.
        assert!(read_list(&[0xc0], 1, |_, _| Ok(())).is_err());
        // A width above 32 cannot be a delta.
        assert!(read_list(&[33], 1, |_, _| Ok(())).is_err());
        // Trailing bytes are corrupt.
        assert!(read_list(&[0x00, 0], 1, |_, _| Ok(())).is_err());
        // An escape code without its varint is truncation, not zero.
        assert!(read_list(&[0x40, 0x03], 1, |_, _| Ok(())).is_err());
    }

    #[test]
    fn a_list_longer_than_a_block_reads_every_posting() {
        let docs: Vec<u32> = (0..300).map(|index| index * 2).collect();
        let tfs: Vec<u32> = (0..300).map(|index| index % 5 + 1).collect();
        let mut bytes = Vec::new();
        write_list(&docs, &tfs, &mut bytes).unwrap();
        let seen = decode(&bytes, docs.len() as u32).unwrap();
        assert_eq!(seen.len(), 300);
        assert_eq!(seen[0], (0, 1));
        assert_eq!(seen[299], (598, 5));
    }

    #[test]
    fn positions_round_trip_per_posting_across_blocks() {
        let docs: Vec<u32> = (0..300).map(|index| index * 2).collect();
        let tfs: Vec<u32> = (0..300).map(|index| index % 4 + 1).collect();
        let positions: Vec<Vec<u32>> = tfs
            .iter()
            .enumerate()
            .map(|(doc, tf)| (0..*tf).map(|at| doc as u32 * 7 + at).collect())
            .collect();
        let mut bytes = Vec::new();
        write_list_positions(&docs, &tfs, &positions, &mut bytes).unwrap();
        let seen = decode_positions(&bytes, docs.len() as u32).unwrap();
        assert_eq!(seen.len(), 300);
        for (index, (doc, run)) in seen.iter().enumerate() {
            assert_eq!(*doc, docs[index]);
            assert_eq!(run, &positions[index]);
        }
        // Positioning costs bytes; a list without them stays byte-identical.
        let mut plain = Vec::new();
        write_list(&docs, &tfs, &mut plain).unwrap();
        assert!(bytes.len() > plain.len());
        assert!(plain[..].iter().all(|byte| bytes.contains(byte)));
    }

    #[test]
    fn the_cursor_walks_forward_and_answers_absent_docs() {
        let docs: Vec<u32> = vec![3, 9, 500];
        let tfs: Vec<u32> = vec![2, 1, 3];
        let positions: Vec<Vec<u32>> = vec![vec![1, 4], vec![7], vec![2, 5, 6]];
        let mut bytes = Vec::new();
        write_list_positions(&docs, &tfs, &positions, &mut bytes).unwrap();
        let mut cursor = Positions::new(&bytes, 3).unwrap();
        assert_eq!(cursor.at(0).unwrap(), None);
        assert_eq!(cursor.at(3).unwrap(), Some(&[1, 4][..]));
        assert_eq!(cursor.at(4).unwrap(), None);
        assert_eq!(cursor.at(9).unwrap(), Some(&[7][..]));
        assert_eq!(cursor.at(500).unwrap(), Some(&[2, 5, 6][..]));
        assert_eq!(cursor.at(501).unwrap(), None);
        assert!(Positions::new(&[1, 2], 0).is_err());
    }

    #[test]
    fn the_cursor_reads_across_block_boundaries() {
        // 300 postings is three blocks: the first delta of block 2 and 3 is a gap from
        // the previous block's last doc, which the cursor must carry.
        let docs: Vec<u32> = (0..300).map(|index| index * 3).collect();
        let tfs: Vec<u32> = vec![2; 300];
        let positions: Vec<Vec<u32>> = (0..300).map(|doc| vec![doc * 2, doc * 2 + 1]).collect();
        let mut bytes = Vec::new();
        write_list_positions(&docs, &tfs, &positions, &mut bytes).unwrap();
        let mut cursor = Positions::new(&bytes, 300).unwrap();
        for doc in [0u32, 128, 129, 255, 256, 299] {
            assert_eq!(
                cursor.at(doc * 3).unwrap(),
                Some(&positions[doc as usize][..]),
                "doc {} lives at ordinal {}",
                doc * 3,
                doc
            );
            assert_eq!(cursor.at(doc * 3 + 1).unwrap(), None, "nothing between");
        }
    }

    #[test]
    fn a_position_stream_is_validated_like_any_other() {
        // A run has to match the tf of its posting exactly.
        let mut bytes = Vec::new();
        assert!(write_list_positions(&[1], &[2], &[vec![3]], &mut bytes).is_err());
        // Positions must be strictly ascending, so a repeated ordinal is refused.
        let mut bytes = Vec::new();
        assert!(write_list_positions(&[1], &[2], &[vec![3, 3]], &mut bytes).is_err());
        // And no ordinal may pass the format bound.
        let mut bytes = Vec::new();
        assert!(write_list_positions(&[1], &[1], &[vec![MAX_POSITION + 1]], &mut bytes).is_err());
        // A truncated run is a bounds error at read time, not a zero.
        let mut bytes = Vec::new();
        write_list_positions(&[1], &[3], &[vec![1, 2, 3]], &mut bytes).unwrap();
        assert!(read_list_positions(&bytes[..bytes.len() - 1], 1, |_, _, _| Ok(())).is_err());
        // A non-monotonic pair decodes to an overflow, not a wrapped ordinal.
        let mut forged = Vec::new();
        write_list(&[1], &[2], &mut forged).unwrap();
        // tf 2, then the run: absolute 5 and a gap that would step back.
        forged.extend_from_slice(&[5, 200, 200, 200, 200, 200, 1]);
        assert!(read_list_positions(&forged, 1, |_, _, _| Ok(())).is_err());
    }

    #[test]
    fn arbitrary_bytes_never_panic() {
        // A deterministic byte sweep instead of a fuzz corpus: every header, width
        // and mode combination the decoder can meet is tried against random data,
        // and the only acceptable outcomes are a value list or `Err`. The positional
        // reader runs on the same bytes: the new mode has to fail the same way.
        let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
        for _ in 0..4_000 {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let len = ((state >> 33) as usize) % 48;
            let df = ((state >> 17) % 200) as u32;
            let seed = state as u8;
            let bytes: Vec<u8> = (0..len)
                .map(|index| seed.rotate_left(index as u32))
                .collect();
            let _ = read_list(&bytes, df, |_, _| Ok(()));
            let _ = read_list_positions(&bytes, df, |_, _, _| Ok(()));
            if let Ok(mut cursor) = Positions::new(&bytes, df) {
                let _ = cursor.at((state >> 9) as u32 % 256);
            }
        }
    }

    #[test]
    fn the_encoder_refuses_unsorted_docs_and_zero_frequencies() {
        let mut bytes = Vec::new();
        assert!(write_list(&[5, 5], &[1, 1], &mut bytes).is_err());
        assert!(write_list(&[5, 4], &[1, 1], &mut bytes).is_err());
        assert!(write_list(&[5], &[0], &mut bytes).is_err());
        assert!(write_list(&[5], &[1, 2], &mut bytes).is_err());
        // A positions table that does not line up with the postings is refused.
        assert!(write_list_positions(&[1, 2], &[1, 1], &[vec![0]], &mut bytes).is_err());
    }
}
