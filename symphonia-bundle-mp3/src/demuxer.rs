// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::support_format;

use symphonia_core::checksum::Crc16AnsiLe;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::audio::well_known::{CODEC_ID_MP1, CODEC_ID_MP2, CODEC_ID_MP3};
use symphonia_core::errors::{Error, Result, SeekErrorKind, seek_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::{FORMAT_ID_MP1, FORMAT_ID_MP2, FORMAT_ID_MP3};
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};

use crate::common::{FrameHeader, MpegLayer};
use crate::header::{self, MAX_MPEG_FRAME_SIZE, MPEG_HEADER_LEN};

use std::io::{Seek, SeekFrom};

use log::{debug, info, warn};

const MP1_FORMAT_INFO: FormatInfo =
    FormatInfo { format: FORMAT_ID_MP1, short_name: "mp1", long_name: "MPEG Audio Layer 1 Native" };

const MP2_FORMAT_INFO: FormatInfo =
    FormatInfo { format: FORMAT_ID_MP2, short_name: "mp2", long_name: "MPEG Audio Layer 2 Native" };

const MP3_FORMAT_INFO: FormatInfo =
    FormatInfo { format: FORMAT_ID_MP3, short_name: "mp3", long_name: "MPEG Audio Layer 3 Native" };

/// MPEG1 and MPEG2 audio elementary stream reader.
///
/// `MpaReader` implements a demuxer for the MPEG1 and MPEG2 audio elementary stream.
pub struct MpaReader<'s> {
    reader: MediaSourceStream<'s>,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    chapters: Option<ChapterGroup>,
    metadata: MetadataLog,
    first_packet_pos: u64,
    next_packet_ts: Timestamp,
    /// A seek index reconstructed from a Xing/Info or VBRI tag's time-to-byte mapping table. When
    /// available, coarse seeks use the index instead of a linear estimation over the stream.
    seek_index: Option<SeekIndex>,
}

impl Scoreable for MpaReader<'_> {
    fn score(mut src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        // Read the sync word for the first (assumed) MPEG frame and try to parse it into a header.
        let sync1 = header::read_frame_header_word_no_sync(&mut src)?;
        let hdr1 = header::parse_frame_header(sync1)?;

        // Since the first header was parsed successfully, this may be a MPEG audio format. However,
        // if there is enough data left to read the frame body and another frame header, then a
        // higher confidence may be gained. If there is not enough data left, return a partially
        // confident score.
        if src.bytes_available() < (hdr1.frame_size + header::MPEG_HEADER_LEN) as u64 {
            return Ok(Score::Supported(127));
        }

        // Skip the frame body.
        src.ignore_bytes(hdr1.frame_size as u64)?;

        // Read another sync word for the second (assumed) MPEG frame.
        let sync2 = header::read_frame_header_word_no_sync(&mut src)?;

        // The second sync word should look like a sync word.
        if !header::is_frame_header_word_synced(sync2) {
            return Ok(Score::Unsupported);
        }

        // Try to parse the second sync word into a header.
        let _ = header::parse_frame_header(sync2)?;

        Ok(Score::Supported(255))
    }
}

impl ProbeableFormat<'_> for MpaReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>> {
        Ok(Box::new(MpaReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[
            // Layer 1
            support_format!(
                MP1_FORMAT_INFO,
                &["mp1"],
                &["audio/mpeg", "audio/mp1"],
                &[
                    &[0xff, 0xfe], // MPEG 1 with CRC
                    &[0xff, 0xff], // MPEG 1
                    &[0xff, 0xf6], // MPEG 2 with CRC
                    &[0xff, 0xf7], // MPEG 2
                    &[0xff, 0xe6], // MPEG 2.5 with CRC
                    &[0xff, 0xe7], // MPEG 2.5
                ]
            ),
            // Layer 2
            support_format!(
                MP2_FORMAT_INFO,
                &["mp2"],
                &["audio/mpeg", "audio/mp2"],
                &[
                    &[0xff, 0xfc], // MPEG 1 with CRC
                    &[0xff, 0xfd], // MPEG 1
                    &[0xff, 0xf4], // MPEG 2 with CRC
                    &[0xff, 0xf5], // MPEG 2
                    &[0xff, 0xe4], // MPEG 2.5 with CRC
                    &[0xff, 0xe5], // MPEG 2.5
                ]
            ),
            // Layer 3
            support_format!(
                MP3_FORMAT_INFO,
                &["mp3"],
                &["audio/mpeg", "audio/mp3"],
                &[
                    &[0xff, 0xfa], // MPEG 1 with CRC
                    &[0xff, 0xfb], // MPEG 1
                    &[0xff, 0xf2], // MPEG 2 with CRC
                    &[0xff, 0xf3], // MPEG 2
                    &[0xff, 0xe2], // MPEG 2.5 with CRC
                    &[0xff, 0xe3], // MPEG 2.5
                ]
            ),
        ]
    }
}

impl FormatReader for MpaReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        // Safety: MpaReader only supports/has audio tracks.
        match self.tracks[0]
            .codec_params
            .as_ref()
            .expect("track has codec params")
            .audio()
            .expect("codec params are audio")
            .codec
        {
            CODEC_ID_MP1 => &MP1_FORMAT_INFO,
            CODEC_ID_MP2 => &MP2_FORMAT_INFO,
            CODEC_ID_MP3 => &MP3_FORMAT_INFO,
            _ => unreachable!(),
        }
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        let (header, data) = loop {
            // Read the next MPEG frame.
            let (header, data) = match read_mpeg_frame(&mut self.reader) {
                Ok(frame) => frame,
                Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // MPEG streams have no well-defined end, so when no more frames can be read,
                    // consider the stream ended.
                    return Ok(None);
                }
                Err(err) => return Err(err),
            };

            // Check if the packet contains a Xing, Info, or VBRI tag.
            if is_maybe_info_tag(&data, &header) {
                if try_read_info_tag(&data, &header).is_some() {
                    // Discard the packet and tag since it was not at the start of the stream.
                    warn!("found an unexpected xing tag, discarding");
                    continue;
                }
            }
            else if is_maybe_vbri_tag(&data, &header)
                && try_read_vbri_tag(&data, &header).is_some()
            {
                // Discard the packet and tag since it was not at the start of the stream.
                warn!("found an unexpected vbri tag, discarding");
                continue;
            }

            break (header, data);
        };

        // The timestamp and duration for this packet.
        let pts = self.next_packet_ts;
        let dur = header.duration();

        // Advance the next packet timestamp based on this packet's duration. If it saturates, then
        // it is not possible to read further.
        self.next_packet_ts = match self.next_packet_ts.checked_add(dur) {
            Some(ts) => ts,
            None => return Ok(None),
        };

        // Build the packet.
        let packet = PacketBuilder::new()
            .track_id(0)
            .pts(pts)
            .trimmed_dur(
                dur,
                self.tracks[0]
                    .num_frames
                    .map(Duration::from)
                    .and_then(|dur| dur.timestamp_from(Timestamp::ZERO)),
            )
            .data(data)
            .build();

        Ok(Some(packet))
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn chapters(&self) -> Option<&ChapterGroup> {
        self.chapters.as_ref()
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn seek(&mut self, mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        const MAX_REF_FRAMES: usize = 4;
        const REF_FRAMES_MASK: usize = MAX_REF_FRAMES - 1;

        // Get the timestamp of the desired audio frame.
        let required_ts = match to {
            // Frame timestamp given.
            SeekTo::Timestamp { ts, .. } => ts,
            // Time value given, calculate frame timestamp using the timebase.
            SeekTo::Time { time, .. } => {
                // The timebase is required to calculate the timestamp.
                let tb =
                    self.tracks[0].time_base.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;

                // If the timestamp overflows, the seek if out-of-range.
                tb.calc_timestamp(time).ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?
            }
        };

        // Get the total duration of the track (excludes delay and padding).
        let dur_ts = self.tracks[0].num_frames.map(Duration::from);

        // Get the delay and padding values (or 0 if unknown).
        let delay = self.tracks[0].delay.unwrap_or(0);
        let padding = self.tracks[0].padding.unwrap_or(0);

        // Calculate the valid timestamp bounds of the track.
        let min_ts = Timestamp::from(-i64::from(delay));
        let max_ts = dur_ts
            .and_then(|dur| min_ts.checked_add(dur))
            .and_then(|dur| dur.checked_add(Duration::from(delay + padding)));

        // Ensure the seek position is within the bounds of the track.
        if required_ts < min_ts {
            return seek_error(SeekErrorKind::OutOfRange);
        }
        else if let Some(max_ts) = max_ts {
            if required_ts > max_ts {
                return seek_error(SeekErrorKind::OutOfRange);
            }
        }

        let is_seekable = self.reader.is_seekable();

        // If the stream is unseekable and the required timestamp in the past, then return an
        // error, it is not possible to seek to it.
        if !is_seekable && required_ts < self.next_packet_ts {
            return seek_error(SeekErrorKind::ForwardOnly);
        }

        debug!("seeking to ts={required_ts}");

        // Step 1
        //
        // In coarse seek mode, the underlying media source stream will be roughly seeked based on
        // the required timestamp and the total duration of the media. Coarse seek mode requires a
        // seekable stream because the total length in bytes of the stream is required. If a Xing or
        // VBRI tag provides a time-to-byte mapping table, the mapping table is used to obtain the
        // approximate byte position instead of a linear estimation so that the position remains
        // accurate for variable bitrate streams.
        //
        // In accurate seek mode, the underlying media source stream will not be seeked unless the
        // required timestamp is in the past, in which case the stream is seeked back to the start.
        match mode {
            SeekMode::Coarse if is_seekable => self.preseek_coarse(required_ts, min_ts, max_ts)?,
            SeekMode::Accurate => self.preseek_accurate(required_ts, min_ts)?,
            _ => (),
        };

        // Step 2
        //
        // Following the pre-seek operation above, parse MPEG frames (packets) one-by-one from the
        // current position in the stream until the frame containing the desired timestamp is
        // reached. For coarse seeks, this should only parse a few packets. For accurate seeks, the
        // entire stream could potentially be parsed.
        let mut frames: [FramePos; MAX_REF_FRAMES] = Default::default();
        let mut n_parsed = 0;

        loop {
            // Sync to the next frame.
            let sync = match header::sync_frame(&mut self.reader) {
                Ok(sync) => sync,
                Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // MPEG streams have no well-defined end, so if no more frames can be read then
                    // assume the seek position is out-of-range. This would normally only happen if
                    // the duration of the track is unknown, or it is was longer than the track
                    // actually is.
                    return seek_error(SeekErrorKind::OutOfRange);
                }
                Err(err) => return Err(err),
            };

            // Parse the synced frame header.
            let header = header::parse_frame_header(sync)?;

            // Position of the frame header.
            let pos = self.reader.pos() - header::MPEG_HEADER_LEN as u64;

            // Calculate the duration of the frame.
            let frame_dur = header.duration();

            // Add the frame to the frame ring.
            frames[n_parsed & REF_FRAMES_MASK] = FramePos { pos, ts: self.next_packet_ts };
            n_parsed += 1;

            let next_packet_ts = match self.next_packet_ts.checked_add(frame_dur) {
                Some(ts) if ts <= required_ts => ts,
                // The timestamp of the next MPEG frame exceeds the desired timestamp, or it
                // exceeds the representable range. Rewind back to the start of the current frame
                // and end the search.
                _ => {
                    // The main_data_begin offset is a negative offset from the frame's header to
                    // where its main data begins. Therefore, for a decoder to properly decode this
                    // frame, the reader must provide previous (reference) frames up-to and
                    // including the frame that contains the first byte this frame's main_data.
                    let main_data_begin = read_main_data_begin(&mut self.reader, &header)? as u64;

                    debug!(
                        "found frame with ts={} @ pos={} with main_data_begin={}",
                        self.next_packet_ts, pos, main_data_begin
                    );

                    // The number of reference frames is 0 if main_data_begin is also 0. Otherwise,
                    // attempt to find the first (oldest) reference frame, then select 1 frame
                    // before that one to actually seek to.
                    let mut n_ref_frames = 0;
                    let mut ref_frame = &frames[(n_parsed - 1) & REF_FRAMES_MASK];

                    if main_data_begin > 0 {
                        // The maximum number of reference frames is limited to the number of frames
                        // read and the number of previous frames recorded.
                        let max_ref_frames = std::cmp::min(n_parsed, frames.len());

                        while n_ref_frames < max_ref_frames {
                            ref_frame = &frames[(n_parsed - n_ref_frames - 1) & REF_FRAMES_MASK];

                            if pos - ref_frame.pos >= main_data_begin {
                                break;
                            }

                            n_ref_frames += 1;
                        }

                        debug!(
                            "will seek -{} frame(s) to ts={} @ pos={} (-{} bytes)",
                            n_ref_frames,
                            ref_frame.ts,
                            ref_frame.pos,
                            pos - ref_frame.pos
                        );
                    }

                    // Do the actual seek to the reference frame.
                    self.reader.seek_buffered(ref_frame.pos);

                    self.next_packet_ts = ref_frame.ts;
                    break;
                }
            };

            // Otherwise, ignore the frame body.
            self.reader.ignore_bytes(header.frame_size as u64)?;

            // Increment the timestamp for the next packet.
            self.next_packet_ts = next_packet_ts;
        }

        debug!(
            "seeked to ts={} (delta={})",
            self.next_packet_ts,
            self.next_packet_ts.saturating_delta(required_ts),
        );

        Ok(SeekedTo { track_id: 0, required_ts, actual_ts: self.next_packet_ts })
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.reader
    }
}

