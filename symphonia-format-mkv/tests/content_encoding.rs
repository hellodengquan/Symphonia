// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Tests for Matroska track content encodings (header stripping compression, and the handling
//! of unsupported compressions and encryptions).

use std::io::Cursor;

use symphonia_core::errors::{Error, Result};
use symphonia_core::formats::prelude::*;
use symphonia_core::io::MediaSourceStream;
use symphonia_core::units::Timestamp;

use symphonia_format_mkv::MkvReader;

// ---------------------------------------------------------------------------------------------
// Minimal EBML/Matroska writer helpers.
// ---------------------------------------------------------------------------------------------

/// Encode an EBML element data size as a variable-length integer.
fn vint_size(len: usize) -> Vec<u8> {
    if len < 0x7F {
        vec![0x80 | len as u8]
    } else if len < 0x3FFF {
        (0x4000u16 | len as u16).to_be_bytes().to_vec()
    } else if len < 0x1F_FFFF {
        (0x20_0000u32 | len as u32).to_be_bytes()[1..].to_vec()
    } else {
        (0x1000_0000u32 | len as u32).to_be_bytes().to_vec()
    }
}

/// Build an EBML element with the given id and data.
fn el(id: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = id.to_vec();
    out.extend(vint_size(data.len()));
    out.extend_from_slice(data);
    out
}

/// Build an EBML unsigned integer element.
fn uint_el(id: &[u8], val: u64) -> Vec<u8> {
    let bytes = val.to_be_bytes();
    let mut start = 0;
    while start < 7 && bytes[start] == 0 {
        start += 1;
    }
    el(id, &bytes[start..])
}

/// Build an EBML string element.
fn str_el(id: &[u8], s: &str) -> Vec<u8> {
    el(id, s.as_bytes())
}

/// Build an EBML master element from child elements.
fn master_el(id: &[u8], children: &[Vec<u8>]) -> Vec<u8> {
    el(id, &children.concat())
}

// Element IDs.
const EBML: &[u8] = &[0x1A, 0x45, 0xDF, 0xA3];
const DOC_TYPE: &[u8] = &[0x42, 0x82];
const SEGMENT: &[u8] = &[0x18, 0x53, 0x80, 0x67];
const INFO: &[u8] = &[0x15, 0x49, 0xA9, 0x66];
const TIMESTAMP_SCALE: &[u8] = &[0x2A, 0xD7, 0xB1];
const MUXING_APP: &[u8] = &[0x4D, 0x80];
const WRITING_APP: &[u8] = &[0x57, 0x41];
const TRACKS: &[u8] = &[0x16, 0x54, 0xAE, 0x6B];
const TRACK_ENTRY: &[u8] = &[0xAE];
const TRACK_NUMBER: &[u8] = &[0xD7];
const TRACK_UID: &[u8] = &[0x73, 0xC5];
const CODEC_ID: &[u8] = &[0x86];
const CODEC_PRIVATE: &[u8] = &[0x63, 0xA2];
const AUDIO: &[u8] = &[0xE1];
const SAMPLING_FREQUENCY: &[u8] = &[0xB5];
const CHANNELS: &[u8] = &[0x9F];
const CONTENT_ENCODINGS: &[u8] = &[0x6D, 0x80];
const CONTENT_ENCODING: &[u8] = &[0x62, 0x40];
const CONTENT_ENCODING_ORDER: &[u8] = &[0x50, 0x31];
const CONTENT_ENCODING_SCOPE: &[u8] = &[0x50, 0x32];
const CONTENT_ENCODING_TYPE: &[u8] = &[0x50, 0x33];
const CONTENT_COMPRESSION: &[u8] = &[0x50, 0x34];
const CONTENT_COMP_ALGO: &[u8] = &[0x42, 0x54];
const CONTENT_COMP_SETTINGS: &[u8] = &[0x42, 0x55];
const CONTENT_ENCRYPTION: &[u8] = &[0x50, 0x35];
const CONTENT_ENC_ALGO: &[u8] = &[0x47, 0xE1];
const CLUSTER: &[u8] = &[0x1F, 0x43, 0xB6, 0x75];
const TIMESTAMP: &[u8] = &[0xE7];
const SIMPLE_BLOCK: &[u8] = &[0xA3];

/// Options for building a track entry.
struct TrackOpts {
    number: u64,
    codec_private: Option<Vec<u8>>,
    content_encodings: Option<Vec<u8>>,
}

