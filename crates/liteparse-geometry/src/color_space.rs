//! ICC profile metadata used in source colorspace descriptions.

use std::error::Error;
use std::io;

use lopdf::Stream;

const ICC_SIGNATURE_OFFSET: usize = 36;
const ICC_COLOR_MODEL_OFFSET: usize = 16;
const ICC_TAG_COUNT_OFFSET: usize = 128;
const ICC_TAG_TABLE_OFFSET: usize = 132;
const ICC_TAG_RECORD_BYTES: usize = 12;
const ICC_DESCRIPTION_LENGTH_OFFSET: usize = 8;
const ICC_DESCRIPTION_TEXT_OFFSET: usize = 12;
const ICC_MLUC_RECORD_SIZE_OFFSET: usize = 12;
const ICC_MLUC_TABLE_OFFSET: usize = 16;
const ICC_MLUC_MIN_RECORD_BYTES: usize = 12;
const ICC_MLUC_LENGTH_OFFSET: usize = 4;
const ICC_MLUC_TEXT_OFFSET: usize = 8;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn bytes(data: &[u8], offset: usize, length: usize) -> Result<&[u8], io::Error> {
    offset
        .checked_add(length)
        .and_then(|end| data.get(offset..end))
        .ok_or_else(|| invalid("ICC metadata is truncated or has invalid offsets"))
}

fn integer(data: &[u8], offset: usize) -> Result<usize, io::Error> {
    let value = u32::from_be_bytes(bytes(data, offset, size_of::<u32>())?.try_into().unwrap());
    usize::try_from(value).map_err(|_| invalid("ICC metadata length is out of range"))
}

fn description(tag: &[u8]) -> Result<String, Box<dyn Error>> {
    match bytes(tag, 0, size_of::<u32>())? {
        b"desc" => {
            let length = integer(tag, ICC_DESCRIPTION_LENGTH_OFFSET)?;
            let text = bytes(tag, ICC_DESCRIPTION_TEXT_OFFSET, length)?;
            Ok(std::str::from_utf8(text)?.trim_end_matches('\0').to_owned())
        }
        b"mluc" => {
            let count = integer(tag, ICC_DESCRIPTION_LENGTH_OFFSET)?;
            let record_size = integer(tag, ICC_MLUC_RECORD_SIZE_OFFSET)?;
            if record_size < ICC_MLUC_MIN_RECORD_BYTES {
                return Err(invalid("ICC localized description record is too short").into());
            }
            let table_length = count
                .checked_mul(record_size)
                .ok_or_else(|| invalid("ICC localized description table is too large"))?;
            let table = bytes(tag, ICC_MLUC_TABLE_OFFSET, table_length)?;
            let mut selected = None;
            for record in table.chunks_exact(record_size) {
                if selected.is_none() || &record[..ICC_MLUC_LENGTH_OFFSET] == b"enUS" {
                    selected = Some(record);
                }
                if &record[..ICC_MLUC_LENGTH_OFFSET] == b"enUS" {
                    break;
                }
            }
            let Some(record) = selected else {
                return Ok(String::new());
            };
            let text = bytes(
                tag,
                integer(record, ICC_MLUC_TEXT_OFFSET)?,
                integer(record, ICC_MLUC_LENGTH_OFFSET)?,
            )?;
            if !text.len().is_multiple_of(size_of::<u16>()) {
                return Err(invalid("ICC localized description has incomplete UTF-16").into());
            }
            let words: Vec<_> = text
                .as_chunks::<{ size_of::<u16>() }>()
                .0
                .iter()
                .map(|word| u16::from_be_bytes(*word))
                .collect();
            Ok(String::from_utf16(&words)?
                .trim_end_matches('\0')
                .to_owned())
        }
        _ => Err(invalid("ICC description has an invalid tag type").into()),
    }
}