impl<'s> MpaReader<'s> {
    pub fn try_new(mut mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        // Try to read the first MPEG frame.
        let (header, packet) = read_mpeg_frame_strict(&mut mss)?;

        // Use the header to populate the codec parameters.
        let mut codec_params = AudioCodecParameters::new();

        codec_params
            .for_codec(header.codec())
            .with_sample_rate(header.sample_rate)
            .with_channels(header.channel_mode.channels());

        // Create the track.
        let mut track = Track::new(0);

        track.with_codec_params(CodecParameters::Audio(codec_params));

        // Check if there is a Xing/Info tag contained in the first frame.
        let info_tag = try_read_info_tag(&packet, &header);
        let vbri_tag = if info_tag.is_some() { None } else { try_read_vbri_tag(&packet, &header) };

        // A seek index, reconstructed from the tag's time-to-byte mapping table.
        let mut seek_index = None;

        if let Some(info_tag) = &info_tag {
            // The LAME tag contains ReplayGain and padding information.
            if let Some(lame_tag) = &info_tag.lame {
                track.with_delay(lame_tag.enc_delay).with_padding(lame_tag.enc_padding);
            }

            // The base Xing/Info tag may contain the number of MPEG frames.
            if let Some(num_mpeg_frames) = info_tag.num_frames {
                info!("using xing header for duration");

                // The total number of audio frames including delay and padding frames.
                let num_frames = u64::from(num_mpeg_frames) * u64::from(header.num_frames());

                // Remove delay and padding frames.
                let discard = track.delay.unwrap_or(0) + track.padding.unwrap_or(0);

                track.with_num_frames(num_frames.saturating_sub(u64::from(discard)));
            }

            // A Xing/Info tag may provide a table of contents mapping points in time to byte
            // offsets within the stream. The table spans the full encoded duration, including
            // encoder delay and padding.
            if let Some(toc) = info_tag.toc {
                let total_ts =
                    u64::from(info_tag.num_frames.unwrap_or(0)) * u64::from(header.num_frames());

                // The table's byte offsets are fractions of the stream's audio data length. The
                // length is only known for seekable media sources.
                let audio_byte_len = mss.byte_len().map(|len| len.saturating_sub(mss.pos()));

                seek_index =
                    audio_byte_len.and_then(|len| SeekIndex::from_xing_toc(toc, total_ts, len));

                if seek_index.is_some() {
                    info!("using xing table of contents for coarse seeks");
                }
                else {
                    warn!("xing table of contents is invalid, it will not be used");
                }
            }
        }
        else if let Some(vbri_tag) = &vbri_tag {
            info!("using vbri header for duration");

            let num_frames = u64::from(vbri_tag.num_mpeg_frames) * u64::from(header.num_frames());

            // Check if there is a VBRI tag.
            track.with_num_frames(num_frames);

            // A VBRI tag always carries a seek table mapping groups of MPEG frames to byte offsets
            // within the stream.
            let total_ts = u64::from(vbri_tag.num_mpeg_frames) * u64::from(header.num_frames());

            seek_index = SeekIndex::from_vbri_entries(
                &vbri_tag.entries,
                vbri_tag.entry_scale,
                vbri_tag.frames_per_entry,
                total_ts,
                &header,
            );

            if seek_index.is_some() {
                info!("using vbri seek table for coarse seeks");
            }
            else {
                warn!("vbri seek table is invalid, it will not be used");
            }
        }
        else {
            // The first frame was not a Xing/Info header, rewind back to the start of the frame so
            // that it may be decoded.
            mss.seek_buffered_rev(MPEG_HEADER_LEN + header.frame_size);

            // Likely not a VBR file, so estimate the duration if seekable.
            if mss.is_seekable() {
                info!("estimating duration from bitrate, may be inaccurate for vbr files");

                if let Some(n_mpeg_frames) = estimate_num_mpeg_frames(&mut mss) {
                    track.with_num_frames(n_mpeg_frames * u64::from(header.num_frames()));
                }
            }
        }

        if let Some(num_frames) = track.num_frames {
            // Duration equals the number of frames because the timebase is always 1 / sample rate.
            track.with_duration(Duration::from(num_frames));
        }

        let first_packet_pos = mss.pos();
        let next_packet_ts = Timestamp::from(-i64::from(track.delay.unwrap_or(0)));

        Ok(MpaReader {
            reader: mss,
            media_info: MediaInfo::from_track(&track),
            tracks: vec![track],
            chapters: opts.external_data.chapters,
            metadata: opts.external_data.metadata.unwrap_or_default(),
            first_packet_pos,
            next_packet_ts,
            seek_index,
        })
    }

    /// Seeks the media source stream to a byte position roughly where the packet with the required
    /// timestamp should be located.
    fn preseek_coarse(
        &mut self,
        required_ts: Timestamp,
        min_ts: Timestamp,
        max_ts: Option<Timestamp>,
    ) -> Result<()> {
        // Get the length in bytes of the stream's audio data. It is not possible to coarsely seek
        // without knowing this.
        let audio_byte_len = match self.reader.byte_len() {
            Some(byte_len) => byte_len - self.first_packet_pos,
            None => return seek_error(SeekErrorKind::Unseekable),
        };

        // It is not possible to coarse seek without knowing both the upper and lower timestamp
        // bounds, and the lower timestamp bound must be <= the upper timestamp bound.
        let max_ts = match max_ts {
            Some(max_ts) if max_ts >= min_ts => max_ts,
            _ => return seek_error(SeekErrorKind::Unseekable),
        };

        // Call invariants.
        debug_assert!(min_ts <= required_ts);
        debug_assert!(required_ts <= max_ts);

        // If the stream's tag provides a time-to-byte mapping table, use it to find the approximate
        // byte position. The table's accuracy determines the error of the coarse seek regardless of
        // any variation in the bitrate. If the table cannot be used for some reason, fall back to
        // the linear estimation below.
        if let Some(seek_index) = self.seek_index.take() {
            match self.preseek_coarse_indexed(&seek_index, required_ts, min_ts, max_ts) {
                Ok(()) => {
                    self.seek_index = Some(seek_index);
                    return Ok(());
                }
                Err(err) => {
                    // Restore the index and fall back to the linear estimation.
                    self.seek_index = Some(seek_index);
                    warn!(
                        "failed to use the seek index ({err}), falling back to linear estimation"
                    );
                }
            }
        }

        self.preseek_coarse_linear(required_ts, min_ts, max_ts, audio_byte_len)
    }