impl Default for TrackOpts {
    fn default() -> Self {
        Self { number: 1, codec_private: None, content_encodings: None }
    }
}

/// Build a `ContentEncoding` element declaring header stripping compression.
fn header_strip_encoding(order: u64, scope: u64, stripped: &[u8]) -> Vec<u8> {
    master_el(
        CONTENT_ENCODING,
        &[
            uint_el(CONTENT_ENCODING_ORDER, order),
            uint_el(CONTENT_ENCODING_SCOPE, scope),
            uint_el(CONTENT_ENCODING_TYPE, 0),
            master_el(
                CONTENT_COMPRESSION,
                &[uint_el(CONTENT_COMP_ALGO, 3), el(CONTENT_COMP_SETTINGS, stripped)],
            ),
        ],
    )
}

/// Build a `ContentEncoding` element declaring an unsupported compression algorithm.
fn unsupported_compression_encoding(algo: u64) -> Vec<u8> {
    master_el(
        CONTENT_ENCODING,
        &[
            uint_el(CONTENT_ENCODING_ORDER, 0),
            uint_el(CONTENT_ENCODING_SCOPE, 1),
            uint_el(CONTENT_ENCODING_TYPE, 0),
            master_el(CONTENT_COMPRESSION, &[uint_el(CONTENT_COMP_ALGO, algo)]),
        ],
    )
}

/// Build a `ContentEncoding` element declaring encryption.
fn encryption_encoding() -> Vec<u8> {
    master_el(
        CONTENT_ENCODING,
        &[
            uint_el(CONTENT_ENCODING_ORDER, 0),
            uint_el(CONTENT_ENCODING_SCOPE, 1),
            uint_el(CONTENT_ENCODING_TYPE, 1),
            master_el(CONTENT_ENCRYPTION, &[uint_el(CONTENT_ENC_ALGO, 0)]),
        ],
    )
}

/// Build a track entry for a fake MP3 audio track.
fn track_entry(opts: &TrackOpts) -> Vec<u8> {
    let mut children = vec![
        uint_el(TRACK_NUMBER, opts.number),
        uint_el(TRACK_UID, opts.number),
        str_el(CODEC_ID, "A_MPEG/L3"),
        master_el(
            AUDIO,
            &[el(SAMPLING_FREQUENCY, &44100.0f64.to_be_bytes()), uint_el(CHANNELS, 2)],
        ),
    ];

    if let Some(codec_private) = &opts.codec_private {
        children.push(el(CODEC_PRIVATE, codec_private));
    }

    if let Some(encodings) = &opts.content_encodings {
        children.push(master_el(CONTENT_ENCODINGS, std::slice::from_ref(encodings)));
    }

    master_el(TRACK_ENTRY, &children)
}

/// Lacing modes for building a simple block.
enum Lacing {
    None,
    Xiph,
    FixedSize,
    Ebml,
}

/// Build the data of a `SimpleBlock` element for the given frames.
fn simple_block(track_num: u8, rel_ts: i16, frames: &[&[u8]], lacing: Lacing) -> Vec<u8> {
    assert!(track_num < 0x7F);

    let mut block = vec![0x80 | track_num];
    block.extend(rel_ts.to_be_bytes());

    match lacing {
        Lacing::None => {
            assert_eq!(frames.len(), 1);
            block.push(0x80); // Keyframe, no lacing.
            block.extend_from_slice(frames[0]);
        }
        Lacing::Xiph => {
            block.push(0x80 | 0x02); // Keyframe, Xiph lacing.
            block.push(frames.len() as u8 - 1);
            for frame in &frames[..frames.len() - 1] {
                let (q, r) = (frame.len() / 255, frame.len() % 255);
                block.extend(std::iter::repeat_n(255u8, q));
                block.push(r as u8);
            }
            for frame in frames {
                block.extend_from_slice(frame);
            }
        }
        Lacing::FixedSize => {
            assert!(frames.iter().all(|f| f.len() == frames[0].len()));
            block.push(0x80 | 0x04); // Keyframe, fixed-size lacing.
            block.push(frames.len() as u8 - 1);
            for frame in frames {
                block.extend_from_slice(frame);
            }
        }
        Lacing::Ebml => {
            block.push(0x80 | 0x06); // Keyframe, EBML lacing.
            block.push(frames.len() as u8 - 1);
            // First size is an unsigned vint, subsequent sizes are signed vint deltas.
            block.extend(vint_size(frames[0].len()));
            let mut prev = frames[0].len() as i64;
            for frame in &frames[1..frames.len() - 1] {
                let delta = frame.len() as i64 - prev;
                prev = frame.len() as i64;
                // Single-byte signed vint: value biased by 2^6 - 1 = 63, range -63..=64.
                assert!((-63..=64).contains(&delta));
                block.push(0x80 | ((delta + 63) as u8));
            }
            for frame in frames {
                block.extend_from_slice(frame);
            }
        }
    }

    block
}