pub(crate) fn profile_name(stream: &Stream) -> Result<String, Box<dyn Error>> {
    let components = stream.dict.get(b"N")?.as_i64()?;
    if !matches!(components, 1 | 3 | 4) {
        return Err(invalid("ICC colorspace requires one, three or four components").into());
    }
    let data = if stream.dict.has(b"Filter") {
        stream.decompressed_content()?
    } else {
        stream.content.clone()
    };
    if bytes(&data, ICC_SIGNATURE_OFFSET, size_of::<u32>())? != b"acsp" {
        return Err(invalid("ICC profile signature is invalid").into());
    }
    let model = match bytes(&data, ICC_COLOR_MODEL_OFFSET, size_of::<u32>())? {
        b"GRAY" => "Gray".to_owned(),
        b"RGB " => "RGB".to_owned(),
        b"CMYK" => "CMYK".to_owned(),
        b"Lab " => "Lab".to_owned(),
        _ => components.to_string(),
    };
    let count = integer(&data, ICC_TAG_COUNT_OFFSET)?;
    let table_length = count
        .checked_mul(ICC_TAG_RECORD_BYTES)
        .ok_or_else(|| invalid("ICC tag table is too large"))?;
    let table = bytes(&data, ICC_TAG_TABLE_OFFSET, table_length)?;
    let mut name = String::new();
    for record in table.as_chunks::<ICC_TAG_RECORD_BYTES>().0 {
        if &record[..size_of::<u32>()] == b"desc" {
            let tag = bytes(
                &data,
                integer(record, size_of::<u32>())?,
                integer(record, 2 * size_of::<u32>())?,
            )?;
            name = description(tag)?;
            break;
        }
    }
    Ok(format!("ICCBased({model},{name})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lopdf::dictionary;

    #[test]
    fn localized_description_selects_english_and_checks_utf16_bounds() {
        let french = "Profil"
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect::<Vec<_>>();
        let english = "Profile"
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect::<Vec<_>>();
        let text_offset = ICC_MLUC_TABLE_OFFSET + 2 * ICC_MLUC_MIN_RECORD_BYTES;
        let mut tag = b"mluc\0\0\0\0".to_vec();
        tag.extend_from_slice(&2_u32.to_be_bytes());
        tag.extend_from_slice(&(ICC_MLUC_MIN_RECORD_BYTES as u32).to_be_bytes());
        for (language, offset, text) in [
            (b"frFR", text_offset, &french),
            (b"enUS", text_offset + french.len(), &english),
        ] {
            tag.extend_from_slice(language);
            tag.extend_from_slice(&(text.len() as u32).to_be_bytes());
            tag.extend_from_slice(&(offset as u32).to_be_bytes());
        }
        tag.extend_from_slice(&french);
        tag.extend_from_slice(&english);
        assert_eq!(description(&tag).unwrap(), "Profile");
        assert!(description(&tag[..tag.len() - 1]).is_err());
        let length_offset =
            ICC_MLUC_TABLE_OFFSET + ICC_MLUC_MIN_RECORD_BYTES + ICC_MLUC_LENGTH_OFFSET;
        tag[length_offset..length_offset + size_of::<u32>()].copy_from_slice(&1_u32.to_be_bytes());
        assert!(description(&tag).is_err());
    }

    #[test]
    fn profile_metadata_rejects_invalid_headers_counts_and_tag_offsets() {
        let mut data = vec![0; ICC_TAG_TABLE_OFFSET];
        data[ICC_COLOR_MODEL_OFFSET..ICC_COLOR_MODEL_OFFSET + size_of::<u32>()]
            .copy_from_slice(b"RGB ");
        data[ICC_SIGNATURE_OFFSET..ICC_SIGNATURE_OFFSET + size_of::<u32>()]
            .copy_from_slice(b"acsp");
        let mut stream = Stream::new(dictionary! {"N" => 3}, data);
        assert_eq!(profile_name(&stream).unwrap(), "ICCBased(RGB,)");
        stream.content[ICC_SIGNATURE_OFFSET] = 0;
        assert!(profile_name(&stream).is_err());
        stream.content[ICC_SIGNATURE_OFFSET] = b'a';
        stream.content[ICC_TAG_COUNT_OFFSET..ICC_TAG_TABLE_OFFSET]
            .copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(profile_name(&stream).is_err());
        stream.dict.set("N", 0);
        assert!(profile_name(&stream).is_err());
        let mut tag = b"desc\0\0\0\0".to_vec();
        tag.extend_from_slice(&u32::MAX.to_be_bytes());
        assert!(description(&tag).is_err());
    }
}