    /// Seeks the media source stream to a byte position, estimated from the linear relationship
    /// between the stream's total duration and length in bytes.
    fn preseek_coarse_linear(
        &mut self,
        required_ts: Timestamp,
        min_ts: Timestamp,
        max_ts: Timestamp,
        audio_byte_len: u64,
    ) -> Result<()> {
        // The total duration of the track.
        let total_dur = u128::from(max_ts.abs_delta(min_ts).get());

        // Calculate, roughly, the byte position of where the MPEG frame containing the required
        // timestamp should be in the media source stream relative to the start of the audio data.
        let seek_pos_rel = (u128::from(required_ts.abs_delta(min_ts).get())
            * u128::from(audio_byte_len))
            / total_dur;

        // It is preferable to return a packet with a timestamp before the requested timestamp.
        // Therefore, subtract the maximum packet size from the position found above to ensure this.
        let seek_pos = (seek_pos_rel + u128::from(self.first_packet_pos))
            .saturating_sub(MAX_MPEG_FRAME_SIZE as u128);

        // Seek the media source stream.
        self.reader.seek(SeekFrom::Start(
            seek_pos.try_into().map_err(|_| Error::SeekError(SeekErrorKind::OutOfRange))?,
        ))?;

        // Resync to the start of the next MPEG frame.
        let (header, _) = read_mpeg_frame_strict(&mut self.reader)?;

        // The byte position of the next MPEG frame relative to the start of the audio data.
        let audio_byte_pos = u128::from(self.reader.pos() - self.first_packet_pos);

        // Calculate, roughly, the duration from the start of the audio of the packet based on the
        // byte position after resync.
        let dur_to_pkt =
            Duration::from(((audio_byte_pos * total_dur) / u128::from(audio_byte_len)) as u64);

        // Assume the duration of a packet remains constant throughout the stream (not a
        // guarantee, but usually the case).
        let pkt_dur = header.duration();

        // Compute the timestamp of the next packet. First align the estimated duration from the
        // start of the audio to a packet boundary. Then add to the lower-bound timestamp. In
        // theory, this should never exceed the upper-bound timestamp.
        self.next_packet_ts = min_ts
            .checked_add(
                dur_to_pkt.align_down(pkt_dur).expect("packet duration is always non-zero"),
            )
            .unwrap_or(max_ts);

        Ok(())
    }

    /// Seeks the media source stream to a byte position, estimated from the time-to-byte mapping
    /// table stored in the stream's Xing/Info or VBRI tag.
    fn preseek_coarse_indexed(
        &mut self,
        seek_index: &SeekIndex,
        required_ts: Timestamp,
        min_ts: Timestamp,
        max_ts: Timestamp,
    ) -> Result<()> {
        // The total encoded duration, in timebase units, covered by the seek index. The index's
        // mapping table is indexed relative to the start of the encoded audio which begins at the
        // lower-bound timestamp.
        let total_dur = max_ts.abs_delta(min_ts).get();

        // Sanity check that the duration used to build the index matches the track's current
        // bounds. If they do not agree, the mapping table cannot be used reliably, so fall back to
        // the linear estimation.
        if seek_index.total_ts() != total_dur {
            return seek_error(SeekErrorKind::Unseekable);
        }

        // The offset, in timebase units, of the requested timestamp relative to the start of the
        // encoded audio.
        let req_offset = required_ts.abs_delta(min_ts).get();

        // Select an anchor at, or before, the requested timestamp. The anchor is a real frame
        // boundary with an exact timestamp, so seeking to it keeps the subsequent frame scan
        // consistent.
        let anchor = seek_index.seek_anchor(req_offset);

        // It is preferable to begin slightly before the anchor so that any reference (main data)
        // preceding the anchor's frame is available to the decoder. The following resync discards
        // any partial frame and lands on the anchor or a later, valid frame. Clamp to the start of
        // the audio data.
        let seek_pos_rel =
            u128::from(anchor.byte_offset).saturating_sub(MAX_MPEG_FRAME_SIZE as u128);

        let seek_pos = u128::from(self.first_packet_pos) + seek_pos_rel;

        self.reader.seek(SeekFrom::Start(
            seek_pos.try_into().map_err(|_| Error::SeekError(SeekErrorKind::OutOfRange))?,
        ))?;

        // Resync to, and consume, the next valid MPEG frame. This mirrors the linear estimation
        // path: the consumed frame's timestamp is assigned below, and the frame-by-frame scan
        // begins on the frame immediately following it.
        let (resync_header, _) = read_mpeg_frame_strict(&mut self.reader)?;

        // Because the seek landed no later than the anchor, the resynced frame is the anchor frame
        // or a later one. In either case it lies at, or before, the requested timestamp. Its exact
        // timestamp is the anchor's timestamp plus one frame duration for the frame the scan will
        // encounter first.
        let next_frame_dur = resync_header.duration();

        let next_frame_offset =
            anchor.ts_offset.checked_add(next_frame_dur.get()).unwrap_or(req_offset);

        // Never label a frame past the requested timestamp; such a label would cause the scan to
        // overshoot. The anchor is always at, or before, the request, so clamping here only removes
        // a possible single-frame overshoot when the target is the anchor itself.
        let next_frame_offset = next_frame_offset.min(req_offset);

        self.next_packet_ts =
            min_ts.checked_add(Duration::from(next_frame_offset)).unwrap_or(max_ts);

        Ok(())
    }

    /// Seeks the media source stream back to the start of the first packet if the required
    /// timestamp is in the past.
    fn preseek_accurate(&mut self, required_ts: Timestamp, min_ts: Timestamp) -> Result<()> {
        if required_ts < self.next_packet_ts {
            let seeked_pos = self.reader.seek(SeekFrom::Start(self.first_packet_pos))?;

            // Since the elementary stream has no timestamp information, the position seeked
            // to must be exactly as requested.
            if seeked_pos != self.first_packet_pos {
                return seek_error(SeekErrorKind::Unseekable);
            }

            // Successfuly seeked to the start of the stream, reset the next packet timestamp.
            self.next_packet_ts = min_ts;
        }

        Ok(())
    }
}

/// Reads a MPEG frame and returns the header and buffer.
fn read_mpeg_frame(reader: &mut MediaSourceStream<'_>) -> Result<(FrameHeader, Vec<u8>)> {
    let (header, header_word) = loop {
        // Sync to the next frame header.
        let sync = header::sync_frame(reader)?;

        // Parse the frame header fully.
        if let Ok(header) = header::parse_frame_header(sync) {
            break (header, sync);
        }

        warn!("invalid mpeg audio header");
    };

    // Allocate frame buffer.
    let mut packet = vec![0u8; MPEG_HEADER_LEN + header.frame_size];
    packet[0..MPEG_HEADER_LEN].copy_from_slice(&header_word.to_be_bytes());

    // Read the frame body.
    reader.read_buf_exact(&mut packet[MPEG_HEADER_LEN..])?;

    // Return the parsed header and packet body.
    Ok((header, packet))
}

/// Reads a MPEG frame and checks if the next frame begins after the packet.
fn read_mpeg_frame_strict(reader: &mut MediaSourceStream<'_>) -> Result<(FrameHeader, Vec<u8>)> {
    loop {
        // Read the next MPEG frame.
        let (header, packet) = read_mpeg_frame(reader)?;

        // Get the position before trying to read the next header.
        let pos = reader.pos();

        // Read a sync word from the stream. If this read fails then the file may have ended and
        // this check cannot be performed.
        if let Ok(sync) = header::read_frame_header_word_no_sync(reader) {
            // If the stream is not synced to the next frame's sync word, or the next frame header
            // is not parseable or similar to the current frame header, then reject the current
            // packet since the stream likely synced to random data.
            if !header::is_frame_header_word_synced(sync) || !is_frame_header_similar(&header, sync)
            {
                warn!("skipping junk at {} bytes", pos - packet.len() as u64);

                // Seek back to the second byte of the rejected packet to prevent syncing to the
                // same spot again.
                reader.seek_buffered_rev(packet.len() + MPEG_HEADER_LEN - 1);
                continue;
            }
        }

        // Jump back to the position before the next header was read.
        reader.seek_buffered(pos);

        break Ok((header, packet));
    }
}

/// Check if a sync word parses to a frame header that is similar to the one provided.
fn is_frame_header_similar(header: &FrameHeader, sync: u32) -> bool {
    if let Ok(candidate) = header::parse_frame_header(sync) {
        if header.version == candidate.version
            && header.layer == candidate.layer
            && header.sample_rate == candidate.sample_rate
            && header.n_channels() == candidate.n_channels()
        {
            return true;
        }
    }

    false
}

#[derive(Default)]
struct FramePos {
    ts: Timestamp,
    pos: u64,
}

