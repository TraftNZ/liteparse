//! Preserve filtered inline image operations using their actual zlib boundary.
use std::{error::Error, io};

use flate2::{Decompress, FlushDecompress, Status};
use lopdf::{
    Object, Stream,
    content::{Content, Operation},
};

const INFLATE_BUFFER_BYTES: usize = 8192;
const MAX_IMAGE_DECODE_BYTES: u64 = 512 * 1024 * 1024;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn whitespace(byte: u8) -> bool {
    matches!(byte, 0 | 9 | 10 | 12 | 13 | 32)
}

fn delimiter(byte: u8) -> bool {
    whitespace(byte) || b"()<>[]{}/%".contains(&byte)
}

// Strings and names are indivisible tokens: their contents cannot be operators.
fn token(bytes: &[u8], cursor: &mut usize) -> Result<Option<(usize, usize)>, io::Error> {
    while *cursor < bytes.len() {
        if whitespace(bytes[*cursor]) {
            *cursor += 1;
        } else if bytes[*cursor] == b'%' {
            while *cursor < bytes.len() && !matches!(bytes[*cursor], b'\r' | b'\n') {
                *cursor += 1;
            }
        } else {
            break;
        }
    }
    let start = *cursor;
    let Some(&first) = bytes.get(start) else {
        return Ok(None);
    };
    *cursor += 1;
    if first == b'(' {
        let mut depth = 1usize;
        while let Some(&byte) = bytes.get(*cursor) {
            *cursor += 1;
            match byte {
                b'\\' => {
                    if *cursor < bytes.len() {
                        *cursor += 1;
                    }
                }
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(Some((start, *cursor)));
                    }
                }
                _ => {}
            }
        }
        return Err(invalid("unterminated content string"));
    }
    if matches!(first, b'<' | b'>') && bytes.get(*cursor) == Some(&first) {
        *cursor += 1;
    } else if first == b'<' {
        while bytes.get(*cursor).is_some_and(|byte| *byte != b'>') {
            *cursor += 1;
        }
        if *cursor == bytes.len() {
            return Err(invalid("unterminated hex string"));
        }
        *cursor += 1;
    } else if first == b'/' || !delimiter(first) {
        while bytes.get(*cursor).is_some_and(|byte| !delimiter(*byte)) {
            *cursor += 1;
        }
    }
    Ok(Some((start, *cursor)))
}

fn flate_length(bytes: &[u8]) -> Result<usize, Box<dyn Error>> {
    let mut decoder = Decompress::new(true);
    let mut output = [0; INFLATE_BUFFER_BYTES];
    loop {
        let before = (decoder.total_in(), decoder.total_out());
        let consumed = usize::try_from(before.0)?;
        let status = decoder.decompress(&bytes[consumed..], &mut output, FlushDecompress::None)?;
        if decoder.total_out() > MAX_IMAGE_DECODE_BYTES {
            return Err(invalid("inline image decoded size limit exceeded").into());
        }
        if status == Status::StreamEnd {
            return Ok(usize::try_from(decoder.total_in())?);
        }
        if before == (decoder.total_in(), decoder.total_out()) {
            return Err(invalid("truncated inline image zlib stream").into());
        }
    }
}