/// Build a cluster element with the given timestamp (segment ticks) and simple blocks.
fn cluster(ts: u64, blocks: &[Vec<u8>]) -> Vec<u8> {
    let mut children = vec![uint_el(TIMESTAMP, ts)];
    for block in blocks {
        children.push(el(SIMPLE_BLOCK, block));
    }
    master_el(CLUSTER, &children)
}

/// Build a complete Matroska file with the given tracks and clusters.
fn mkv_file(tracks: &[Vec<u8>], clusters: &[Vec<u8>]) -> Vec<u8> {
    let mut file = master_el(EBML, &[str_el(DOC_TYPE, "matroska")]);

    let mut segment_children = vec![
        master_el(
            INFO,
            &[
                uint_el(TIMESTAMP_SCALE, 1_000_000),
                str_el(MUXING_APP, "symphonia-test"),
                str_el(WRITING_APP, "symphonia-test"),
            ],
        ),
        master_el(TRACKS, tracks),
    ];
    segment_children.extend(clusters.iter().cloned());

    file.extend(master_el(SEGMENT, &segment_children));
    file
}

/// Open an MKV file from an in-memory buffer.
fn open(data: Vec<u8>) -> MkvReader<'static> {
    let mss = MediaSourceStream::new(Box::new(Cursor::new(data)), Default::default());
    MkvReader::try_new(mss, Default::default()).expect("failed to open mkv file")
}

/// Get the next packet, failing if the stream ended.
fn next_packet(reader: &mut MkvReader<'_>) -> Result<Packet> {
    reader.next_packet()?.ok_or(Error::DecodeError("unexpected end of stream"))
}

// ---------------------------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------------------------

/// A track declaring header stripping compression must deliver complete frames with the
/// stripped header prepended back.
#[test]
fn header_stripping_no_lacing() -> Result<()> {
    const HEADER: &[u8] = &[0xFF, 0xFB, 0x90, 0x00];

    let track = track_entry(&TrackOpts {
        content_encodings: Some(header_strip_encoding(0, 1, HEADER)),
        ..Default::default()
    });

    let clusters = vec![
        cluster(0, &[simple_block(1, 0, &[&[0xAA, 0xBB]], Lacing::None)]),
        cluster(26, &[simple_block(1, 0, &[&[0xCC]], Lacing::None)]),
    ];

    let mut reader = mkv_file_reader(&[track], &clusters);

    let packet = next_packet(&mut reader)?;
    assert_eq!(&*packet.data, &[0xFF, 0xFB, 0x90, 0x00, 0xAA, 0xBB]);

    let packet = next_packet(&mut reader)?;
    assert_eq!(&*packet.data, &[0xFF, 0xFB, 0x90, 0x00, 0xCC]);

    assert!(reader.next_packet()?.is_none());

    Ok(())
}

/// Header stripping must be applied to every frame of a block, for every lacing mode.
#[test]
fn header_stripping_all_lacing_modes() -> Result<()> {
    const HEADER: &[u8] = &[0xFF, 0xFB];

    for (name, lacing) in
        [("xiph", Lacing::Xiph), ("fixed", Lacing::FixedSize), ("ebml", Lacing::Ebml)]
    {
        let track = track_entry(&TrackOpts {
            content_encodings: Some(header_strip_encoding(0, 1, HEADER)),
            ..Default::default()
        });

        let frames: [&[u8]; 3] = [&[0x11, 0x12], &[0x21, 0x22], &[0x31, 0x32]];
        let clusters = vec![cluster(0, &[simple_block(1, 0, &frames, lacing)])];

        let mut reader = mkv_file_reader(&[track], &clusters);

        for (i, frame) in frames.iter().enumerate() {
            let packet = next_packet(&mut reader)?;

            let mut expected = HEADER.to_vec();
            expected.extend_from_slice(frame);
            assert_eq!(&*packet.data, &expected, "frame {i} for {name} lacing");
        }

        assert!(reader.next_packet()?.is_none());
    }

    Ok(())
}