/// A single point of a seek index relating an offset in time, in timebase units, to a byte offset
/// relative to the start of the audio data in the media source stream.
#[derive(Copy, Clone, Debug)]
struct SeekPoint {
    /// The offset in timebase units from the start of the encoded audio.
    ts_offset: u64,
    /// The offset in bytes from the start of the audio data.
    byte_offset: u64,
}

/// A seek index reconstructed from the time-to-byte mapping table stored in a Xing/Info or VBRI
/// tag.
///
/// The index is stored as a list of anchor points. Each anchor is a real MPEG frame boundary whose
/// byte offset and timestamp are both known exactly relative to the start of the audio data. Seeking
/// to an anchor therefore lands on a frame whose timestamp can be assigned exactly, after which a
/// simple one-frame-at-a-time scan reaches the requested frame. This keeps the accuracy of a coarse
/// seek bounded by the spacing of the mapping table entries rather than the variation of the
/// stream's bitrate.
///
/// All byte offsets are relative to the start of the audio data in the media source stream, and all
/// timestamps are relative to the start of the encoded audio.
#[derive(Clone, Debug)]
struct SeekIndex {
    /// The anchor points, ordered by increasing time and byte offset.
    anchors: Vec<SeekPoint>,
    /// The total number of timebase units spanned by the encoded audio.
    total_ts: u64,
}

impl SeekIndex {
    /// Reconstruct a seek index from a Xing/Info tag's 100-entry table of contents.
    ///
    /// `audio_byte_len` is the length in bytes of the stream's audio data. Each table entry is a
    /// byte offset expressed as a fraction of the audio data length in units of 1/256th. The 100
    /// entries are spaced evenly across the full encoded duration.
    ///
    /// Returns `None` if the table is empty, invalid, or the audio length is unknown.
    fn from_xing_toc(toc: [u8; 100], total_ts: u64, audio_byte_len: u64) -> Option<Self> {
        if total_ts == 0 || audio_byte_len == 0 {
            return None;
        }

        // The first entry should always be 0. Reject the table if it is not since it is likely
        // corrupt.
        if toc[0] != 0 {
            return None;
        }

        // All entries must be monotonically non-decreasing for the table to describe a valid
        // mapping.
        if toc.windows(2).any(|w| w[0] > w[1]) {
            return None;
        }

        // The table only spans a meaningful duration if at least one non-zero entry is present.
        if toc.iter().all(|&v| v == 0) {
            return None;
        }

        let mut anchors = Vec::with_capacity(100);

        for (idx, &entry) in toc.iter().enumerate() {
            // Entry `idx` describes the frame at `idx / 100` of the encoded duration.
            let ts_offset = u128::from(idx as u64) * u128::from(total_ts) / 100;
            let byte_offset = u128::from(entry) * u128::from(audio_byte_len) / 256;

            anchors
                .push(SeekPoint { ts_offset: ts_offset as u64, byte_offset: byte_offset as u64 });
        }

        Some(SeekIndex { anchors, total_ts })
    }

    /// Reconstruct a seek index from a VBRI tag's seek table entries. The raw entries are ordered by
    /// increasing time and must each be multiplied by `entry_scale` to obtain their byte offsets
    /// relative to the start of the audio data. Consecutive entries are separated by
    /// `frames_per_entry` MPEG frames, and entry 0 describes the frames beginning at the start of the
    /// audio.
    ///
    /// Returns `None` if the table is empty or invalid.
    fn from_vbri_entries(
        raw_entries: &[u32],
        entry_scale: u16,
        frames_per_entry: u16,
        total_ts: u64,
        header: &FrameHeader,
    ) -> Option<Self> {
        if raw_entries.is_empty() || total_ts == 0 || frames_per_entry == 0 || entry_scale == 0 {
            return None;
        }

        let frames_per_entry = u64::from(frames_per_entry);
        let frame_dur = u64::from(header.num_frames());

        let mut anchors: Vec<SeekPoint> = Vec::with_capacity(raw_entries.len());

        for (idx, &raw) in raw_entries.iter().enumerate() {
            // Apply the scale factor to obtain the byte offset relative to the start of the audio
            // data.
            let byte_offset = u64::from(raw).saturating_mul(u64::from(entry_scale));

            // Offsets must be monotonically non-decreasing. A zero first entry is valid because it
            // simply points to the start of the audio data, but a backwards offset later in the
            // table indicates corruption.
            if let Some(prev) = anchors.last() {
                if byte_offset < prev.byte_offset {
                    return None;
                }
            }

            // Entry 0 describes the frames starting at the beginning of the encoded audio.
            let ts_offset =
                u64::try_from(idx).ok()?.saturating_mul(frames_per_entry) * frame_dur;

            anchors.push(SeekPoint { ts_offset, byte_offset });
        }

        Some(SeekIndex { anchors, total_ts })
    }

    /// The total number of timebase units spanned by the encoded audio described by the index.
    fn total_ts(&self) -> u64 {
        self.total_ts
    }

    /// Select the anchor point to seek to for a requested timestamp offset, in timebase units from
    /// the start of the encoded audio.
    ///
    /// The selected anchor is the latest anchor whose timestamp is at, or before, the requested
    /// timestamp, after skipping a few anchors backwards so that several frames precede the target.
    /// Those preceding frames supply any reference (main data) the decoder may require and amortize
    /// the limited precision of the mapping table. The returned anchor always lies within the valid
    /// audio range.
    fn seek_anchor(&self, req_offset: u64) -> SeekPoint {
        // Find the first anchor strictly after the requested timestamp. The anchor immediately
        // before it is therefore the latest anchor at, or before, the request.
        let end = self.anchors.partition_point(|anchor| anchor.ts_offset <= req_offset);

        // The latest anchor at, or before, the request. If the request precedes every anchor, start
        // from the first anchor.
        let target_idx = end.saturating_sub(1);

        // Skip a few anchors backwards to leave reference frames ahead of the seek point. Never
        // index past the first anchor.
        const BACKTRACK_ANCHORS: usize = 2;
        let anchor_idx = target_idx.saturating_sub(BACKTRACK_ANCHORS);

        self.anchors[anchor_idx]
    }
}

/// Reads the main_data_begin field from the side information of a MPEG audio frame.
fn read_main_data_begin<B: ReadBytes>(reader: &mut B, header: &FrameHeader) -> Result<u16> {
    // After the head the optional CRC is present.
    if header.has_crc {
        let _crc = reader.read_be_u16()?;
    }

    // For MPEG version 1 the first 9 bits is main_data_begin.
    let main_data_begin = if header.is_mpeg1() {
        reader.read_be_u16()? >> 7
    }
    // For MPEG version 2 the first 8 bits is main_data_begin.
    else {
        u16::from(reader.read_u8()?)
    };

    Ok(main_data_begin)
}

/// Estimates the total number of MPEG frames in the media source stream.
fn estimate_num_mpeg_frames(reader: &mut MediaSourceStream<'_>) -> Option<u64> {
    const MAX_FRAMES: u32 = 16;
    const MAX_LEN: usize = 16 * 1024;

    // Macro to convert a Result to Option, and break a loop on exit.
    macro_rules! break_on_err {
        ($expr:expr) => {
            match $expr {
                Ok(a) => a,
                _ => break None,
            }
        };
    }

    let start_pos = reader.pos();

    let mut total_frame_len = 0;
    let mut total_frames = 0;

    let total_len = match reader.byte_len() {
        Some(len) => len - start_pos,
        _ => return None,
    };

    let num_mpeg_frames = loop {
        // Read the frame header.
        let header_val = break_on_err!(reader.read_be_u32());

        // Parse the frame header.
        let header = break_on_err!(header::parse_frame_header(header_val));

        // Tabulate the size.
        total_frame_len += MPEG_HEADER_LEN + header.frame_size;
        total_frames += 1;

        // Ignore the frame body.
        break_on_err!(reader.ignore_bytes(header.frame_size as u64));

        // Read up-to 16 frames, or 16kB, then calculate the average MPEG frame length, and from
        // that, the total number of MPEG frames.
        if total_frames > MAX_FRAMES || total_frame_len > MAX_LEN {
            let avg_mpeg_frame_len = total_frame_len as f64 / total_frames as f64;
            break Some((total_len as f64 / avg_mpeg_frame_len) as u64);
        }
    };

    // Rewind back to the first frame seen upon entering this function.
    reader.seek_buffered_rev((reader.pos() - start_pos) as usize);

    num_mpeg_frames
}

const XING_TAG_ID: [u8; 4] = *b"Xing";
const INFO_TAG_ID: [u8; 4] = *b"Info";

/// The LAME tag is an extension to the Xing/Info tag.
#[allow(dead_code)]
struct LameTag {
    encoder: String,
    replaygain_peak: Option<f32>,
    replaygain_radio: Option<f32>,
    replaygain_audiophile: Option<f32>,
    enc_delay: u32,
    enc_padding: u32,
}

/// The Xing/Info time additional information for regarding a MP3 file.
#[allow(dead_code)]
struct XingInfoTag {
    num_frames: Option<u32>,
    num_bytes: Option<u32>,
    toc: Option<[u8; 100]>,
    quality: Option<u32>,
    is_cbr: bool,
    lame: Option<LameTag>,
}

/// Try to read a Xing/Info tag from the provided MPEG frame.
fn try_read_info_tag(buf: &[u8], header: &FrameHeader) -> Option<XingInfoTag> {
    // The Info header is a completely optional piece of information. Therefore, flatten an error
    // reading the tag into a None.
    try_read_info_tag_inner(buf, header).ok().flatten()
}

