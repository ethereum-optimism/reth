use rstest::rstest;

#[derive(Clone, Copy, Debug)]
enum CompressionFormat {
    NippyJar,
    NippyJarDictionary,
    Receipt,
    Transaction,
}

mod previous {
    use old_compressors as compressors;
    use old_zstd as zstd;

    include!("support/codec.rs");
}

mod updated {
    use new_compressors as compressors;
    use new_zstd as zstd;

    include!("support/codec.rs");
}

#[rstest]
#[case::nippy_jar(CompressionFormat::NippyJar)]
#[case::nippy_jar_dictionary(CompressionFormat::NippyJarDictionary)]
#[case::receipt(CompressionFormat::Receipt)]
#[case::transaction(CompressionFormat::Transaction)]
fn updated_reader_accepts_previous_frames(
    #[case] format: CompressionFormat,
    #[values(0, 256, 4096, 16384)] data_size: usize,
) -> std::io::Result<()> {
    let original = test_payload(data_size);
    let compressed = previous::encode(format, &original)?;

    assert_eq!(updated::decode(format, &compressed, original.len())?, original);
    Ok(())
}

#[rstest]
#[case::nippy_jar(CompressionFormat::NippyJar)]
#[case::nippy_jar_dictionary(CompressionFormat::NippyJarDictionary)]
#[case::receipt(CompressionFormat::Receipt)]
#[case::transaction(CompressionFormat::Transaction)]
fn previous_reader_accepts_updated_frames(
    #[case] format: CompressionFormat,
    #[values(0, 256, 4096, 16384)] data_size: usize,
) -> std::io::Result<()> {
    let original = test_payload(data_size);
    let compressed = updated::encode(format, &original)?;

    assert_eq!(previous::decode(format, &compressed, original.len())?, original);
    Ok(())
}

#[rstest]
#[case::nippy_jar(CompressionFormat::NippyJar)]
#[case::nippy_jar_dictionary(CompressionFormat::NippyJarDictionary)]
fn updated_reader_rejects_malformed_frames(#[case] format: CompressionFormat) {
    assert!(updated::decode(format, b"invalid zstd frame", 16384).is_err());
}

#[test]
fn compression_dictionaries_are_unchanged() {
    assert_eq!(new_compressors::RECEIPT_DICTIONARY, old_compressors::RECEIPT_DICTIONARY);
    assert_eq!(new_compressors::TRANSACTION_DICTIONARY, old_compressors::TRANSACTION_DICTIONARY);
}

#[test]
fn native_library_matches_release_version() {
    assert_eq!(old_zstd_safe::version_number(), 10507);
}

fn test_payload(data_size: usize) -> Vec<u8> {
    (0u8..=255).cycle().take(data_size).collect()
}