/// When a track declares multiple content encodings, they must be reversed in descending
/// ContentEncodingOrder.
#[test]
fn header_stripping_multiple_layers_order() -> Result<()> {
    // Encoding: the layer with order 0 is applied first, stripping H0 from the start of each
    // frame. Then the layer with order 1 strips H1 from the result. The stored frame is the
    // body only. Decoding must restore H1 first, then H0.
    const H0: &[u8] = &[0x01, 0x02];
    const H1: &[u8] = &[0x03];
    const BODY: &[u8] = &[0xAA, 0xBB];

    let track = track_entry(&TrackOpts {
        content_encodings: Some(
            [header_strip_encoding(0, 1, H0), header_strip_encoding(1, 1, H1)].concat(),
        ),
        ..Default::default()
    });

    let clusters = vec![cluster(0, &[simple_block(1, 0, &[BODY], Lacing::None)])];

    let mut reader = mkv_file_reader(&[track], &clusters);

    let packet = next_packet(&mut reader)?;
    assert_eq!(&*packet.data, &[0x01, 0x02, 0x03, 0xAA, 0xBB]);

    Ok(())
}

/// When a content encoding declares the codec private scope, the codec private data must be
/// restored before being used to build the codec parameters.
#[test]
fn header_stripping_codec_private() -> Result<()> {
    const HEADER: &[u8] = &[0xDE, 0xAD];
    const STRIPPED_PRIVATE: &[u8] = &[0xBE, 0xEF];

    let track = track_entry(&TrackOpts {
        // Scope 3: applies to both the frames (1) and the codec private data (2).
        content_encodings: Some(header_strip_encoding(0, 3, HEADER)),
        codec_private: Some(STRIPPED_PRIVATE.to_vec()),
        ..Default::default()
    });

    let clusters = vec![cluster(0, &[simple_block(1, 0, &[&[0x77]], Lacing::None)])];

    let mut reader = mkv_file_reader(&[track], &clusters);

    // The codec parameters must have been built from the restored codec private data.
    let track = &reader.tracks()[0];
    let extra_data = track
        .codec_params
        .as_ref()
        .and_then(|params| params.audio().and_then(|audio| audio.extra_data.as_ref()))
        .expect("missing audio extra data");
    assert_eq!(&**extra_data, &[0xDE, 0xAD, 0xBE, 0xEF]);

    // Frames are restored, too.
    let packet = next_packet(&mut reader)?;
    assert_eq!(&*packet.data, &[0xDE, 0xAD, 0x77]);

    Ok(())
}

/// A track declaring encryption must not be decodable: its codec parameters are not populated
/// and its frames are never delivered. Reading its data reports an unsupported error, while
/// other tracks of the same file remain readable.
#[test]
fn encrypted_track_is_not_decodable() -> Result<()> {
    let good_track = track_entry(&TrackOpts { number: 1, ..Default::default() });
    let encrypted_track = track_entry(&TrackOpts {
        number: 2,
        content_encodings: Some(encryption_encoding()),
        ..Default::default()
    });

    let clusters = vec![cluster(
        0,
        &[
            simple_block(2, 0, &[&[0xAA]], Lacing::None),
            simple_block(1, 0, &[&[0xBB]], Lacing::None),
            simple_block(2, 26, &[&[0xCC]], Lacing::None),
            simple_block(1, 26, &[&[0xDD]], Lacing::None),
        ],
    )];

    let mut reader = mkv_file_reader(&[good_track, encrypted_track], &clusters);

    // The encrypted track has no codec parameters.
    assert_eq!(reader.tracks().len(), 2);
    assert!(reader.tracks()[0].codec_params.is_some());
    assert!(reader.tracks()[1].codec_params.is_none());

    // Reading the encrypted track's data reports an unsupported error...
    match reader.next_packet() {
        Err(Error::Unsupported(_)) => (),
        other => panic!("expected unsupported error, got {other:?}"),
    }

    // ...but the other track is still readable...
    let packet = next_packet(&mut reader)?;
    assert_eq!(packet.track_id, 1);
    assert_eq!(&*packet.data, &[0xBB]);

    // ...even after further errors for the encrypted track's frames.
    match reader.next_packet() {
        Err(Error::Unsupported(_)) => (),
        other => panic!("expected unsupported error, got {other:?}"),
    }

    let packet = next_packet(&mut reader)?;
    assert_eq!(packet.track_id, 1);
    assert_eq!(&*packet.data, &[0xDD]);

    assert!(reader.next_packet()?.is_none());

    Ok(())
}