fn try_read_info_tag_inner(buf: &[u8], header: &FrameHeader) -> Result<Option<XingInfoTag>> {
    // Do a quick check that this is a Xing/Info tag.
    if !is_maybe_info_tag(buf, header) {
        return Ok(None);
    }

    // The position of the Xing/Info tag relative to the end of the header. This is equal to the
    // side information length for the frame. The CRC is not included in this offset calculation.
    let offset = MPEG_HEADER_LEN + header.side_info_len();

    // Start the CRC with the header and side information.
    let mut crc16 = Crc16AnsiLe::new(0);
    crc16.process_buf_bytes(&buf[..offset]);

    // Start reading the Xing/Info tag after the side information.
    let mut reader = MonitorStream::new(BufReader::new(&buf[offset..]), crc16);

    // Check for Xing/Info header.
    let id = reader.read_quad_bytes()?;

    if id != XING_TAG_ID && id != INFO_TAG_ID {
        return Ok(None);
    }

    // The "Info" id is used for CBR files.
    let is_cbr = id == INFO_TAG_ID;

    // Flags indicates what information is provided in this Xing/Info tag.
    let flags = reader.read_be_u32()?;

    let num_frames = if flags & 0x1 != 0 { Some(reader.read_be_u32()?) } else { None };

    let num_bytes = if flags & 0x2 != 0 { Some(reader.read_be_u32()?) } else { None };

    let toc = if flags & 0x4 != 0 {
        let mut toc = [0; 100];
        reader.read_buf_exact(&mut toc)?;
        Some(toc)
    }
    else {
        None
    };

    let quality = if flags & 0x8 != 0 { Some(reader.read_be_u32()?) } else { None };

    /// The full LAME extension size.
    const LAME_EXT_LEN: u64 = 36;
    /// The minimal LAME extension size up-to the encode delay & padding fields.
    const MIN_LAME_EXT_LEN: u64 = 24;

    // The LAME extension may not always be present, or complete. The important fields in the
    // extension are within the first 24 bytes. Therefore, try to read those if they're available.
    let lame = if reader.inner().bytes_available() >= MIN_LAME_EXT_LEN {
        // Encoder string.
        let mut encoder = [0; 9];
        reader.read_buf_exact(&mut encoder)?;

        // Revision.
        let _revision = reader.read_u8()?;

        // Lowpass filter value.
        let _lowpass = reader.read_u8()?;

        // Replay gain peak in 9.23 (bit) fixed-point format.
        let replaygain_peak = match reader.read_be_u32()? {
            0 => None,
            peak => Some(32767.0 * (peak as f32 / 2.0f32.powi(23))),
        };

        // Radio replay gain.
        let replaygain_radio = parse_lame_tag_replaygain(reader.read_be_u16()?, 1);

        // Audiophile replay gain.
        let replaygain_audiophile = parse_lame_tag_replaygain(reader.read_be_u16()?, 2);

        // Encoding flags & ATH type.
        let _encoding_flags = reader.read_u8()?;

        // Arbitrary bitrate.
        let _abr = reader.read_u8()?;

        let (enc_delay, enc_padding) = {
            let trim = reader.read_be_u24()?;

            if encoder[..4] == *b"LAME" || encoder[..4] == *b"Lavf" || encoder[..4] == *b"Lavc" {
                let delay = 528 + 1 + (trim >> 12);
                let padding = trim & ((1 << 12) - 1);

                (delay, padding.saturating_sub(528 + 1))
            }
            else {
                (0, 0)
            }
        };

        // If possible, attempt to read the extra fields of the extension if they weren't
        // truncated.
        let crc = if reader.inner().bytes_available() >= LAME_EXT_LEN - MIN_LAME_EXT_LEN {
            // Flags.
            let _misc = reader.read_u8()?;

            // MP3 gain.
            let _mp3_gain = reader.read_u8()?;

            // Preset and surround info.
            let _surround_info = reader.read_be_u16()?;

            // Music length.
            let _music_len = reader.read_be_u32()?;

            // Music (audio) CRC.
            let _music_crc = reader.read_be_u16()?;

            // The tag CRC. LAME always includes this CRC regardless of the protection bit, but
            // other encoders may only do so if the protection bit is set.
            if header.has_crc || encoder[..4] == *b"LAME" {
                // Read the CRC using the inner reader to not change the computed CRC.
                Some(reader.inner_mut().read_be_u16()?)
            }
            else {
                // No CRC is present.
                None
            }
        }
        else {
            // The tag is truncated. No CRC will be present.
            info!("xing tag lame extension is truncated");
            None
        };

        // If there was no CRC written, then assume the tag is correct. Otherwise, use the CRC.
        // Accept a written CRC of 0 which defacto means to ignore the CRC.
        let is_tag_ok = crc.is_none_or(|crc| crc == 0 || crc == reader.monitor().crc());

        if is_tag_ok {
            // The CRC matched or is not present.
            Some(LameTag {
                encoder: String::from_utf8_lossy(&encoder).into(),
                replaygain_peak,
                replaygain_radio,
                replaygain_audiophile,
                enc_delay,
                enc_padding,
            })
        }
        else {
            // The CRC did not match, this is probably not a LAME tag.
            warn!("xing tag lame extension crc mismatch");
            None
        }
    }
    else {
        // Frame not large enough for a LAME tag.
        info!("xing tag too small for lame extension");
        None
    };

    Ok(Some(XingInfoTag { num_frames, num_bytes, toc, quality, is_cbr, lame }))
}

fn parse_lame_tag_replaygain(value: u16, expected_name: u8) -> Option<f32> {
    // The 3 most-significant bits are the name code.
    let name = ((value & 0xe000) >> 13) as u8;

    if name == expected_name {
        let gain = (value & 0x01ff) as f32 / 10.0;
        Some(if value & 0x200 != 0 { -gain } else { gain })
    }
    else {
        None
    }
}

/// Perform a fast check to see if the packet contains a Xing/Info tag. If this returns true, the
/// packet should be parsed fully to ensure it is in fact a tag.
fn is_maybe_info_tag(buf: &[u8], header: &FrameHeader) -> bool {
    const MIN_XING_TAG_LEN: usize = 8;

    // Only supported with layer 3 packets.
    if header.layer != MpegLayer::Layer3 {
        return false;
    }

    // The position of the Xing/Info tag relative to the start of the packet. This is equal to the
    // side information length for the frame. The CRC is not included in this offset calculation.
    let offset = MPEG_HEADER_LEN + header.side_info_len();

    // The packet must be big enough to contain a tag.
    if buf.len() < offset + MIN_XING_TAG_LEN {
        return false;
    }

    // The tag ID must be present and correct.
    let id = &buf[offset..offset + 4];

    if id != XING_TAG_ID && id != INFO_TAG_ID {
        return false;
    }

    // The side information, which follows the header and optional CRC, should be zeroed.
    !buf[header.header_size()..offset].iter().any(|&b| b != 0)
}

const VBRI_TAG_ID: [u8; 4] = *b"VBRI";

/// The contents of a VBRI tag.
#[allow(dead_code)]
struct VbriTag {
    num_bytes: u32,
    num_mpeg_frames: u32,
    /// The number of MPEG frames spanned by a single seek table entry.
    frames_per_entry: u16,
    /// The scale factor applied to each raw seek table entry to obtain a byte offset.
    entry_scale: u16,
    /// The size in bytes of a single seek table entry.
    entry_size: u16,
    /// The raw seek table entries, ordered by increasing time. Each entry must be multiplied by
    /// `entry_scale` to obtain the byte offset relative to the start of the audio data.
    entries: Vec<u32>,
}

/// Try to read a VBRI tag from the provided MPEG frame.
fn try_read_vbri_tag(buf: &[u8], header: &FrameHeader) -> Option<VbriTag> {
    // The VBRI header is a completely optional piece of information. Therefore, flatten an error
    // reading the tag into a None.
    try_read_vbri_tag_inner(buf, header).ok().flatten()
}

fn try_read_vbri_tag_inner(buf: &[u8], header: &FrameHeader) -> Result<Option<VbriTag>> {
    // Do a quick check that this is a VBRI tag.
    if !is_maybe_vbri_tag(buf, header) {
        return Ok(None);
    }

    let mut reader = BufReader::new(buf);

    // The VBRI tag is always 32 bytes after the header.
    reader.ignore_bytes(MPEG_HEADER_LEN as u64 + 32)?;

    // Check for the VBRI signature.
    let id = reader.read_quad_bytes()?;

    if id != VBRI_TAG_ID {
        return Ok(None);
    }

    // The version is always 1.
    let version = reader.read_be_u16()?;

    if version != 1 {
        return Ok(None);
    }

    // Delay is a 2-byte big-endiann floating point value?
    let _delay = reader.read_be_u16()?;
    let _quality = reader.read_be_u16()?;

    let num_bytes = reader.read_be_u32()?;
    let num_mpeg_frames = reader.read_be_u32()?;

    // The seek table's metadata immediately follows the fixed portion of the tag.
    let num_entries = u64::from(reader.read_be_u16()?);
    let entry_scale = reader.read_be_u16()?;
    let entry_size = reader.read_be_u16()?;
    let frames_per_entry = reader.read_be_u16()?;

    // Validate the seek table's metadata before attempting to parse it. Each entry is a big-endian
    // integer of `entry_size` bytes, though only 1, 2, or 4-byte entries are defined.
    if frames_per_entry == 0
        || num_entries == 0
        || entry_scale == 0
        || !matches!(entry_size, 1 | 2 | 4)
    {
        // The fixed portion of the tag is still valid, so return it without a seek table.
        warn!("vbri tag has an invalid seek table, the table will not be used");

        return Ok(Some(VbriTag {
            num_bytes,
            num_mpeg_frames,
            frames_per_entry,
            entry_scale,
            entry_size,
            entries: Vec::new(),
        }));
    }

    // The seek table must fit within the provided frame.
    let table_len = num_entries.saturating_mul(u64::from(entry_size));

    if reader.bytes_available() < table_len {
        warn!("vbri seek table is truncated, the table will not be used");

        return Ok(Some(VbriTag {
            num_bytes,
            num_mpeg_frames,
            frames_per_entry,
            entry_scale,
            entry_size,
            entries: Vec::new(),
        }));
    }

    let mut entries = Vec::with_capacity(num_entries as usize);

    for _ in 0..num_entries {
        let entry = match entry_size {
            1 => u32::from(reader.read_u8()?),
            2 => u32::from(reader.read_be_u16()?),
            4 => reader.read_be_u32()?,
            _ => unreachable!(),
        };

        entries.push(entry);
    }

    Ok(Some(VbriTag {
        num_bytes,
        num_mpeg_frames,
        frames_per_entry,
        entry_scale,
        entry_size,
        entries,
    }))
}

