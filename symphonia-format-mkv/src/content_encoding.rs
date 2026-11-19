// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Matroska track content encoding (compression / encryption) support.
//!
//! A Matroska track may declare that its frames (and optionally its codec private data) were
//! content encoded when muxed. The most common content encoding is "header stripping"
//! compression: a number of bytes that are identical at the start of every frame (for example,
//! the MP3 or AC-3 frame header) are removed from each frame and stored once in the track
//! headers. A demuxer must reverse the declared content encodings before delivering frames to a
//! decoder.

use crate::segment::ContentEncodingElement;

/// Content encoding scope bit: the encoding applies to the block (frame) data.
const CONTENT_ENCODING_SCOPE_BLOCK: u64 = 1;
/// Content encoding scope bit: the encoding applies to the codec private data.
const CONTENT_ENCODING_SCOPE_PRIVATE: u64 = 2;

/// A content decoder reverses a single supported content encoding.
#[derive(Clone, Debug)]
pub(crate) enum ContentDecoder {
    /// Header stripping compression (ContentCompAlgo = 3). The bytes stripped from the start of
    /// each frame (or of the codec private data) when muxing are prepended back.
    HeaderStrip(Box<[u8]>),
}

impl ContentDecoder {
    /// Decode a single piece of content encoded data.
    fn decode(&self, data: &[u8]) -> Box<[u8]> {
        match self {
            ContentDecoder::HeaderStrip(header) => {
                let mut decoded = Vec::with_capacity(header.len() + data.len());
                decoded.extend_from_slice(header);
                decoded.extend_from_slice(data);
                decoded.into_boxed_slice()
            }
        }
    }
}

/// Decode `data` by applying each content decoder in order.
pub(crate) fn decode_content(decoders: &[ContentDecoder], data: &[u8]) -> Box<[u8]> {
    let mut decoders = decoders.iter();

    let mut data = match decoders.next() {
        Some(decoder) => decoder.decode(data),
        None => return Box::from(data),
    };

    for decoder in decoders {
        data = decoder.decode(&data);
    }

    data
}

/// The content decoders for a track, built from the track's declared content encodings.
#[derive(Debug, Default)]
pub(crate) enum ContentDecoders {
    /// The track declares no content encodings.
    #[default]
    None,
    /// All declared content encodings are supported and can be reversed.
    Supported {
        /// The decoders to apply to each frame, in application order.
        frames: Box<[ContentDecoder]>,
        /// The decoders to apply to the codec private data, in application order.
        private: Box<[ContentDecoder]>,
    },
    /// The track declares an encryption, or a compression that is not supported. The track
    /// cannot be decoded and no data may be delivered for it.
    Unsupported,
}

impl ContentDecoders {
    /// Build the content decoders for a track from its declared content encodings.
    pub(crate) fn new(encodings: &[ContentEncodingElement]) -> Self {
        if encodings.is_empty() {
            return Self::None;
        }

        // When encoding, the content encodings are applied in ascending ContentEncodingOrder.
        // Therefore, when decoding, they are reversed in descending ContentEncodingOrder.
        let mut ordered: Vec<&ContentEncodingElement> = encodings.iter().collect();
        ordered.sort_by_key(|encoding| std::cmp::Reverse(encoding.order));

        let mut frames = Vec::new();
        let mut private = Vec::new();

        for encoding in ordered {
            // Only consider encodings applying to the block (frame) data and/or the codec
            // private data.
            if encoding.scope & (CONTENT_ENCODING_SCOPE_BLOCK | CONTENT_ENCODING_SCOPE_PRIVATE) == 0
            {
                continue;
            }

            // Build the decoder for the encoding, if the encoding is supported.
            let decoder = match encoding.encoding_type {
                // Compression.
                0 if !encoding.has_encryption => match &encoding.compression {
                    Some(compression) => match compression.algo {
                        // Header stripping. The stripped bytes are stored in the compression
                        // settings. If absent, there is nothing to restore.
                        3 => ContentDecoder::HeaderStrip(
                            compression.settings.clone().unwrap_or_default(),
                        ),
                        // zlib, bzlib, lzo1x, or an unknown compression algorithm.
                        _ => return Self::Unsupported,
                    },
                    // A compression encoding without a compression element is malformed.
                    None => return Self::Unsupported,
                },
                // Encryption, or an unknown encoding type.
                _ => return Self::Unsupported,
            };

            if encoding.scope & CONTENT_ENCODING_SCOPE_BLOCK != 0 {
                frames.push(decoder.clone());
            }
            if encoding.scope & CONTENT_ENCODING_SCOPE_PRIVATE != 0 {
                private.push(decoder);
            }
        }

        Self::Supported { frames: frames.into_boxed_slice(), private: private.into_boxed_slice() }
    }

    /// Returns true if the track cannot be decoded because it declares an encryption or an
    /// unsupported compression.
    pub(crate) fn is_unsupported(&self) -> bool {
        matches!(self, Self::Unsupported)
    }

    /// Decode a frame's data, reversing any content encodings applied to it.
    pub(crate) fn decode_frame(&self, data: Box<[u8]>) -> Box<[u8]> {
        match self {
            Self::Supported { frames, .. } if !frames.is_empty() => decode_content(frames, &data),
            _ => data,
        }
    }
}