/// A track declaring an unsupported compression (e.g., zlib) must not be decodable, while
/// other tracks of the same file remain readable and seekable.
#[test]
fn unsupported_compression_track_is_not_decodable() -> Result<()> {
    let good_track = track_entry(&TrackOpts { number: 1, ..Default::default() });
    let zlib_track = track_entry(&TrackOpts {
        number: 2,
        content_encodings: Some(unsupported_compression_encoding(0)),
        ..Default::default()
    });

    let clusters = vec![
        cluster(
            0,
            &[
                simple_block(2, 0, &[&[0xAA]], Lacing::None),
                simple_block(1, 0, &[&[0xBB]], Lacing::None),
            ],
        ),
        cluster(1000, &[simple_block(1, 0, &[&[0xCC]], Lacing::None)]),
    ];

    let mut reader = mkv_file_reader(&[good_track, zlib_track], &clusters);

    assert!(reader.tracks()[1].codec_params.is_none());

    // The compressed track reports unsupported when its data is read.
    match reader.next_packet() {
        Err(Error::Unsupported(_)) => (),
        other => panic!("expected unsupported error, got {other:?}"),
    }

    let packet = next_packet(&mut reader)?;
    assert_eq!(&*packet.data, &[0xBB]);

    // The other track remains seekable. The second cluster starts at ts=1000 (segment ticks),
    // which is also 1000 track ticks since the track timestamp scale is 1.0.
    let seeked =
        reader.seek(SeekMode::Coarse, SeekTo::Timestamp { ts: Timestamp::new(1000), track_id: 1 });
    assert!(seeked.is_ok(), "seek failed: {seeked:?}");

    let packet = next_packet(&mut reader)?;
    assert_eq!(packet.track_id, 1);
    assert_eq!(&*packet.data, &[0xCC]);

    Ok(())
}

/// A file without any content encodings must behave exactly as before, and seeking must work.
#[test]
fn no_content_encodings_unchanged() -> Result<()> {
    let track = track_entry(&TrackOpts { number: 1, ..Default::default() });

    let clusters = vec![
        cluster(0, &[simple_block(1, 0, &[&[0x01, 0x02]], Lacing::None)]),
        cluster(500, &[simple_block(1, 0, &[&[0x03]], Lacing::None)]),
    ];

    let mut reader = mkv_file_reader(&[track], &clusters);

    let packet = next_packet(&mut reader)?;
    assert_eq!(&*packet.data, &[0x01, 0x02]);

    // Seek to the second cluster.
    reader.seek(SeekMode::Coarse, SeekTo::Timestamp { ts: Timestamp::new(500), track_id: 1 })?;

    let packet = next_packet(&mut reader)?;
    assert_eq!(&*packet.data, &[0x03]);

    assert!(reader.next_packet()?.is_none());

    Ok(())
}

/// After seeking, delivered frames must still have their content encodings reversed.
#[test]
fn header_stripping_after_seek() -> Result<()> {
    const HEADER: &[u8] = &[0xAB, 0xCD];

    let track = track_entry(&TrackOpts {
        content_encodings: Some(header_strip_encoding(0, 1, HEADER)),
        ..Default::default()
    });

    let clusters = vec![
        cluster(0, &[simple_block(1, 0, &[&[0x01]], Lacing::None)]),
        cluster(500, &[simple_block(1, 0, &[&[0x02]], Lacing::None)]),
    ];

    let mut reader = mkv_file_reader(&[track], &clusters);

    reader.seek(SeekMode::Coarse, SeekTo::Timestamp { ts: Timestamp::new(500), track_id: 1 })?;

    let packet = next_packet(&mut reader)?;
    assert_eq!(&*packet.data, &[0xAB, 0xCD, 0x02]);

    Ok(())
}

/// Helper to build and open a file.
fn mkv_file_reader(tracks: &[Vec<u8>], clusters: &[Vec<u8>]) -> MkvReader<'static> {
    open(mkv_file(tracks, clusters))
}
