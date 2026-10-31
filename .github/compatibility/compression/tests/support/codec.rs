use super::CompressionFormat;

pub(super) fn encode(format: CompressionFormat, data: &[u8]) -> std::io::Result<Vec<u8>> {
    match format {
        CompressionFormat::NippyJar => zstd::stream::encode_all(data, 0),
        CompressionFormat::NippyJarDictionary => {
            let dictionary =
                zstd::dict::EncoderDictionary::copy(compressors::TRANSACTION_DICTIONARY, 0);
            zstd::bulk::Compressor::with_prepared_dictionary(&dictionary)?.compress(data)
        }
        CompressionFormat::Receipt => compressors::create_receipt_compressor().compress(data),
        CompressionFormat::Transaction => compressors::create_tx_compressor().compress(data),
    }
}

pub(super) fn decode(
    format: CompressionFormat,
    frame: &[u8],
    data_size: usize,
) -> std::io::Result<Vec<u8>> {
    match format {
        CompressionFormat::NippyJar => zstd::stream::decode_all(frame),
        CompressionFormat::NippyJarDictionary => {
            let dictionary =
                zstd::dict::DecoderDictionary::copy(compressors::TRANSACTION_DICTIONARY);
            zstd::bulk::Decompressor::with_prepared_dictionary(&dictionary)?
                .decompress(frame, data_size)
        }
        CompressionFormat::Receipt => {
            Ok(compressors::create_receipt_decompressor().decompress(frame).to_vec())
        }
        CompressionFormat::Transaction => {
            Ok(compressors::create_tx_decompressor().decompress(frame).to_vec())
        }
    }
}