pub(crate) fn decode(bytes: &[u8]) -> Result<Vec<Operation>, Box<dyn Error>> {
    let mut cursor = 0;
    let mut chunk_start = 0;
    let mut depth = 0usize;
    let mut operations = Vec::new();
    while let Some((start, end)) = token(bytes, &mut cursor)? {
        let value = &bytes[start..end];
        match value {
            b"[" | b"<<" => {
                depth += 1;
                continue;
            }
            b"]" | b">>" => {
                depth = depth
                    .checked_sub(1)
                    .ok_or_else(|| invalid("unbalanced content container"))?;
                continue;
            }
            _ => {}
        }
        if depth != 0 || value != b"BI" {
            continue;
        }
        let header_start = end;
        let header_end = loop {
            let Some((start, end)) = token(bytes, &mut cursor)? else {
                return Err(invalid("inline image missing ID").into());
            };
            if &bytes[start..end] == b"ID" {
                break start;
            }
        };
        let mut dictionary_bytes = b"<< ".to_vec();
        dictionary_bytes.extend_from_slice(&bytes[header_start..header_end]);
        dictionary_bytes.extend_from_slice(b" >> inlineimageheader");
        let header = Content::decode(&dictionary_bytes)?;
        let dictionary = header
            .operations
            .first()
            .and_then(|op| op.operands.first())
            .ok_or_else(|| invalid("missing inline image dictionary"))?
            .as_dict()?
            .clone();
        let filter = dictionary.get(b"F").or_else(|_| dictionary.get(b"Filter"));
        let flate = filter.ok().is_some_and(|object| match object {
            Object::Name(name) => matches!(name.as_slice(), b"Fl" | b"FlateDecode"),
            Object::Array(filters) if filters.len() == 1 => filters[0]
                .as_name()
                .is_ok_and(|name| matches!(name, b"Fl" | b"FlateDecode")),
            _ => false,
        });
        if !flate {
            // Keep lopdf's existing decoding rules for other image encodings.
            operations.extend(Content::decode(&bytes[chunk_start..])?.operations);
            return Ok(operations);
        }
        if !bytes.get(cursor).is_some_and(|byte| whitespace(*byte)) {
            return Err(invalid("inline image ID missing separator").into());
        }
        cursor += 1;
        if bytes.get(cursor - 1) == Some(&b'\r') && bytes.get(cursor) == Some(&b'\n') {
            cursor += 1;
        }
        let data_start = cursor;
        cursor += flate_length(&bytes[data_start..])?;
        let data_end = cursor;
        if !bytes.get(cursor).is_some_and(|byte| whitespace(*byte)) {
            return Err(invalid("inline image data missing EI separator").into());
        }
        let terminator =
            token(bytes, &mut cursor)?.ok_or_else(|| invalid("inline image missing EI"))?;
        if &bytes[terminator.0..terminator.1] != b"EI" {
            return Err(invalid("inline image stream not followed by EI").into());
        }
        operations.extend(Content::decode(&bytes[chunk_start..start])?.operations);
        operations.push(Operation::new(
            "BI",
            vec![Object::Stream(Stream::new(
                dictionary,
                bytes[data_start..data_end].to_vec(),
            ))],
        ));
        chunk_start = cursor;
    }
    operations.extend(Content::decode(&bytes[chunk_start..])?.operations);
    Ok(operations)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write;

    #[test]
    fn compressed_image_preserves_payload_and_following_operations() {
        let pixels = b" EI BI ID Q \0\xff";
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::none());
        encoder.write_all(pixels).unwrap();
        let compressed = encoder.finish().unwrap();
        assert!(compressed.windows(4).any(|bytes| bytes == b" EI "));
        let mut bytes =
            b"(BI ID) Tj /BI Do q 10 0 0 20 0 0 cm BI /CS/R17 /W 5 /H 1 /BPC 8 /F/Fl ID ".to_vec();
        bytes.extend_from_slice(&compressed);
        bytes.extend_from_slice(b"\nEI Q 1 2 m 3 4 l S");
        let operations = decode(&bytes).unwrap();
        assert_eq!(
            operations
                .iter()
                .map(|op| op.operator.as_str())
                .collect::<Vec<_>>(),
            ["Tj", "Do", "q", "cm", "BI", "Q", "m", "l", "S"]
        );
        assert_eq!(
            operations[4].operands[0].as_stream().unwrap().content,
            compressed
        );
        assert!(decode(&bytes[..bytes.len() - 25]).is_err());
    }
    #[test]
    #[ignore = "requires the decompressed real addendum page streams"]
    fn addendum_inline_images_preserve_following_page_content() {
        let dir = std::path::PathBuf::from(
            std::env::var_os("ADDENDUM_CONTENT_DIR").expect("real content directory"),
        );
        for page in [27, 28, 29, 30] {
            let bytes = std::fs::read(dir.join(format!("page-{page}.content.bin"))).unwrap();
            let operations = decode(&bytes).unwrap();
            let images: Vec<_> = operations
                .iter()
                .enumerate()
                .filter(|(_, op)| op.operator == "BI")
                .collect();
            assert_eq!(images.len(), 1, "page {page}");
            let (position, image) = images[0];
            assert_eq!(
                image.operands[0]
                    .as_stream()
                    .unwrap()
                    .dict
                    .get(b"CS")
                    .unwrap()
                    .as_name()
                    .unwrap(),
                b"R17"
            );
            assert!(
                position > 0 && position + 1 < operations.len(),
                "page {page}"
            );
            assert_eq!(operations[position + 1].operator, "Q", "page {page}");
        }
    }
}
