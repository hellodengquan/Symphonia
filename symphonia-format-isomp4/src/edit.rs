// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Gapless playback support for ISO Base Media File Format audio tracks.
//!
//! Two sources of gapless information are supported, in order of preference:
//!
//! 1. The track's edit list (`edts/elst`). A single, normal-rate edit defines the valid audio
//!    region of the track. The portion of the media timeline before the edit is the encoder
//!    delay, and the portion after the edit is the encoder padding.
//! 2. The iTunes gapless information (`com.apple.iTunes:iTunSMPB` freeform metadata tag), used
//!    only when the track has no edit list.
//!
//! All values produced by this module are expressed in the media (`mdhd`) timescale, which for
//! almost all audio tracks is equal to the audio sample rate, so a single time unit corresponds
//! to a single decoded audio frame.

use symphonia_core::codecs::CodecParameters;

use crate::atoms::TrakAtom;

use log::debug;

/// The gapless playback information for a single track.
///
/// All fields are expressed in the media (`mdhd`) timescale of the track.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct GaplessInfo {
    /// The media time of the first frame that should be presented to the user. Frames before
    /// this time are encoder delay frames.
    pub start_media_time: u64,
    /// The number of frames that should be presented to the user. Frames beyond
    /// `start_media_time + valid_duration` are encoder padding frames.
    pub valid_duration: u64,
}

impl GaplessInfo {
    /// The number of leading (encoder delay) frames.
    pub fn delay(&self) -> u64 {
        self.start_media_time
    }

    /// The number of trailing (encoder padding) frames given the total number of media frames
    /// stored in the track.
    pub fn padding(&self, total_media_duration: u64) -> u64 {
        let end = self.start_media_time.saturating_add(self.valid_duration);
        total_media_duration.saturating_sub(end)
    }
}

/// Resolve the gapless playback information for a non-fragmented audio track.
///
/// The edit list is always the preferred source. If the edit list is absent, or does not contain
/// a single normal-rate edit of the media, the iTunes `iTunSMPB` gapless metadata is used as a
/// fallback. If neither source provides usable gapless information, `None` is returned and the
/// caller preserves the original behaviour.
///
/// `movie_timescale` is the timescale of the movie header (`mvhd`) atom in which edit list
/// segment durations are expressed. `itunes_gapless` is the raw value of the iTunes
/// `iTunSMPB` freeform tag, if present in the movie's user-data metadata.
pub fn resolve_gapless_info(
    trak: &TrakAtom,
    itunes_gapless: Option<&[u8]>,
    movie_timescale: u32,
) -> Option<GaplessInfo> {
    if let Some(info) = resolve_from_edit_list(trak, movie_timescale) {
        debug!("isomp4: using edit list for gapless playback: {info:?}");
        return Some(info);
    }

    if let Some(info) = resolve_from_itunes(trak, itunes_gapless) {
        debug!("isomp4: using iTunes gapless information: {info:?}");
        return Some(info);
    }

    None
}

/// Convert a duration expressed in one timescale into the equivalent duration in another
/// timescale, rounding up so that the valid audio region is never shortened by truncation.
fn scale_duration(duration: u64, src_timescale: u32, dst_timescale: u32) -> u64 {
    if src_timescale == dst_timescale {
        return duration;
    }

    let scaled = u128::from(duration)
        .saturating_mul(u128::from(dst_timescale))
        .saturating_add(u128::from(src_timescale) - 1)
        / u128::from(src_timescale);

    u64::try_from(scaled).unwrap_or(u64::MAX)
}