/// Perform a fast check to see if the packet contains a VBRI tag. If this returns true, the
/// packet should be parsed fully to ensure it is in fact a tag.
fn is_maybe_vbri_tag(buf: &[u8], header: &FrameHeader) -> bool {
    const MIN_VBRI_TAG_LEN: usize = 26;
    const VBRI_TAG_OFFSET: usize = MPEG_HEADER_LEN + 32;

    // Only supported with layer 3 packets.
    if header.layer != MpegLayer::Layer3 {
        return false;
    }

    // The packet must be big enough to contain a tag.
    if buf.len() < VBRI_TAG_OFFSET + MIN_VBRI_TAG_LEN {
        return false;
    }

    // The tag ID must be present and correct.
    let id = &buf[VBRI_TAG_OFFSET..VBRI_TAG_OFFSET + 4];

    if id != VBRI_TAG_ID {
        return false;
    }

    // The bytes preceeding the VBRI tag (mostly the side information) should be all 0.
    !buf[header.header_size()..VBRI_TAG_OFFSET].iter().any(|&b| b != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::{ChannelMode, Emphasis, MpegVersion};

    #[test]
    fn test_is_maybe_info_tag_with_crc() {
        let header = FrameHeader {
            version: MpegVersion::Mpeg1,
            layer: MpegLayer::Layer3,
            bitrate: 128000,
            sample_rate: 44100,
            sample_rate_idx: 0,
            channel_mode: ChannelMode::Stereo,
            emphasis: Emphasis::None,
            is_copyrighted: false,
            is_original: false,
            has_padding: false,
            has_crc: true,
            frame_size: 417,
        };

        let mut buf = vec![0u8; 100];
        // Header
        buf[0..4].copy_from_slice(&[0xff, 0xfa, 0x90, 0x44]);
        // CRC (non-zero)
        buf[4] = 0x12;
        buf[5] = 0x34;
        // Tag ID at offset 32 + 4 = 36
        buf[36..40].copy_from_slice(b"Xing");

        // Ensure the heuristic returns true even if a non-zero CRC is present.
        assert!(is_maybe_info_tag(&buf, &header));
    }

    #[test]
    fn test_is_maybe_vbri_tag_with_crc() {
        let header = FrameHeader {
            version: MpegVersion::Mpeg1,
            layer: MpegLayer::Layer3,
            bitrate: 128000,
            sample_rate: 44100,
            sample_rate_idx: 0,
            channel_mode: ChannelMode::Stereo,
            emphasis: Emphasis::None,
            is_copyrighted: false,
            is_original: false,
            has_padding: false,
            has_crc: true,
            frame_size: 417,
        };

        let mut buf = vec![0u8; 100];
        // Header
        buf[0..4].copy_from_slice(&[0xff, 0xfa, 0x90, 0x44]);
        // CRC (non-zero)
        buf[4] = 0x12;
        buf[5] = 0x34;
        // Tag ID at offset 36
        buf[36..40].copy_from_slice(b"VBRI");

        // Ensure the heuristic returns true even if a non-zero CRC is present.
        assert!(is_maybe_vbri_tag(&buf, &header));
    }

    // =========================================================================================
    // Helpers for building synthetic MPEG audio streams.
    // =========================================================================================

    /// The number of samples per MPEG frame for the synthetic test streams (MPEG1 Layer3).
    const SAMPLES_PER_FRAME: u64 = 1152;

    /// Build a MPEG1 Layer3, stereo, 44.1kHz frame header word for the given bitrate.
    fn mpeg1_l3_header_word(bitrate: u32) -> u32 {
        // The bitrate index for MPEG1 Layer3 is looked up in BIT_RATES_MPEG1_L3.
        let bitrate_idx: u32 = match bitrate {
            32_000 => 1,
            40_000 => 2,
            48_000 => 3,
            56_000 => 4,
            64_000 => 5,
            80_000 => 6,
            96_000 => 7,
            112_000 => 8,
            128_000 => 9,
            160_000 => 10,
            192_000 => 11,
            224_000 => 12,
            256_000 => 13,
            320_000 => 14,
            _ => panic!("unsupported test bitrate {bitrate}"),
        };

        // 0xfffb: 11 sync bits, MPEG1 (0b11), Layer3 (0b01), no CRC (protection bit 1).
        // 0x90: bitrate index in the high nibble (sample-rate index 0 -> 44.1kHz, no padding).
        // 0x00: stereo joint-stereo extension bits zeroed, no copyright/original/emphasis.
        0xfffb_0000 | (bitrate_idx << 12) | 0x0090
    }

    /// The size in bytes (excluding the 4-byte header) of a MPEG1 Layer3, 44.1kHz frame.
    fn mpeg1_l3_frame_size(bitrate: u32) -> usize {
        // 144 * bitrate / sample_rate - 4, with no padding slot.
        (144 * bitrate / 44_100) as usize - 4
    }

    /// Build a single MPEG frame as a byte vector. The frame body is zeroed so it is never mistaken
    /// for another frame sync word.
    fn build_frame(bitrate: u32) -> Vec<u8> {
        let header = mpeg1_l3_header_word(bitrate);
        let frame_size = mpeg1_l3_frame_size(bitrate);

        let mut frame = vec![0u8; MPEG_HEADER_LEN + frame_size];
        frame[0..MPEG_HEADER_LEN].copy_from_slice(&header.to_be_bytes());
        frame
    }

    /// Insert a Xing tag with the provided 100-entry table of contents into the first frame.
    fn insert_xing_tag(frame: &mut [u8], num_mpeg_frames: u32, toc: [u8; 100]) {
        let header =
            header::parse_frame_header(u32::from_be_bytes(frame[0..4].try_into().unwrap()))
                .unwrap();

        // The tag begins right after the side information. For MPEG1 stereo this is 36 bytes.
        let offset = MPEG_HEADER_LEN + header.side_info_len();

        frame[offset..offset + 4].copy_from_slice(b"Xing");
        // Flags: num_frames (0x1) | num_bytes (0x2) | toc (0x4).
        frame[offset + 4..offset + 8].copy_from_slice(&7u32.to_be_bytes());
        frame[offset + 8..offset + 12].copy_from_slice(&num_mpeg_frames.to_be_bytes());

        // The total number of bytes in the audio data; filled in by the caller's stream builder.
        // Use a placeholder of 0 here and overwrite once the full stream is assembled.
        frame[offset + 12..offset + 16].copy_from_slice(&0u32.to_be_bytes());
        frame[offset + 16..offset + 116].copy_from_slice(&toc);
    }

    /// Overwrite the num_bytes field of a Xing tag.
    fn set_xing_num_bytes(stream: &mut [u8], first_frame_len: usize, num_bytes: u32) {
        let header =
            header::parse_frame_header(u32::from_be_bytes(stream[0..4].try_into().unwrap()))
                .unwrap();
        let offset = MPEG_HEADER_LEN + header.side_info_len();
        stream[offset + 12..offset + 16].copy_from_slice(&num_bytes.to_be_bytes());
        let _ = first_frame_len;
    }

    /// Insert a VBRI tag with a seek table into the first frame. `entries` are the raw table
    /// entries; each entry occupies `entry_size` bytes and is scaled by `entry_scale` at parse.
    fn insert_vbri_tag(
        frame: &mut [u8],
        num_mpeg_frames: u32,
        num_entries: u16,
        entry_scale: u16,
        entry_size: u16,
        frames_per_entry: u16,
        entries: &[u32],
    ) {
        // The fixed VBRI tag starts 32 bytes after the header.
        let base = MPEG_HEADER_LEN + 32;

        frame[base..base + 4].copy_from_slice(b"VBRI");
        frame[base + 4..base + 6].copy_from_slice(&1u16.to_be_bytes()); // version
        frame[base + 6..base + 8].copy_from_slice(&0u16.to_be_bytes()); // delay
        frame[base + 8..base + 10].copy_from_slice(&0u16.to_be_bytes()); // quality
        frame[base + 10..base + 14].copy_from_slice(&0u32.to_be_bytes()); // num bytes
        frame[base + 14..base + 18].copy_from_slice(&num_mpeg_frames.to_be_bytes());
        frame[base + 18..base + 20].copy_from_slice(&num_entries.to_be_bytes());
        frame[base + 20..base + 22].copy_from_slice(&entry_scale.to_be_bytes());
        frame[base + 22..base + 24].copy_from_slice(&entry_size.to_be_bytes());
        frame[base + 24..base + 26].copy_from_slice(&frames_per_entry.to_be_bytes());

        let table_base = base + 26;

        for (idx, &entry) in entries.iter().enumerate() {
            let off = table_base + idx * entry_size as usize;
            match entry_size {
                1 => frame[off] = entry as u8,
                2 => frame[off..off + 2].copy_from_slice(&(entry as u16).to_be_bytes()),
                4 => frame[off..off + 4].copy_from_slice(&entry.to_be_bytes()),
                _ => panic!("unsupported entry size"),
            }
        }
    }

    /// Build a synthetic VBR stream of `num_frames` frames. The first frame is encoded at
    /// `first_bitrate` so it is large enough to hold a Xing/Info or VBRI tag. Subsequent frames
    /// alternate between `low_bitrate` and `high_bitrate` so the linear byte/time relationship does
    /// not hold.
    fn build_vbr_stream(
        num_frames: u32,
        first_bitrate: u32,
        low_bitrate: u32,
        high_bitrate: u32,
        make_tag: impl FnOnce(&mut [u8], usize),
    ) -> Vec<u8> {
        let mut stream = Vec::new();

        // Build the first frame separately so the tag closure, which is consumed once, is applied
        // exactly once.
        let mut first_frame = build_frame(first_bitrate);
        let first_frame_len = first_frame.len();
        make_tag(&mut first_frame, first_frame_len);
        stream.extend_from_slice(&first_frame);

        for idx in 1..num_frames {
            let bitrate = if idx % 2 == 0 { low_bitrate } else { high_bitrate };
            let frame = build_frame(bitrate);
            stream.extend_from_slice(&frame);
        }

        stream
    }

    /// Open a synthetic stream as a `MpaReader`.
    fn open_reader(stream: Vec<u8>) -> MpaReader<'static> {
        let mss =
            MediaSourceStream::new(Box::new(std::io::Cursor::new(stream)), Default::default());
        MpaReader::try_new(mss, FormatOptions::default()).expect("reader should open")
    }

    // =========================================================================================
    // SeekIndex unit tests.
    // =========================================================================================

    fn test_header() -> FrameHeader {
        header::parse_frame_header(mpeg1_l3_header_word(128_000)).unwrap()
    }

    #[test]
    fn test_xing_seek_index_rejects_invalid_tables() {
        // A zero total duration is rejected.
        assert!(SeekIndex::from_xing_toc([0; 100], 0, 10_000).is_none());

        // A zero audio length is rejected.
        assert!(SeekIndex::from_xing_toc([0; 100], 1152, 0).is_none());

        // A non-zero first entry is rejected.
        let mut toc = [0u8; 100];
        toc[0] = 1;
        assert!(SeekIndex::from_xing_toc(toc, 1152, 10_000).is_none());

        // A table that decreases is rejected.
        let mut toc = [0u8; 100];
        toc[1] = 10;
        toc[2] = 5;
        assert!(SeekIndex::from_xing_toc(toc, 1152, 10_000).is_none());

        // An all-zero table is rejected.
        assert!(SeekIndex::from_xing_toc([0u8; 100], 1152, 10_000).is_none());
    }

    #[test]
    fn test_xing_seek_index_anchors() {
        // A linearly increasing TOC where entry i maps to i/100 of the stream.
        let mut toc = [0u8; 100];
        for (i, entry) in toc.iter_mut().enumerate() {
            *entry = ((i as u64 * 255) / 99).min(255) as u8;
        }

        let total_ts = 100 * SAMPLES_PER_FRAME;
        let audio_byte_len = 10_000u64;

        let index = SeekIndex::from_xing_toc(toc, total_ts, audio_byte_len).expect("valid toc");

        // A request at the start resolves to the first anchor (the start of the audio data).
        let first = index.seek_anchor(0);
        assert_eq!(first.ts_offset, 0);
        assert_eq!(first.byte_offset, 0);

        // A request at, or beyond, the end resolves to an anchor at, or before, the end rather
        // than extrapolating past the table. Note the selection backtracks a couple of anchors to
        // leave reference frames, so it need not be the final anchor.
        let end = index.seek_anchor(total_ts);
        let beyond = index.seek_anchor(total_ts + 5_000);
        assert_eq!(end.ts_offset, beyond.ts_offset);
        assert_eq!(end.byte_offset, beyond.byte_offset);
        assert!(end.ts_offset <= total_ts);
        assert!(end.ts_offset > 0, "a backtracked end anchor should still be well into the stream");

        // The anchor's byte offset matches the corresponding table entry's fraction.
        let anchor_entry = (end.ts_offset * 100 / total_ts) as usize;
        let expected = u64::from(toc[anchor_entry]) * audio_byte_len / 256;
        assert_eq!(end.byte_offset, expected);
    }

    #[test]
    fn test_xing_seek_index_anchor_is_at_or_before_request() {
        // Build a TOC that maps perfectly linearly: entry i stores 256 * i / 100 (rounded down).
        let mut toc = [0u8; 100];
        for (i, entry) in toc.iter_mut().enumerate() {
            *entry = ((i as u64 * 256) / 100).min(255) as u8;
        }

        let total_ts = 100 * SAMPLES_PER_FRAME;
        let audio_byte_len = 10_000u64;

        let index = SeekIndex::from_xing_toc(toc, total_ts, audio_byte_len).expect("valid toc");

        // Every selected anchor must lie at, or before, the requested timestamp.
        for request in (0..=total_ts).step_by(SAMPLES_PER_FRAME as usize) {
            let anchor = index.seek_anchor(request);
            assert!(anchor.ts_offset <= request, "anchor past the request");
        }

        // The anchor selected near the midpoint must be close to the midpoint in bytes. The
        // selection backtracks a couple of anchors (each spanning 1% of the stream), so allow for
        // that bounded backtrack plus integer truncation of the table entries.
        let anchor = index.seek_anchor(total_ts / 2);
        let half = audio_byte_len / 2;
        let tolerance = audio_byte_len / 25 + 1;
        assert!(
            anchor.byte_offset.abs_diff(half) <= tolerance,
            "byte_offset={}, half={half}",
            anchor.byte_offset
        );
    }

    #[test]
    fn test_vbri_seek_index_applies_scale_and_bounds() {
        let header = test_header();

        // Two raw entries of 100 and 200 with a scale of 2 -> offsets 200 and 400.
        let raw = [100u32, 200];
        let total_ts = 4 * SAMPLES_PER_FRAME;

        let index =
            SeekIndex::from_vbri_entries(&raw, 2, 1, total_ts, &header).expect("valid vbri table");

        // Entry 0 starts at the beginning of the audio, with a scaled byte offset of 200.
        let first = index.seek_anchor(0);
        assert_eq!(first.ts_offset, 0);
        assert_eq!(first.byte_offset, 200);

        // A request at entry 1 backtracks to entry 0 so reference frames remain available.
        let backtracked = index.seek_anchor(SAMPLES_PER_FRAME);
        assert_eq!(backtracked.ts_offset, 0);
        assert_eq!(backtracked.byte_offset, 200);

        // With only two anchors, any request backtracks all the way to the first anchor, which is
        // the earliest valid seek point.
        let second_request = 4 * SAMPLES_PER_FRAME;
        let second = index.seek_anchor(second_request);
        assert_eq!(second.ts_offset, 0);
        assert_eq!(second.byte_offset, 200);

        // A request past the last entry does not extrapolate: it resolves to a valid anchor within
        // the table (here the first anchor, since the two-anchor table always backtracks to it).
        let clamped = index.seek_anchor(total_ts);
        assert_eq!(clamped.byte_offset, 200);
        assert!(clamped.ts_offset <= total_ts);

        // The same anchor is returned for a request far beyond the end of the table.
        let beyond = index.seek_anchor(total_ts + 10_000);
        assert_eq!(beyond.byte_offset, clamped.byte_offset);

        // A scale of zero is rejected.
        assert!(SeekIndex::from_vbri_entries(&raw, 0, 1, total_ts, &header).is_none());

        // A zero first entry is valid because it points to the start of the audio data.
        assert!(SeekIndex::from_vbri_entries(&[0u32, 200], 2, 1, total_ts, &header).is_some());

        // A non-monotonic table is rejected.
        assert!(SeekIndex::from_vbri_entries(&[200u32, 100], 1, 1, total_ts, &header).is_none());
    }

    #[test]
    fn test_vbri_seek_index_anchor_backtracks() {
        let header = test_header();

        // Eight entries, one frame apart, with strictly increasing byte offsets.
        let raw: Vec<u32> = (1..=8).map(|i| i * 100).collect();
        let total_ts = 8 * SAMPLES_PER_FRAME;

        let index =
            SeekIndex::from_vbri_entries(&raw, 1, 1, total_ts, &header).expect("valid vbri table");

        // Requesting the last entry must backtrack to leave reference frames before the target, so
        // the selected anchor is never the final anchor itself.
        let anchor = index.seek_anchor(total_ts);
        assert!(anchor.ts_offset < total_ts);

        // Backtracking must never move before the first anchor.
        let first = index.seek_anchor(0);
        assert_eq!(first.ts_offset, 0);

        // An anchor is always at, or before, the request.
        for request in (0..=total_ts).step_by(SAMPLES_PER_FRAME as usize) {
            assert!(index.seek_anchor(request).ts_offset <= request);
        }
    }

    // =========================================================================================
    // End-to-end coarse seek tests on synthetic VBR streams.
    // =========================================================================================

    /// Compute the byte size of each frame of the synthetic VBR streams used by the coarse seek
    /// tests. The first frame uses `first_bitrate`; later frames alternate between `low_bitrate` and
    /// `high_bitrate`.
    fn stream_frame_sizes(
        num_frames: u32,
        first_bitrate: u32,
        low_bitrate: u32,
        high_bitrate: u32,
    ) -> Vec<u64> {
        (0..num_frames)
            .map(|idx| {
                let bitrate = if idx == 0 {
                    first_bitrate
                }
                else if idx % 2 == 0 {
                    low_bitrate
                }
                else {
                    high_bitrate
                };
                build_frame(bitrate).len() as u64
            })
            .collect()
    }

    /// Build a Xing table of contents that exactly describes a stream with the given frame sizes
    /// and uniform per-frame duration.
    fn build_xing_toc(frame_sizes: &[u64]) -> [u8; 100] {
        let total_bytes: u64 = frame_sizes.iter().sum();
        let mut offsets = Vec::with_capacity(frame_sizes.len());
        let mut acc = 0u64;
        for &size in frame_sizes {
            offsets.push(acc);
            acc += size;
        }

        let num_frames = frame_sizes.len() as u64;
        let mut toc = [0u8; 100];

        for (i, entry) in toc.iter_mut().enumerate() {
            // The frame that contains this point in time.
            let frame_idx = (i as u64 * num_frames / 100).min(num_frames - 1);
            let byte_pos = offsets[frame_idx as usize];
            *entry = (256 * byte_pos / total_bytes).min(255) as u8;
        }

        toc
    }

    #[test]
    fn test_coarse_seek_uses_xing_toc() {
        const NUM_FRAMES: u32 = 24;
        const FIRST: u32 = 320_000;
        const LOW: u32 = 32_000;
        const HIGH: u32 = 160_000;

        let frame_sizes = stream_frame_sizes(NUM_FRAMES, FIRST, LOW, HIGH);
        let total_audio_bytes: u64 = frame_sizes.iter().sum();

        // A TOC that describes the synthetic stream's actual frame layout.
        let toc = build_xing_toc(&frame_sizes);

        let mut stream = build_vbr_stream(NUM_FRAMES, FIRST, LOW, HIGH, |frame, _| {
            insert_xing_tag(frame, NUM_FRAMES, toc);
        });
        set_xing_num_bytes(&mut stream, frame_sizes[0] as usize, total_audio_bytes as u32);

        let mut reader = open_reader(stream);

        // The reader must have reconstructed a seek index.
        assert!(reader.seek_index.is_some(), "a xing seek index should be present");

        // Seek to the middle of the stream using coarse mode.
        let target_ts = (NUM_FRAMES as u64 / 2) * SAMPLES_PER_FRAME;

        let seeked = reader
            .seek(
                SeekMode::Coarse,
                SeekTo::Timestamp { ts: Timestamp::new(target_ts as i64), track_id: 0 },
            )
            .expect("coarse seek should succeed");

        // The actual timestamp must be at or before the requested timestamp.
        assert!(
            seeked.actual_ts.get() <= target_ts as i64,
            "actual_ts={} should be <= target={target_ts}",
            seeked.actual_ts
        );

        // Since the TOC accurately describes the stream, the seek must land within a few frames of
        // the target rather than being thrown off by the large bitrate differences between frames.
        let error = target_ts.abs_diff(seeked.actual_ts.get() as u64);
        assert!(error <= 3 * SAMPLES_PER_FRAME, "seek error too large: {error}");

        // The packet following the seek must be decodable: it must begin with a valid sync word.
        let packet = reader.next_packet().expect("a packet should be available").unwrap();
        assert!(header::is_frame_header_word_synced(u32::from_be_bytes(
            packet.data[0..4].try_into().unwrap()
        )));
    }

    #[test]
    fn test_coarse_seek_uses_vbri_table() {
        const NUM_FRAMES: u32 = 16;
        const FIRST: u32 = 320_000;
        const LOW: u32 = 32_000;
        const HIGH: u32 = 160_000;
        const FRAMES_PER_ENTRY: u16 = 2;

        let frame_sizes = stream_frame_sizes(NUM_FRAMES, FIRST, LOW, HIGH);

        // One entry per two frames: the byte offset at the start of every second frame. Offsets are
        // relative to the start of the audio data, and entry 0 therefore starts at zero.
        let num_entries = NUM_FRAMES / u32::from(FRAMES_PER_ENTRY);
        let mut entries = Vec::with_capacity(num_entries as usize);
        let mut acc: u32 = 0;
        for i in 0..num_entries {
            if i > 0 {
                let base = (i * 2) as usize;
                acc += (frame_sizes[base - 2] + frame_sizes[base - 1]) as u32;
            }
            entries.push(acc);
        }

        let stream = build_vbr_stream(NUM_FRAMES, FIRST, LOW, HIGH, |frame, _| {
            insert_vbri_tag(
                frame,
                NUM_FRAMES,
                num_entries as u16,
                1,
                4,
                FRAMES_PER_ENTRY,
                &entries,
            );
        });

        let mut reader = open_reader(stream);

        assert!(reader.seek_index.is_some(), "a vbri seek index should be present");

        // Seek to roughly the three-quarter point.
        let target_ts = (NUM_FRAMES as u64 * 3 / 4) * SAMPLES_PER_FRAME;

        let seeked = reader
            .seek(
                SeekMode::Coarse,
                SeekTo::Timestamp { ts: Timestamp::new(target_ts as i64), track_id: 0 },
            )
            .expect("coarse seek should succeed");

        assert!(seeked.actual_ts.get() <= target_ts as i64);

        let error = target_ts.abs_diff(seeked.actual_ts.get() as u64);
        // The table groups two frames per entry, so allow a few frames of residual error.
        assert!(error <= 4 * SAMPLES_PER_FRAME, "seek error too large: {error}");

        // The next packet must be a valid frame.
        let packet = reader.next_packet().expect("a packet should be available").unwrap();
        assert!(header::is_frame_header_word_synced(u32::from_be_bytes(
            packet.data[0..4].try_into().unwrap()
        )));
    }

    #[test]
    fn test_coarse_seek_falls_back_without_index() {
        const NUM_FRAMES: u32 = 40;
        const FIRST: u32 = 320_000;
        const LOW: u32 = 64_000;
        const HIGH: u32 = 128_000;

        // A plain VBR stream with no Xing/Info or VBRI tag in the first frame.
        let stream = build_vbr_stream(NUM_FRAMES, FIRST, LOW, HIGH, |_, _| {});

        let mut reader = open_reader(stream);

        // No seek index is available, so the linear estimation path must be used.
        assert!(reader.seek_index.is_none());

        // The duration must be estimated since no tag is present.
        assert!(reader.tracks()[0].num_frames.is_some(), "duration should be estimated");

        let target_ts = (NUM_FRAMES as u64 / 2) * SAMPLES_PER_FRAME;

        let seeked = reader
            .seek(
                SeekMode::Coarse,
                SeekTo::Timestamp { ts: Timestamp::new(target_ts as i64), track_id: 0 },
            )
            .expect("coarse seek should succeed using the fallback");

        // The fallback may overshoot in coarse mode, but the reader must still produce packets
        // whose timestamps progress forward from the reported position.
        let first = reader.next_packet().expect("a packet should be available").unwrap();
        assert_eq!(first.pts, seeked.actual_ts);

        let second = reader.next_packet().expect("a second packet should be available").unwrap();
        assert!(second.pts > first.pts);
    }

    #[test]
    fn test_seek_stays_within_valid_audio_bounds() {
        const NUM_FRAMES: u32 = 16;
        const FIRST: u32 = 320_000;
        const LOW: u32 = 32_000;
        const HIGH: u32 = 160_000;

        let frame_sizes = stream_frame_sizes(NUM_FRAMES, FIRST, LOW, HIGH);
        let toc = build_xing_toc(&frame_sizes);

        let stream = build_vbr_stream(NUM_FRAMES, FIRST, LOW, HIGH, |frame, _| {
            insert_xing_tag(frame, NUM_FRAMES, toc);
        });

        let mut reader = open_reader(stream);
        let max_ts = reader.tracks()[0]
            .num_frames
            .map(|n| Timestamp::new(n as i64))
            .expect("duration should be known");

        // Seek near the end of the valid audio range; it must succeed and never run past the end.
        let near_end = max_ts.saturating_sub(Duration::from(2 * SAMPLES_PER_FRAME));

        let seeked = reader
            .seek(SeekMode::Coarse, SeekTo::Timestamp { ts: near_end, track_id: 0 })
            .expect("seek near the end should succeed");

        assert!(seeked.actual_ts <= max_ts, "seeked past the valid audio range");

        // Seeking beyond the valid range is rejected.
        let result = reader.seek(
            SeekMode::Coarse,
            SeekTo::Timestamp {
                ts: max_ts.checked_add(Duration::from(SAMPLES_PER_FRAME)).unwrap(),
                track_id: 0,
            },
        );
        assert!(matches!(result, Err(Error::SeekError(SeekErrorKind::OutOfRange))));
    }

    #[test]
    fn test_accurate_seek_unaffected_by_index() {
        const NUM_FRAMES: u32 = 12;
        const FIRST: u32 = 320_000;
        const LOW: u32 = 32_000;
        const HIGH: u32 = 160_000;

        let frame_sizes = stream_frame_sizes(NUM_FRAMES, FIRST, LOW, HIGH);
        let toc = build_xing_toc(&frame_sizes);

        let stream = build_vbr_stream(NUM_FRAMES, FIRST, LOW, HIGH, |frame, _| {
            insert_xing_tag(frame, NUM_FRAMES, toc);
        });

        let mut reader = open_reader(stream);

        // Accurate seeks must ignore the coarse seek index and count frames from the start, so the
        // reported position is exact.
        let target_frame = NUM_FRAMES / 2;
        let target_ts = u64::from(target_frame) * SAMPLES_PER_FRAME;

        let seeked = reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp { ts: Timestamp::new(target_ts as i64), track_id: 0 },
            )
            .expect("accurate seek should succeed");

        assert_eq!(seeked.actual_ts.get(), target_ts as i64);
    }
}