/// Parse a single, normal-rate edit of the media from the track's edit list.
///
/// Edit lists with empty (silence) edits, multiple edits of the media, or edits that do not play
/// the media forward at normal rate are not supported and cause `None` to be returned.
fn resolve_from_edit_list(trak: &TrakAtom, movie_timescale: u32) -> Option<GaplessInfo> {
    let elst = trak.edts.as_ref()?.elst.as_ref()?;

    // A single edit entry is the only supported edit layout. Everything else preserves the
    // existing (non-gapless) behaviour.
    let entry = match elst.entries.as_slice() {
        [entry] => entry,
        _ => {
            debug!("isomp4: edit lists with multiple entries are not supported for gapless");
            return None;
        }
    };

    // An empty edit (media_time == -1) only introduces silence and does not describe a region of
    // the media.
    if entry.media_time < 0 {
        debug!("isomp4: empty edit list entries are not supported for gapless");
        return None;
    }

    // Only edits playing the media forward at normal rate (rate 1.0) can be mapped to a simple
    // delay/padding trim.
    if entry.media_rate_int != 1 || entry.media_rate_frac != 0 {
        debug!("isomp4: non-normal-rate edits are not supported for gapless");
        return None;
    }

    if entry.segment_duration == 0 {
        debug!("isomp4: ignoring zero-duration edit list entry");
        return None;
    }

    let media_timescale = trak.mdia.mdhd.timescale.get();

    let start_media_time = u64::try_from(entry.media_time).ok()?;
    let edit_duration = scale_duration(entry.segment_duration, movie_timescale, media_timescale);

    let total_media_duration = trak.mdia.minf.stbl.stts.total_duration;

    // The edit must reference a region of the actual media. Clamp both bounds so that a
    // contradictory edit list cannot produce an out-of-bounds or inverted region.
    if start_media_time >= total_media_duration {
        debug!("isomp4: edit list starts past the end of the media, ignoring");
        return None;
    }

    let max_valid_duration = total_media_duration - start_media_time;
    let valid_duration = edit_duration.min(max_valid_duration);

    if valid_duration == 0 {
        return None;
    }

    Some(GaplessInfo { start_media_time, valid_duration })
}

/// Parse the iTunes `iTunSMPB` gapless tag from the movie's user-data metadata.
///
/// The tag value is an ASCII string of hexadecimal fields separated by spaces:
/// `priming leading padding remaining ...`. The first field (`priming`) is always 0, while
/// `leading` and `padding` are the encoder delay and padding in complete audio frames.
fn resolve_from_itunes(trak: &TrakAtom, smpb: Option<&[u8]>) -> Option<GaplessInfo> {
    if !is_audio_track(trak) {
        return None;
    }

    let smpb = smpb?;

    let text = std::str::from_utf8(smpb).ok()?.trim();

    let mut fields = text.split(' ').filter(|field| !field.is_empty());

    // Field 0: priming flags/version (always 0 for valid tags).
    let _priming = fields.next()?;
    // Field 1: encoder delay in audio frames.
    let leading = u64::from_str_radix(fields.next()?, 16).ok()?;
    // Field 2: encoder padding in audio frames.
    let padding = u64::from_str_radix(fields.next()?, 16).ok()?;

    if leading == 0 && padding == 0 {
        return None;
    }

    let total_media_duration = trak.mdia.minf.stbl.stts.total_duration;

    // Contradictory tags must not produce an out-of-bounds region.
    if leading >= total_media_duration {
        debug!("isomp4: iTunes gapless delay exceeds the media duration, ignoring");
        return None;
    }

    let valid_duration =
        total_media_duration - leading - padding.min(total_media_duration - leading);

    if valid_duration == 0 {
        return None;
    }

    Some(GaplessInfo { start_media_time: leading, valid_duration })
}

fn is_audio_track(trak: &TrakAtom) -> bool {
    trak.mdia
        .minf
        .stbl
        .stsd
        .make_codec_params()
        .as_ref()
        .map(CodecParameters::is_audio)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::GaplessInfo;

    #[test]
    fn padding_is_total_duration_minus_valid_region() {
        let info = GaplessInfo { start_media_time: 2112, valid_duration: 10_000 };

        assert_eq!(info.delay(), 2112);
        assert_eq!(info.padding(2112 + 10_000 + 576), 576);
    }

    #[test]
    fn padding_saturates_to_zero_for_short_media() {
        let info = GaplessInfo { start_media_time: 100, valid_duration: 1_000 };

        // The media is shorter than the declared valid region.
        assert_eq!(info.padding(500), 0);
    }

    #[test]
    fn scale_duration_rounds_up() {
        // 1000 units at 44100 Hz expressed in a 48000 Hz timescale: 1088.4 -> 1089.
        assert_eq!(super::scale_duration(1000, 44100, 48000), 1089);
        // Same timescale is an identity conversion.
        assert_eq!(super::scale_duration(1000, 44100, 44100), 1000);
    }

    #[test]
    fn itunes_smpb_parse() {
        // A typical iTunSMPB value: 0 priming, 2112 delay, 576 padding.
        let value = b"00000000 00000840 00000240 0000273000000000 ";
        let text = std::str::from_utf8(value).unwrap().trim();
        let mut fields = text.split(' ').filter(|f| !f.is_empty());
        let _ = fields.next();
        let leading = u64::from_str_radix(fields.next().unwrap(), 16).unwrap();
        let padding = u64::from_str_radix(fields.next().unwrap(), 16).unwrap();
        assert_eq!(leading, 2112);
        assert_eq!(padding, 576);
    }
}
