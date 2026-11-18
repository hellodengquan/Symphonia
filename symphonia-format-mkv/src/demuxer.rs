// Symphonia
// Copyright (c) 2019-2026 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::collections::{HashMap, VecDeque};
use std::convert::TryFrom;
use std::num::NonZero;

use symphonia_core::errors::{Error, Result, SeekErrorKind, seek_error, unsupported_error};
use symphonia_core::formats::prelude::*;
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_MKV;
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};
use symphonia_core::support_format;
use symphonia_core::units::TimeBase;

use log::{info, warn};

use crate::codecs::make_track_codec_params;
use crate::ebml::{EbmlElementInfo, EbmlError, EbmlIterator, EbmlSchema, ReadEbml};
use crate::lacing::{Frame, extract_frames};
use crate::schema::{MkvElement, MkvSchema};
use crate::segment::{
    AttachmentsElement, BlockGroupElement, ChaptersElement, CuesElement, EbmlHeaderElement,
    InfoElement, MatroskaTicks, NonZeroMatroskaTicks, SeekHeadElement, SegmentTicks,
    SignedTrackTicks, TagsElement, TargetTagsMap, TracksElement,
};

const MKV_FORMAT_INFO: FormatInfo =
    FormatInfo { format: FORMAT_ID_MKV, short_name: "matroska", long_name: "Matroska / WebM" };

pub struct TrackState {
    /// The Matroska track number (Symphonia's track ID).
    track_num: u32,
    /// The default frame duration.
    pub(crate) default_frame_duration: Option<NonZeroMatroskaTicks>,
    /// The codec delay.
    pub(crate) codec_delay: MatroskaTicks,
    /// The track's timebase.
    pub(crate) track_time_base: TimeBase,
    /// The track's timestamp scale.
    pub(crate) track_timestamp_scale: f64,
}

/// Matroska (MKV) and WebM demultiplexer.
///
/// `MkvReader` implements a demuxer for the Matroska and WebM formats.
pub struct MkvReader<'s> {
    /// Iterator over EBML element headers
    iter: EbmlIterator<MediaSourceStream<'s>, MkvSchema>,
    /// The position of the first cluster, relative to the start of the Segment's data, if known.
    /// This is the earliest position within the media that can be seeked to. It is only available
    /// for seekable media sources.
    first_cluster_pos: Option<u64>,
    /// The absolute byte position where the Segment element header begins.
    segment_header_pos: u64,
    media_info: MediaInfo,
    tracks: Vec<Track>,
    track_states: HashMap<u32, TrackState>,
    attachments: Vec<Attachment>,
    chapters: Option<ChapterGroup>,
    metadata: MetadataLog,
    cues: Option<CuesElement>,
    current_cluster: Option<ClusterState>,
    frames: VecDeque<Frame>,
    /// The lowest presentation timestamp observed for each track across the whole media, in that
    /// track's ticks. This lets a seek distinguish a target that is genuinely past the end of the
    /// media from one that lies before the current read position after an end-of-stream scan.
    min_seen_pts: HashMap<u32, i64>,
}

#[derive(Copy, Clone, Debug)]
struct ClusterState {
    /// The cluster timestamp in Segment ticks..
    timestamp: Option<SegmentTicks>,
    /// The start position in bytes of the cluster.
    start: u64,
}

/// A resolved cue point location for a specific track, copied out of the cue index so the reader
/// can be mutably repositioned without borrowing the index.
#[derive(Copy, Clone, Debug)]
struct CueSeek {
    /// The cue point's time in Matroska ticks.
    time: MatroskaTicks,
    /// The position of the cluster, relative to the start of the Segment's data.
    cluster_pos: u64,
    /// The position of the exact block within the cluster/segment, if present.
    cluster_rel_pos: Option<u64>,
}

/// The result of a forward scan while seeking.
#[derive(Copy, Clone, Debug)]
enum ScanOutcome {
    /// A frame covering the requested timestamp was found, at the given actual timestamp.
    Found(Timestamp),
    /// The target lies before the current read position; a rewind is required.
    NeedRewind,
    /// The end of the media was reached before a frame of the requested track was found.
    EndOfStream,
}

impl<'s> MkvReader<'s> {
    pub fn try_new(mss: MediaSourceStream<'s>, opts: FormatOptions) -> Result<Self> {
        // Get the total length of the stream, if possible.
        let (is_seekable, total_len) = (mss.is_seekable(), mss.byte_len());

        match total_len {
            Some(len) if is_seekable => info!("stream is seekable with len={len} bytes."),
            _ => (),
        }

        let mut it = EbmlIterator::new(mss, MkvSchema, total_len);

        // Read the EBML header.
        let ebml = it.next_element::<EbmlHeaderElement>()?;

        if !matches!(ebml.doc_type.as_str(), "matroska" | "webm") {
            return unsupported_error("mkv: not a matroska / webm file");
        }

        // Read the root element, Segment.
        let (segment_header_pos, segment_pos) = match it.next_header()? {
            Some(elem) if elem.element_type() == MkvElement::Segment => {
                (elem.pos(), elem.data_pos())
            }
            _ => return unsupported_error("mkv: missing segment element"),
        };

        // Descend into the Segment element.
        it.push_element()?;

        let mut segment_tracks = None;
        let mut info = None;
        let mut cues = None;
        let mut current_cluster = None;
        let mut seek_positions = Vec::new();
        let mut tags = Vec::new();
        let mut attachments = None;
        let mut chapters = None;

        while let Ok(Some(header)) = it.next_header() {
            match header.element_type() {
                MkvElement::SeekHead => {
                    let seek_head = it.read_master_element::<SeekHeadElement>()?;
                    for element in seek_head.seeks.into_vec() {
                        let element_type = match it.schema().get_element_info(element.id as u32) {
                            Some(info) => info.element_type(),
                            None => continue,
                        };
                        seek_positions.push((element_type, element.position));
                    }
                }
                MkvElement::Tracks => {
                    segment_tracks = Some(it.read_master_element::<TracksElement>()?);
                }
                MkvElement::Info => {
                    info = Some(it.read_master_element::<InfoElement>()?);
                }
                MkvElement::Cues => {
                    cues = Some(it.read_master_element::<CuesElement>()?);
                }
                MkvElement::Tags => {
                    // Multiple tags element per segment allowed.
                    tags.push(it.read_master_element::<TagsElement>()?);
                }
                MkvElement::Cluster => {
                    // Set state for current cluster for the first call of `next_element`.
                    current_cluster =
                        Some(ClusterState { timestamp: None, start: header.pos() - segment_pos });

                    // Don't look forward into the stream since we can't be sure that we'll
                    // find anything useful.
                    break;
                }
                MkvElement::Attachments => {
                    // Only one attachments element per segment is expected.
                    if attachments.is_some() {
                        log::warn!("unexpected attachments element");
                    }
                    attachments = Some(it.read_master_element::<AttachmentsElement>()?);
                }
                MkvElement::Chapters => {
                    // Only one chapters element per segment is expected.
                    if chapters.is_some() {
                        log::warn!("unexpected chapters element");
                    }
                    chapters = Some(it.read_master_element::<ChaptersElement>()?);
                }
                other => {
                    log::debug!("top-level scan ignored element {other:?}");
                }
            }
        }

        if is_seekable {
            // All elements preceeding the element iterator's current position have already been
            // read and do not need to be revisited.
            seek_positions.retain(|sp| sp.1 >= it.pos());
            // Make sure we don't jump backwards unnecessarily.
            seek_positions.sort_by_key(|sp| sp.1);

            for (_, pos) in seek_positions {
                // Ascend back to the segment element.
                it.pop_elements_upto(MkvElement::Segment)?;

                // Seek the iterator to the child element.
                it.seek_to_child(pos)?;

                // Resume iteration.
                let element_type = match it.next_header()? {
                    Some(header) => header.element_type(),
                    _ => continue,
                };

                // Safety: The element type or position may be incorrect. The element iterator will
                // validate the type (as declared in the header) of the element at the seeked
                // position against the element type asked to be read.
                match element_type {
                    MkvElement::Tracks => {
                        segment_tracks = Some(it.read_master_element::<TracksElement>()?);
                    }
                    MkvElement::Info => {
                        info = Some(it.read_master_element::<InfoElement>()?);
                    }
                    MkvElement::Tags => {
                        // Multiple tags element per segment allowed.
                        tags.push(it.read_master_element::<TagsElement>()?);
                    }
                    MkvElement::Cues => {
                        cues = Some(it.read_master_element::<CuesElement>()?);
                    }
                    MkvElement::Attachments => {
                        // Only one attachments element per segment is expected.
                        if attachments.is_some() {
                            log::warn!("unexpected attachments element after meta seek");
                        }
                        attachments = Some(it.read_master_element::<AttachmentsElement>()?);
                    }
                    MkvElement::Chapters => {
                        // Only one chapters element per segment is expected.
                        if chapters.is_some() {
                            log::warn!("unexpected chapters element after meta seek");
                        }
                        chapters = Some(it.read_master_element::<ChaptersElement>()?)
                    }
                    _ => (),
                }
            }
        }

        let segment_tracks =
            segment_tracks.ok_or(Error::DecodeError("mkv: missing Tracks element"))?;

        // If seekable, seek to the start of the first cluster, if known, or the start of the
        // segment. If unseekable, the element iterator is already positioned at the start of the
        // first cluster.
        let first_cluster_pos = if is_seekable {
            let cluster_pos = current_cluster.as_ref().map(|cluster| cluster.start).unwrap_or(0);
            it.seek_to_child(cluster_pos)?;
            let _ = it.next_header()?;
            Some(cluster_pos)
        } else {
            // For an unseekable source the first cluster is the cluster the iterator is currently
            // positioned at, but it cannot be revisited, so do not record a seekable position.
            None
        };

        // Descend into the cluster.
        it.push_element()?;

        let info = info.ok_or(Error::DecodeError("mkv: missing Info element"))?;

        // Create a hashmap of all per-target tags (edition, chapter, & attachment tags).
        let mut per_target_tags: TargetTagsMap = Default::default();

        segment_tracks.get_target_uids(&mut per_target_tags);

        if let Some(chapters) = &chapters {
            chapters.get_target_uids(&mut per_target_tags);
        }
        if let Some(attachments) = &attachments {
            attachments.get_target_uids(&mut per_target_tags);
        }

        // Begin with externally provided metadata and chapters.
        let mut metadata = opts.external_data.metadata.unwrap_or_default();

        // Post-process all tag elements into metadata revisions, while also collecting per-target
        // tags.
        let is_video = segment_tracks.tracks.as_ref().iter().any(|t| t.video.is_some());
        for tag in tags {
            metadata.push(tag.into_metadata(&mut per_target_tags, is_video));
        }

        // Post-process chapters element.
        let chapters = chapters
            .map(|chapters| chapters.into_chapter_group(&mut per_target_tags))
            .unwrap_or(opts.external_data.chapters);

        // Process attachments element.
        let attachments = attachments
            .map(|attachments| attachments.into_attachments(&mut per_target_tags))
            .unwrap_or_default();

        // Should TimeBase use a u64/u64 rational?
        // Reduce the timebase to reduce the chance of overflows later.
        let time_base = TimeBase::new(
            info.timestamp_scale
                .try_into()
                .map_err(|_| Error::Unsupported("mkv: timestamp scale too large (report this)"))?,
            NonZero::new(1_000_000_000).expect("1_000_000_000 is non-zero"),
        )
        .reduce();

        let mut tracks = Vec::new();
        let mut track_states = HashMap::new();

        for track in segment_tracks.tracks {
            // The track's timebase is scaled by the track timestamp scale.
            let track_time_base = time_base
                .scale(track.track_timestamp_scale)
                .ok_or(Error::DecodeError("mkv: track timebase is invalid"))?;

            // Create the track state.
            let state = TrackState {
                // TODO: This should be 64-bit, but track IDs are 32-bit.
                track_num: u32::try_from(track.number.get())
                    .map_err(|_| Error::Unsupported("mkv: track number too large (report this)"))?,
                default_frame_duration: track.default_duration,
                codec_delay: track.codec_delay,
                track_time_base,
                track_timestamp_scale: track.track_timestamp_scale,
            };

            // Create the track.
            let mut tr = Track::new(state.track_num);

            tr.with_time_base(track_time_base);

            if let Some(lang_bcp47) = &track.lang_bcp47 {
                tr.with_language(lang_bcp47);
            } else {
                tr.with_language(&track.lang);
            }

            tr.with_flags(track.flags);

            if let Some(codec_params) = make_track_codec_params(track)? {
                tr.with_codec_params(codec_params);
            }

            tracks.push(tr);
            track_states.insert(state.track_num, state);
        }

        // Populate media information.
        let mut media_info = MediaInfo::new();

        media_info.with_time_base(time_base);

        if let Some(duration) = info.duration {
            media_info.with_duration(Duration::new(duration.get().round() as u64));
        }

        Ok(Self {
            iter: it,
            first_cluster_pos,
            segment_header_pos,
            media_info,
            tracks,
            track_states,
            attachments,
            chapters,
            metadata,
            cues,
            current_cluster,
            frames: VecDeque::new(),
            min_seen_pts: HashMap::new(),
        })
    }

    /// Scan forward from the iterator's current position, together with the already buffered
    /// frames, until a frame for the requested track is found whose presentation timestamp is not
    /// later than the requested timestamp `ts`, while the following frame of the same track is not
    /// earlier than `ts`.
    ///
    /// Frames belonging to other tracks are never removed: they remain queued for delivery in their
    /// original positions. Frames of the requested track that end before the requested timestamp are
    /// discarded.
    ///
    /// The scan distinguishes three outcomes:
    ///
    /// * [`ScanOutcome::Found`] - a covering frame was located; its PTS is the actual seek position.
    /// * [`ScanOutcome::NeedRewind`] - the first frame of the requested track that can be reached is
    ///   already later than the requested timestamp, so the target lies before the current position.
    /// * [`ScanOutcome::EndOfStream`] - the media ended before any frame of the requested track was
    ///   reached.
    fn seek_track_forward_scan(&mut self, track_id: u32, ts: Timestamp) -> Result<ScanOutcome> {
        loop {
            // Process every currently buffered frame exactly once. Frames are moved into a new
            // queue preserving their original delivery order. Frames of other tracks are always
            // retained. Frames of the requested track that end before the requested timestamp are
            // discarded; the first covering frame (and everything after it) is retained.
            let buffered = std::mem::take(&mut self.frames);
            let mut next_queue: VecDeque<Frame> = VecDeque::with_capacity(buffered.len());
            let mut outcome: Option<ScanOutcome> = None;

            for frame in buffered {
                if frame.track_num != track_id || outcome.is_some() {
                    // Other tracks are always retained; once a terminal state is reached all later
                    // frames (including same-track) stay queued for delivery or the rewind retry.
                    next_queue.push_back(frame);
                    continue;
                }

                let frame_ts = frame.pts.into_ts();
                let frame_end = frame
                    .pts
                    .into_ts()
                    .checked_add(frame.dur.into_dur())
                    .ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;

                if frame_ts > ts {
                    // Earliest reachable frame of the requested track begins after the target: the
                    // covering frame is before the current read position.
                    outcome = Some(ScanOutcome::NeedRewind);
                    next_queue.push_back(frame);
                } else if frame_ts == ts || frame_end > ts {
                    // The frame covers the requested timestamp when it begins exactly at `ts`, or
                    // when it begins before `ts` and ends strictly after it. Frames use half-open
                    // intervals, so a non-zero-length frame ending exactly at `ts` does not cover
                    // the instant `ts`; the following frame (which begins at `ts`) does.
                    outcome = Some(ScanOutcome::Found(frame_ts));
                    next_queue.push_back(frame);
                }
                // Else the frame is entirely before the target (`frame_end <= ts`): discard it.
            }

            // Replace the queue with the retained frames; frames decoded in subsequent iterations
            // will be appended after these.
            self.frames = next_queue;

            if let Some(outcome) = outcome {
                return Ok(outcome);
            }

            // No covering frame among the buffered ones. Decode the next cluster element, which
            // appends fresh frames, then re-evaluate.
            if !self.next_element()? {
                // The media ended without reaching a frame of the requested track that covers the
                // target. All observed target frames ended before `ts`, so it is out of range.
                return Ok(ScanOutcome::EndOfStream);
            }
        }
    }

    /// Position the element iterator at the beginning of the cluster referenced by a cue point,
    /// and optionally at the exact block within it. Any previously buffered frames are discarded.
    fn seek_to_cue_point(&mut self, seek: CueSeek) -> Result<()> {
        let CueSeek { time, cluster_pos, cluster_rel_pos } = seek;

        // Ascend back to the segment element.
        self.iter.pop_elements_upto(MkvElement::Segment)?;

        // Seek to the specific cluster element.
        self.iter.seek_to_child(cluster_pos)?;

        // Resume iteration.
        let cluster = match self.iter.next_header()? {
            // The seeked element is a cluster.
            Some(header) if header.element_type() == MkvElement::Cluster => header,
            // The seeked element is not a cluster or there were no more elements at the cue
            // position. The cue point was malformed.
            _ => return seek_error(SeekErrorKind::Unseekable),
        };

        // Convert the cue point's timestamp (Matroska ticks) into Segment ticks for the cluster
        // state.
        let timestamp = time.into_segment_ticks(
            self.media_info.time_base.expect("media info time base always populated"),
        );

        // Update the current cluster metadata. The cluster timestamp element itself will overwrite
        // this once read.
        self.current_cluster =
            Some(ClusterState { timestamp: Some(timestamp), start: cluster.pos() });

        // Descend into the cluster element.
        self.iter.push_element()?;

        // If a cluster relative position is available, seek to the exact simple block or block
        // group element. The relative position is relative to the start of the cluster's data, and
        // the iterator has just descended into the cluster, making it the current parent. Validate
        // that the element at that position is actually a block; otherwise fall back to scanning the
        // cluster from its beginning.
        if let Some(cluster_rel_pos) = cluster_rel_pos {
            // Compute the absolute position of the referenced block so the iterator can be left
            // positioned there after validating the element type.
            let block_abs_pos = self
                .iter
                .parent()
                .map(|parent| parent.data_pos())
                .unwrap_or(0)
                .checked_add(cluster_rel_pos)
                .ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;

            self.iter.seek_to_child(cluster_rel_pos)?;
            let is_block = matches!(
                self.iter.next_header()?,
                Some(header)
                    if matches!(
                        header.element_type(),
                        MkvElement::SimpleBlock | MkvElement::BlockGroup
                    )
            );

            if is_block {
                // The relative position pointed at a block. Seek back to its start so the forward
                // scan reads it as the next element rather than skipping the already-consumed
                // header. This stays within the read-ahead buffer on seekable sources.
                self.iter.seek_to_abs_pos(block_abs_pos)?;
            } else {
                // The relative position did not point at a block. Reset to the beginning of the
                // cluster and scan forward instead.
                self.iter.pop_element()?;
                self.iter.seek_to_child(cluster_pos)?;
                let cluster = match self.iter.next_header()? {
                    Some(header) if header.element_type() == MkvElement::Cluster => header,
                    _ => return seek_error(SeekErrorKind::Unseekable),
                };
                self.current_cluster =
                    Some(ClusterState { timestamp: Some(timestamp), start: cluster.pos() });
                self.iter.push_element()?;
            }
        }

        Ok(())
    }

    /// Reposition the iterator to the beginning of the first cluster of the segment, discarding any
    /// buffered frames. This enables a forward scan to satisfy a seek request that is earlier than
    /// the current read position.
    fn rewind_to_first_cluster(&mut self) -> Result<()> {
        let cluster_pos =
            self.first_cluster_pos.ok_or(Error::SeekError(SeekErrorKind::Unseekable))?;

        // Normalize the iterator onto the Segment element. While nested inside the Segment (the
        // common case, including being inside a cluster) simply ascend to it. If a previous scan
        // reached the end of the media and popped above the Segment, re-open the Segment header so
        // it becomes the current element and push it.
        if self.iter.depth() == 0 {
            // Seek to the Segment header and re-enter it.
            self.iter.seek_to_child(self.segment_header_pos)?;
            let is_segment = matches!(
                self.iter.next_header()?,
                Some(header) if header.element_type() == MkvElement::Segment
            );
            if !is_segment {
                return seek_error(SeekErrorKind::Unseekable);
            }
            self.iter.push_element()?;
        } else {
            // Ascend back to the Segment, tolerating a stack that already lost it.
            match self.iter.pop_elements_upto(MkvElement::Segment) {
                Ok(()) => {}
                Err(EbmlError::NotAnAncestor) => {
                    self.iter.seek_to_child(self.segment_header_pos)?;
                    let is_segment = matches!(
                        self.iter.next_header()?,
                        Some(header) if header.element_type() == MkvElement::Segment
                    );
                    if !is_segment {
                        return seek_error(SeekErrorKind::Unseekable);
                    }
                    self.iter.push_element()?;
                }
                Err(err) => return Err(err.into()),
            }
        }

        // The cluster position is relative to the Segment's data, exactly what `seek_to_child`
        // expects while the Segment is the current parent.
        self.iter.seek_to_child(cluster_pos)?;

        let cluster = match self.iter.next_header()? {
            Some(header) if header.element_type() == MkvElement::Cluster => header,
            _ => return seek_error(SeekErrorKind::Unseekable),
        };

        // The cluster timestamp is not known until the timestamp element is read.
        self.current_cluster = Some(ClusterState { timestamp: None, start: cluster.pos() });

        // Descend into the cluster.
        self.iter.push_element()?;

        Ok(())
    }

    fn seek_track_by_ts_atomic(
        &mut self,
        id: u32,
        tb: TimeBase,
        ts: Timestamp,
    ) -> Result<SeekedTo> {
        // Save the iterator, cluster, and buffered frame states to restore in-case of an error.
        let iter_state = self.iter.save_state();
        let cluster_state = self.current_cluster;
        // Snapshot the pending frames. A successful seek leaves `self.frames` holding exactly the
        // frames of the new read position (the forward scan preserves other-track frames); on
        // failure the snapshot is restored so no pending media is lost.
        let saved_frames = self
            .frames
            .iter()
            .map(|frame| Frame {
                track_num: frame.track_num,
                pts: frame.pts,
                dur: frame.dur,
                data: frame.data.clone(),
            })
            .collect::<VecDeque<Frame>>();

        match self.seek_track_by_ts(id, tb, ts) {
            Ok(seeked_to) => Ok(seeked_to),
            Err(err) => {
                // Restore the saved iterator, cluster, and buffered frame states so reading resumes
                // exactly where it left off, without packet loss or duplication. On an unseekable
                // source the underlying reader never moved, so a failed physical restore is
                // expected and the in-memory state is still valid; preserve the original error.
                if let Err(restore_err) = self.iter.restore_state(iter_state) {
                    log::debug!(
                        "could not restore iterator state after failed seek: {restore_err:?}"
                    );
                }
                self.current_cluster = cluster_state;
                self.frames = saved_frames;
                Err(err)
            }
        }
    }

    fn seek_track_by_ts(&mut self, id: u32, tb: TimeBase, ts: Timestamp) -> Result<SeekedTo> {
        log::debug!("seeking track_id={id} to ts={ts}");

        // If cues exist, use the nearest cue point for the requested track. Cue points store
        // timestamps in Matroska ticks while the timestamp being seeked to is in signed Track ticks.
        // Convert to unsigned Matroska ticks for iterating the cue points. If the timestamp is
        // negative, then this is an error because cue points only contain unsigned Matroska ticks.
        let cue_seek = if let Some(cues) = &self.cues {
            let ts_matroska = SignedTrackTicks::from(ts)
                .try_into_matroska_ticks(tb)
                .ok_or(Error::SeekError(SeekErrorKind::OutOfRange))?;

            // Select the latest cue point, considering only track positions that reference the
            // requested track, whose time is not later than the requested timestamp. Each track's
            // cue positions are independent.
            let mut target: Option<CueSeek> = None;

            'cues: for cue_point in &cues.points {
                if cue_point.time > ts_matroska {
                    break;
                }

                for position in cue_point.positions.iter() {
                    if position.track.get() == u64::from(id) {
                        target = Some(CueSeek {
                            time: cue_point.time,
                            cluster_pos: position.cluster_pos,
                            cluster_rel_pos: position.cluster_rel_pos,
                        });
                        continue 'cues;
                    }
                }
            }

            target
        } else {
            None
        };

        if let Some(seek) = cue_seek {
            log::debug!("found cue point: track_id={}, cluster_pos={}", id, seek.cluster_pos);

            // Any frames buffered before the seek belong to the old position.
            self.frames.clear();

            self.seek_to_cue_point(seek)?;
        }

        // Scan forward from the current position. With a cue this starts at the cue's cluster;
        // without a cue it starts at the current read position so buffered frames and forward
        // progress are reused.
        match self.seek_track_forward_scan(id, ts)? {
            ScanOutcome::Found(actual_ts) => {
                return Ok(SeekedTo { track_id: id, required_ts: ts, actual_ts });
            }
            ScanOutcome::NeedRewind => {}
            ScanOutcome::EndOfStream => {
                // The scan reached the end of the media without a covering frame. If a frame of the
                // requested track was ever observed at or before the requested timestamp, the target
                // may simply lie before the current read position: rewind and re-scan. Otherwise the
                // requested timestamp is genuinely out of range.
                let observed_before_target =
                    self.min_seen_pts.get(&id).is_some_and(|min_pts| *min_pts <= ts.get());
                if !observed_before_target {
                    return Err(Error::SeekError(SeekErrorKind::OutOfRange));
                }
            }
        }

        // The target precedes the current read position. Rewind to the first cluster and scan the
        // whole media again. On an unseekable source this fails cleanly.
        //
        // Only frames of the requested track are invalidated: frames belonging to other tracks are
        // preserved so they remain deliverable, while the re-scan rebuilds the requested track's
        // frames from the first cluster.
        self.frames.retain(|frame| frame.track_num != id);
        self.rewind_to_first_cluster()?;

        match self.seek_track_forward_scan(id, ts)? {
            ScanOutcome::Found(actual_ts) => {
                Ok(SeekedTo { track_id: id, required_ts: ts, actual_ts })
            }
            // After a full re-scan from the first cluster the target either exists or is genuinely
            // out of range.
            _ => Err(Error::SeekError(SeekErrorKind::OutOfRange)),
        }
    }

    fn next_element(&mut self) -> Result<bool> {
        match self.iter.next_header()? {
            None => {
                // The EBML iterator has consumed all child elements at the current level of the
                // document.
                match self.iter.parent() {
                    None => {
                        // The parent is the document. The media has ended.
                        return Ok(false);
                    }
                    Some(parent) if parent.element_type() == MkvElement::Cluster => {
                        // The parent was a cluster element. Reset the cluster state.
                        self.current_cluster = None;
                    }
                    _ => (),
                }

                // Ascend to its parent.
                self.iter.pop_element()?;
            }
            Some(child) => {
                match child.element_type() {
                    // Cluster element.
                    MkvElement::Cluster => {
                        self.current_cluster =
                            Some(ClusterState { timestamp: None, start: child.data_pos() });

                        // Descend into the cluster.
                        self.iter.push_element()?;
                    }
                    // Children of a cluster element.
                    MkvElement::Timestamp => {
                        // Cluster timestamp element.
                        match self.current_cluster.as_mut() {
                            Some(cc) => {
                                cc.timestamp = self.iter.read_u64()?.map(SegmentTicks::from)
                            }
                            _ => log::warn!("expected to have cluster"),
                        }
                    }
                    block_type @ (MkvElement::SimpleBlock | MkvElement::BlockGroup) => {
                        // Get the current cluster information.
                        let Some(cluster) = self.current_cluster.as_ref() else {
                            log::warn!("expected to have cluster");
                            return Ok(true);
                        };

                        // Get the cluster timestamp.
                        let Some(cluster_ts) = cluster.timestamp else {
                            log::warn!("missing cluster timestamp");
                            return Ok(true);
                        };

                        // Get block data and duration.
                        let (data, duration) = match block_type {
                            MkvElement::SimpleBlock => (self.iter.read_binary()?, None),
                            MkvElement::BlockGroup => {
                                let group = self.iter.read_master_element::<BlockGroupElement>()?;
                                (group.data, group.duration)
                            }
                            _ => unreachable!(),
                        };

                        // Extract frames.
                        let frames_before = self.frames.len();
                        if !extract_frames(
                            &data,
                            duration,
                            cluster_ts,
                            &self.track_states,
                            &mut self.frames,
                        )? {
                            warn!("pts for block is too large");
                            return Ok(false);
                        }

                        // Remember the lowest PTS observed for each track so a seek can tell an
                        // out-of-range target apart from one before the current read position. Only
                        // the frames appended by this block need inspecting.
                        for frame in self.frames.iter().skip(frames_before) {
                            self.min_seen_pts
                                .entry(frame.track_num)
                                .and_modify(|min_pts| {
                                    *min_pts = (*min_pts).min(frame.pts.get());
                                })
                                .or_insert(frame.pts.get());
                        }
                    }
                    // All other elements.
                    other => {
                        log::debug!("ignored element {other:?}");
                    }
                }
            }
        }

        Ok(true)
    }
}

impl ProbeableFormat<'_> for MkvReader<'_> {
    fn try_probe_new(
        mss: MediaSourceStream<'_>,
        opts: FormatOptions,
    ) -> Result<Box<dyn FormatReader + '_>>
    where
        Self: Sized,
    {
        Ok(Box::new(MkvReader::try_new(mss, opts)?))
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            MKV_FORMAT_INFO,
            &["webm", "mkv"],
            &["video/webm", "video/x-matroska"],
            &[b"\x1A\x45\xDF\xA3"] // Top-level element Ebml element
        )]
    }
}

impl FormatReader for MkvReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &MKV_FORMAT_INFO
    }

    fn media_info(&self) -> &MediaInfo {
        &self.media_info
    }

    fn attachments(&self) -> &[Attachment] {
        &self.attachments
    }

    fn chapters(&self) -> Option<&ChapterGroup> {
        self.chapters.as_ref()
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        if self.tracks.is_empty() {
            return seek_error(SeekErrorKind::Unseekable);
        }

        match to {
            SeekTo::Time { time, track_id } => {
                let track = match track_id {
                    Some(id) => self.tracks.iter().find(|track| track.id == id),
                    None => self.tracks.first(),
                };
                let track = track.ok_or(Error::SeekError(SeekErrorKind::InvalidTrack))?;
                let tb = track.time_base.expect("track always has a timebase");
                let ts = match tb.calc_timestamp(time) {
                    Some(ts) => ts,
                    None => {
                        warn!("seek ts is too large");
                        return seek_error(SeekErrorKind::OutOfRange);
                    }
                };
                let track_id = track.id;
                self.seek_track_by_ts_atomic(track_id, tb, ts)
            }
            SeekTo::Timestamp { ts, track_id } => {
                match self.tracks.iter().find(|t| t.id == track_id) {
                    Some(track) => {
                        let tb = track.time_base.expect("track always has a timebase");
                        self.seek_track_by_ts_atomic(track_id, tb, ts)
                    }
                    None => seek_error(SeekErrorKind::InvalidTrack),
                }
            }
        }
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }

    fn next_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(frame) = self.frames.pop_front() {
                return Ok(Some(Packet::new(
                    frame.track_num,
                    frame.pts.into_ts(),
                    frame.dur.into_dur(),
                    frame.data,
                )));
            }

            if !self.next_element()? {
                // Reached the end of stream.
                return Ok(None);
            }
        }
    }

    fn into_inner<'s>(self: Box<Self>) -> MediaSourceStream<'s>
    where
        Self: 's,
    {
        self.iter.into_inner()
    }
}

impl Scoreable for MkvReader<'_> {
    fn score(_src: ScopedStream<&mut MediaSourceStream<'_>>) -> Result<Score> {
        Ok(Score::Supported(255))
    }
}

impl ReadEbml for MediaSourceStream<'_> {}

impl From<EbmlError> for Error {
    fn from(value: EbmlError) -> Self {
        // All non-IO EBML errors are mapped to a decode error.
        let msg = match value {
            EbmlError::IoError(err) => return Error::IoError(err),
            EbmlError::InvalidEbmlElementIdLength => "mkv (ebml): invalid ebml element id length",
            EbmlError::InvalidEbmlDataLength => "mkv (ebml): invalid ebml vint length",
            EbmlError::UnknownElement => "mkv (ebml): the element is unknown",
            EbmlError::UnknownElementDataSize => "mkv (ebml): the element data size is unknown",
            EbmlError::UnexpectedElement => "mkv (ebml): encountered an unexpected element",
            EbmlError::UnexpectedElementDataType => {
                "mkv (ebml): unexpected element data type for the operation"
            }
            EbmlError::UnexpectedElementDataSize => {
                "mkv (ebml): unexpected data size for the element's data type"
            }
            EbmlError::NoElement => "mkv (ebml): no current element",
            EbmlError::NoParent => "mkv (ebml): no parent element",
            EbmlError::NotAnAncestor => {
                "mkv (ebml): the element is not an ancestor of the current element"
            }
            EbmlError::Overrun => "mkv (ebml): the element was overrun when read",
            EbmlError::ExpectedMasterElement => "mkv (ebml): expected a master element",
            EbmlError::ExpectedNonMasterElement => "mkv (ebml): expected a non-master element",
            EbmlError::SeekOutOfRange => "mkv (ebml): the seek was out of range",
            EbmlError::BufferTooSmall => "mkv (ebml): the buffer is too small",
            EbmlError::MaximumDepthReached => "mkv (ebml): maximum ebml document depth reached",
            EbmlError::ElementError(err) => err,
        };
        Error::DecodeError(msg)
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use symphonia_core::errors::{Error, SeekErrorKind};
    use symphonia_core::formats::prelude::*;
    use symphonia_core::io::{MediaSourceStream, MediaSourceStreamOptions, ReadOnlySource};
    use symphonia_core::units::Timestamp;

    use super::MkvReader;

    /// Matroska element identifiers used by the test media builder.
    mod id {
        pub const EBML: u32 = 0x1a45dfa3;
        pub const DOC_TYPE: u32 = 0x4282;
        pub const SEGMENT: u32 = 0x18538067;
        pub const INFO: u32 = 0x1549a966;
        pub const TIMESTAMP_SCALE: u32 = 0x2ad7b1;
        pub const DURATION: u32 = 0x4489;
        pub const MUXING_APP: u32 = 0x4d80;
        pub const WRITING_APP: u32 = 0x5741;
        pub const TRACKS: u32 = 0x1654ae6b;
        pub const TRACK_ENTRY: u32 = 0xae;
        pub const TRACK_NUMBER: u32 = 0xd7;
        pub const TRACK_UID: u32 = 0x73c5;
        pub const TRACK_TYPE: u32 = 0x83;
        pub const CODEC_ID: u32 = 0x86;
        pub const DEFAULT_DURATION: u32 = 0x23e383;
        pub const AUDIO: u32 = 0xe1;
        pub const SAMPLING_FREQUENCY: u32 = 0xb5;
        pub const CHANNELS: u32 = 0x9f;
        pub const CLUSTER: u32 = 0x1f43b675;
        pub const CLUSTER_TIMESTAMP: u32 = 0xe7;
        pub const BLOCK_GROUP: u32 = 0xa0;
        pub const BLOCK: u32 = 0xa1;
        pub const BLOCK_DURATION: u32 = 0x9b;
        pub const CUES: u32 = 0x1c53bb6b;
        pub const CUE_POINT: u32 = 0xbb;
        pub const CUE_TIME: u32 = 0xb3;
        pub const CUE_TRACK_POSITIONS: u32 = 0xb7;
        pub const CUE_TRACK: u32 = 0xf7;
        pub const CUE_CLUSTER_POSITION: u32 = 0xf1;
        pub const CUE_RELATIVE_POSITION: u32 = 0xf0;
    }

    /// Encode an unsigned integer as an EBML variable-size integer.
    fn encode_vint(value: u64) -> Vec<u8> {
        // Determine the minimum length (1..=8) required to store the value with its VINT marker.
        let mut len = 1usize;
        let mut marker_bit = 1u64 << 7;
        while value >= marker_bit && len < 8 {
            len += 1;
            marker_bit = 1u64 << (7 * len);
        }
        let mut out = Vec::with_capacity(len);
        let raw = value | marker_bit;
        for shift in (0..len).rev() {
            out.push((raw >> (8 * shift)) as u8);
        }
        out
    }

    /// Encode an EBML element with an explicit identifier and payload.
    fn element(element_id: u32, payload: &[u8]) -> Vec<u8> {
        let id_len = if element_id <= 0xff {
            1
        } else if element_id <= 0xffff {
            2
        } else if element_id <= 0xff_ffff {
            3
        } else {
            4
        };

        let mut out = Vec::new();
        for shift in (0..id_len).rev() {
            out.push((element_id >> (8 * shift)) as u8);
        }
        out.extend_from_slice(&encode_vint(payload.len() as u64));
        out.extend_from_slice(payload);
        out
    }

    fn uint_element(element_id: u32, value: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut v = value;
        loop {
            bytes.push((v & 0xff) as u8);
            v >>= 8;
            if v == 0 {
                break;
            }
        }
        bytes.reverse();
        element(element_id, &bytes)
    }

    fn float64_element(element_id: u32, value: f64) -> Vec<u8> {
        element(element_id, &value.to_be_bytes())
    }

    fn string_element(element_id: u32, value: &str) -> Vec<u8> {
        element(element_id, value.as_bytes())
    }

    /// Build a simple block payload: track number vint, signed 16-bit relative timestamp, flags
    /// byte (no lacing), followed by frame data.
    fn block_payload(track_num: u32, rel_ts: i16, frame_data: &[u8]) -> Vec<u8> {
        let mut payload = encode_vint(u64::from(track_num));
        payload.extend_from_slice(&rel_ts.to_be_bytes());
        payload.push(0x80); // No lacing.
        payload.extend_from_slice(frame_data);
        payload
    }

    /// Build a BlockGroup containing a Block with an explicit 10ms block duration so frames carry
    /// a non-zero duration regardless of default-duration handling.
    fn block_group(track_num: u32, rel_ts: i16, frame_data: &[u8]) -> Vec<u8> {
        let block = element(id::BLOCK, &block_payload(track_num, rel_ts, frame_data));
        // BlockDuration is in Track ticks; the 1ms timestamp scale makes ticks equal milliseconds.
        let duration = uint_element(id::BLOCK_DURATION, 10);
        element(id::BLOCK_GROUP, &[block, duration].concat())
    }

    /// A single media block placed in a cluster.
    struct BlockSpec {
        track: u32,
        rel_ts: i16,
        data: Vec<u8>,
    }

    struct ClusterSpec {
        timestamp: u64,
        blocks: Vec<BlockSpec>,
    }

    struct CueSpec {
        time: u64,
        track: u32,
    }

    struct MkvBuilder {
        with_cues: bool,
        with_relative_positions: bool,
        multi_track_cue_positions: bool,
        clusters: Vec<ClusterSpec>,
        cues: Vec<CueSpec>,
    }

    impl MkvBuilder {
        fn new(with_cues: bool) -> Self {
            MkvBuilder {
                with_cues,
                with_relative_positions: false,
                multi_track_cue_positions: false,
                clusters: Vec::new(),
                cues: Vec::new(),
            }
        }

        fn with_relative_positions(mut self) -> Self {
            self.with_relative_positions = true;
            self
        }

        fn with_multi_track_cue_positions(mut self) -> Self {
            self.multi_track_cue_positions = true;
            self
        }

        fn add_cluster(&mut self, timestamp: u64, blocks: Vec<BlockSpec>) {
            self.clusters.push(ClusterSpec { timestamp, blocks });
        }

        fn add_cue(&mut self, time: u64, track: u32) {
            self.cues.push(CueSpec { time, track });
        }

        fn build(&self) -> Vec<u8> {
            // EBML header declaring a WebM document.
            let mut ebml_payload = Vec::new();
            ebml_payload.extend(string_element(id::DOC_TYPE, "webm"));
            let ebml = element(id::EBML, &ebml_payload);

            // Info: 1ms timestamp scale (1_000_000 ns), so segment ticks equal milliseconds.
            let mut info_payload = Vec::new();
            info_payload.extend(uint_element(id::TIMESTAMP_SCALE, 1_000_000));
            let last_ts = self
                .clusters
                .last()
                .and_then(|c| c.blocks.last())
                .map(|b| c_timestamp(b, self.clusters.last().unwrap().timestamp))
                .unwrap_or(0);
            info_payload.extend(float64_element(id::DURATION, last_ts as f64));
            info_payload.extend(string_element(id::MUXING_APP, "symphonia-test"));
            info_payload.extend(string_element(id::WRITING_APP, "symphonia-test"));
            let info = element(id::INFO, &info_payload);

            // Two audio tracks. Track numbers 1 and 2, frame duration 10ms each.
            let track1 = self.track_entry(1, 1);
            let track2 = self.track_entry(2, 2);
            let tracks = element(id::TRACKS, &[track1, track2].concat());

            // Clusters. First encode the cluster bodies, recording each cluster's offset relative
            // to the start of the cluster region. They are made segment-relative once the size of
            // the elements that precede them is known.
            let mut cluster_bytes = Vec::new();
            let mut cluster_offsets: Vec<u64> = Vec::new();
            for cluster in &self.clusters {
                cluster_offsets.push(cluster_bytes.len() as u64);

                let mut payload = Vec::new();
                payload.extend(uint_element(id::CLUSTER_TIMESTAMP, cluster.timestamp));
                for block in &cluster.blocks {
                    payload.extend(block_group(block.track, block.rel_ts, &block.data));
                }
                cluster_bytes.extend(element(id::CLUSTER, &payload));
            }

            // Cues must precede the clusters: the reader's top-level scan stops at the first
            // Cluster and only revisits later elements via a SeekHead, which these minimal test
            // files omit. Because the cue block sits between the Tracks and clusters, its encoded
            // length contributes to every cluster's segment-relative position. First build the cue
            // block using provisional cluster offsets, measure its length, then rebuild it with the
            // corrected, segment-relative positions.
            let prefix_len = (info.len() + tracks.len()) as u64;

            let build_cues = |offset: u64| -> Vec<u8> {
                if !self.with_cues {
                    return Vec::new();
                }

                let build_position = |cue: &CueSpec| -> Vec<u8> {
                    let mut positions_payload = Vec::new();
                    positions_payload.extend(uint_element(id::CUE_TRACK, u64::from(cue.track)));

                    // Cue points are created at cluster boundaries in this builder.
                    let cluster_idx = self
                        .clusters
                        .iter()
                        .position(|c| c.timestamp == cue.time)
                        .expect("cue time must match a cluster timestamp");
                    let cluster_pos = offset + cluster_offsets[cluster_idx];
                    positions_payload.extend(uint_element(id::CUE_CLUSTER_POSITION, cluster_pos));

                    if self.with_relative_positions {
                        // CueRelativePosition is relative to the start of the cluster's data. The
                        // first block follows the cluster's timestamp element.
                        let timestamp_elem = uint_element(id::CLUSTER_TIMESTAMP, cue.time);
                        let block_rel = timestamp_elem.len() as u64;
                        positions_payload
                            .extend(uint_element(id::CUE_RELATIVE_POSITION, block_rel));
                    }
                    element(id::CUE_TRACK_POSITIONS, &positions_payload)
                };

                let mut cue_payload = Vec::new();
                if self.multi_track_cue_positions {
                    // Emit one CuePoint per distinct time, carrying every track's position.
                    let mut times: Vec<u64> = self.cues.iter().map(|cue| cue.time).collect();
                    times.dedup();
                    for time in times {
                        let mut point_payload = Vec::new();
                        point_payload.extend(uint_element(id::CUE_TIME, time));
                        for cue in self.cues.iter().filter(|cue| cue.time == time) {
                            point_payload.extend(build_position(cue));
                        }
                        cue_payload.extend(element(id::CUE_POINT, &point_payload));
                    }
                } else {
                    for cue in &self.cues {
                        let mut point_payload = Vec::new();
                        point_payload.extend(uint_element(id::CUE_TIME, cue.time));
                        point_payload.extend(build_position(cue));
                        cue_payload.extend(element(id::CUE_POINT, &point_payload));
                    }
                }

                element(id::CUES, &cue_payload)
            };

            // First pass measures the encoded cue block length. Larger cluster offsets may widen the
            // VINT encodings inside the cue block, so iterate until the length stabilizes.
            let mut cues_len = build_cues(prefix_len).len() as u64;
            loop {
                let next_len = build_cues(prefix_len + cues_len).len() as u64;
                if next_len == cues_len {
                    break;
                }
                cues_len = next_len;
            }
            let cues_bytes = build_cues(prefix_len + cues_len);

            let segment_payload = [info, tracks, cues_bytes, cluster_bytes].concat();

            let segment = element(id::SEGMENT, &segment_payload);
            [ebml, segment].concat()
        }

        fn track_entry(&self, number: u32, uid: u32) -> Vec<u8> {
            let mut payload = Vec::new();
            payload.extend(uint_element(id::TRACK_NUMBER, u64::from(number)));
            payload.extend(uint_element(id::TRACK_UID, u64::from(uid)));
            payload.extend(uint_element(id::TRACK_TYPE, 2)); // Audio.
            payload.extend(string_element(id::CODEC_ID, "A_OPUS"));
            // 10ms default frame duration in Matroska ticks (nanoseconds).
            payload.extend(uint_element(id::DEFAULT_DURATION, 10_000_000));

            let mut audio = Vec::new();
            audio.extend(float64_element(id::SAMPLING_FREQUENCY, 48_000.0));
            audio.extend(uint_element(id::CHANNELS, 1));
            payload.extend(element(id::AUDIO, &audio));

            element(id::TRACK_ENTRY, &payload)
        }
    }

    fn c_timestamp(block: &BlockSpec, cluster_ts: u64) -> u64 {
        cluster_ts.saturating_add_signed(i64::from(block.rel_ts))
    }

    fn reader(data: Vec<u8>) -> Box<dyn FormatReader> {
        let cursor = Cursor::new(data);
        let mss = MediaSourceStream::new(Box::new(cursor), MediaSourceStreamOptions::default());
        Box::new(
            MkvReader::try_new(mss, FormatOptions::default())
                .unwrap_or_else(|err| panic!("reader should build: {err:?}")),
        )
    }

    fn seek_to_ts(reader: &mut Box<dyn FormatReader>, ts: i64, track_id: u32) -> SeekedTo {
        reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(ts), track_id })
            .expect("seek should succeed")
    }

    /// Collect the remaining packets, returning (track_id, pts) pairs.
    fn drain_pts(reader: &mut Box<dyn FormatReader>) -> Vec<(u32, i64)> {
        let mut packets = Vec::new();
        while let Some(packet) = reader.next_packet().expect("read should not error") {
            packets.push((packet.track_id, packet.pts.get()));
        }
        packets
    }

    /// Build a 50ms two-track media: 5 clusters at 0/10/20/30/40ms, each holding one block per
    /// track with 10ms frame duration. Cues (when present) point at every cluster for both tracks.
    fn build_media(with_cues: bool) -> Vec<u8> {
        let mut builder = MkvBuilder::new(with_cues);

        for cluster_ts in [0u64, 10, 20, 30, 40] {
            let mut blocks = Vec::new();
            for track in [1u32, 2] {
                blocks.push(BlockSpec { track, rel_ts: 0, data: vec![track as u8; 4] });
            }
            builder.add_cluster(cluster_ts, blocks);

            if with_cues {
                builder.add_cue(cluster_ts, 1);
                builder.add_cue(cluster_ts, 2);
            }
        }

        builder.build()
    }

    #[test]
    fn sequential_read_yields_all_frames_in_order() {
        for with_cues in [false, true] {
            let mut reader = reader(build_media(with_cues));
            let packets = drain_pts(&mut reader);

            // Each of the 5 clusters contributes one frame per track.
            assert_eq!(packets.len(), 10, "cues={with_cues}");

            for (index, (track, pts)) in packets.iter().enumerate() {
                let expected_track = if index % 2 == 0 { 1 } else { 2 };
                assert_eq!(*track, expected_track, "cues={with_cues}");
                assert_eq!(*pts, (index / 2 * 10) as i64, "cues={with_cues}");
            }
        }
    }

    #[test]
    fn seek_forward_lands_on_frame_covering_requested_ts() {
        for with_cues in [false, true] {
            let mut reader = reader(build_media(with_cues));

            // Seek both tracks to 25ms: the frame at 20ms covers the interval [20, 30), so the
            // actual seek position is 20ms (never later than the requested 25ms).
            let seeked = seek_to_ts(&mut reader, 25, 1);
            assert_eq!(seeked.actual_ts.get(), 20, "cues={with_cues}");
            assert!(seeked.actual_ts.get() <= 25);

            let seeked = seek_to_ts(&mut reader, 25, 2);
            assert_eq!(seeked.actual_ts.get(), 20, "cues={with_cues}");

            // Frames earlier than the seek position were discarded for both tracks; reading resumes
            // at the covering frame for each track without mixing in pre-seek media.
            let packets = drain_pts(&mut reader);
            let track1: Vec<i64> =
                packets.iter().filter(|(t, _)| *t == 1).map(|(_, p)| *p).collect();
            let track2: Vec<i64> =
                packets.iter().filter(|(t, _)| *t == 2).map(|(_, p)| *p).collect();

            assert_eq!(track1, vec![20, 30, 40], "cues={with_cues}");
            assert_eq!(track2, vec![20, 30, 40], "cues={with_cues}");
        }
    }

    #[test]
    fn seek_backward_is_supported_with_and_without_cues() {
        for with_cues in [false, true] {
            let mut reader = reader(build_media(with_cues));

            // Move to the end of the media first.
            let seeked = seek_to_ts(&mut reader, 45, 1);
            assert_eq!(seeked.actual_ts.get(), 40, "cues={with_cues}");

            // Seek backwards to 15ms: the frame at 10ms covers [10, 20).
            let seeked = seek_to_ts(&mut reader, 15, 1);
            assert_eq!(seeked.actual_ts.get(), 10, "cues={with_cues}");

            let packets = drain_pts(&mut reader);
            let track1: Vec<i64> =
                packets.iter().filter(|(t, _)| *t == 1).map(|(_, p)| *p).collect();
            assert_eq!(track1, vec![10, 20, 30, 40], "cues={with_cues}");
        }
    }

    #[test]
    fn seek_keeps_other_tracks_buffered_frames() {
        // This is specifically a no-cue case: frames are buffered during a forward seek and frames
        // of the non-requested track must not be discarded.
        let mut reader = reader(build_media(false));

        // Read a few packets so frames remain buffered inside the reader.
        let first = reader.next_packet().unwrap().unwrap();
        assert_eq!((first.track_id, first.pts.get()), (1, 0));
        let second = reader.next_packet().unwrap().unwrap();
        assert_eq!((second.track_id, second.pts.get()), (2, 0));

        // Seek track 1 forward to 35ms. Track 2's already decoded and future frames must survive.
        let seeked = seek_to_ts(&mut reader, 35, 1);
        assert_eq!(seeked.actual_ts.get(), 30);

        let packets = drain_pts(&mut reader);
        let track1: Vec<i64> = packets.iter().filter(|(t, _)| *t == 1).map(|(_, p)| *p).collect();
        let track2: Vec<i64> = packets.iter().filter(|(t, _)| *t == 2).map(|(_, p)| *p).collect();

        // Track 1 resumes at the seek position, track 2 continues from its current position without
        // any lost packets.
        assert_eq!(track1, vec![30, 40]);
        assert_eq!(track2, vec![10, 20, 30, 40]);
    }

    #[test]
    fn failed_seek_preserves_read_position_and_pending_frames() {
        // Drain the full stream once to establish the expected packet sequence.
        let expected = drain_pts(&mut reader(build_media(false)));

        // A second reader reproduces the same sequence after a failed seek.
        let mut reader = reader(build_media(false));

        // Consume one packet so frames remain buffered inside the reader.
        let first = reader.next_packet().unwrap().unwrap();
        assert_eq!((first.track_id, first.pts.get()), (1, 0));

        // Request a timestamp beyond the end of the media.
        let result = reader.seek(
            SeekMode::Accurate,
            SeekTo::Timestamp { ts: Timestamp::new(100_000), track_id: 1 },
        );
        assert!(matches!(result, Err(Error::SeekError(SeekErrorKind::OutOfRange))));

        // Reading must continue exactly where it left off: the first packet was already delivered,
        // so the remainder matches the tail of the full sequence without loss or duplication.
        let after = drain_pts(&mut reader);
        assert_eq!(&expected[1..], after.as_slice());
    }

    #[test]
    fn seek_actual_ts_is_never_after_requested_ts() {
        for with_cues in [false, true] {
            // Request exactly a frame boundary.
            let mut rdr = reader(build_media(with_cues));
            let seeked = seek_to_ts(&mut rdr, 30, 2);
            assert_eq!(seeked.actual_ts.get(), 30, "cues={with_cues}");

            // A fresh reader for the request one tick before a boundary: must land on the previous
            // frame.
            let mut rdr = reader(build_media(with_cues));
            let seeked = seek_to_ts(&mut rdr, 29, 2);
            assert_eq!(seeked.actual_ts.get(), 20, "cues={with_cues}");
            assert!(seeked.actual_ts.get() <= 29);
        }
    }

    #[test]
    fn per_track_cue_positions_are_independent() {
        // Build media where only track 2 has cue points. A seek on track 1 must still succeed via
        // the linear fallback, while a seek on track 2 must use its cue.
        let mut builder = MkvBuilder::new(true);
        for cluster_ts in [0u64, 10, 20, 30, 40] {
            let blocks = [1u32, 2]
                .iter()
                .map(|track| BlockSpec { track: *track, rel_ts: 0, data: vec![*track as u8; 4] })
                .collect();
            builder.add_cluster(cluster_ts, blocks);
            // Only track 2 gets a cue point at every cluster.
            builder.add_cue(cluster_ts, 2);
        }
        let mut reader = reader(builder.build());

        let seeked = seek_to_ts(&mut reader, 35, 1);
        assert_eq!(seeked.actual_ts.get(), 30);

        let seeked = seek_to_ts(&mut reader, 35, 2);
        assert_eq!(seeked.actual_ts.get(), 30);

        let packets = drain_pts(&mut reader);
        let track1: Vec<i64> = packets.iter().filter(|(t, _)| *t == 1).map(|(_, p)| *p).collect();
        let track2: Vec<i64> = packets.iter().filter(|(t, _)| *t == 2).map(|(_, p)| *p).collect();
        assert_eq!(track1, vec![30, 40]);
        assert_eq!(track2, vec![30, 40]);
    }

    #[test]
    fn unseekable_source_rejects_backward_seek_without_losing_buffered_frames() {
        let data = build_media(false);
        let read_only = ReadOnlySource::new(Cursor::new(data));
        let mss = MediaSourceStream::new(Box::new(read_only), MediaSourceStreamOptions::default());
        let mut reader: Box<dyn FormatReader> =
            Box::new(MkvReader::try_new(mss, FormatOptions::default()).expect("reader builds"));

        // Read the first packet. This leaves subsequent frames buffered and more data pending.
        let first = reader.next_packet().unwrap().unwrap();
        assert_eq!((first.track_id, first.pts.get()), (1, 0));

        // A backward seek cannot be satisfied on an unseekable source: it must fail cleanly with an
        // `Unseekable` error rather than panic or return a bogus position.
        let result = reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(0), track_id: 1 });
        assert!(matches!(result, Err(Error::SeekError(SeekErrorKind::Unseekable))));

        // The already-buffered frame that follows the one delivered before the failed seek must
        // still be available without duplication of the first packet.
        let second = reader.next_packet().unwrap().unwrap();
        assert_eq!((second.track_id, second.pts.get()), (2, 0));
    }

    #[test]
    fn seeking_invalid_track_returns_error() {
        let mut reader = reader(build_media(false));
        let result = reader
            .seek(SeekMode::Accurate, SeekTo::Timestamp { ts: Timestamp::new(0), track_id: 99 });
        assert!(matches!(result, Err(Error::SeekError(SeekErrorKind::InvalidTrack))));
    }

    #[test]
    fn seek_uses_cue_relative_positions() {
        // Build media whose cues carry an explicit relative position pointing at the first block of
        // each cluster. The accurate seek contract must be identical to the no-relative case.
        let mut builder = MkvBuilder::new(true).with_relative_positions();
        for cluster_ts in [0u64, 10, 20, 30, 40] {
            let blocks = [1u32, 2]
                .iter()
                .map(|track| BlockSpec { track: *track, rel_ts: 0, data: vec![*track as u8; 4] })
                .collect();
            builder.add_cluster(cluster_ts, blocks);
            builder.add_cue(cluster_ts, 1);
            builder.add_cue(cluster_ts, 2);
        }
        let mut reader = reader(builder.build());

        let seeked = seek_to_ts(&mut reader, 35, 1);
        assert_eq!(seeked.actual_ts.get(), 30);

        // The cue's relative position points directly at track 1's block in the cluster at 30ms, so
        // the first packet delivered for the seeked track is the covering frame at 30ms, followed by
        // the remaining track-1 frames.
        let packets = drain_pts(&mut reader);
        let track1: Vec<i64> = packets.iter().filter(|(t, _)| *t == 1).map(|(_, p)| *p).collect();
        assert_eq!(track1, vec![30, 40]);
    }

    #[test]
    fn multiple_cue_track_positions_per_cue_point_are_parsed() {
        // A single CuePoint containing a CueTrackPositions entry for each track must make both
        // positions independently selectable. The parser previously kept only the last position.
        let mut builder = MkvBuilder::new(true).with_multi_track_cue_positions();
        for cluster_ts in [0u64, 10, 20, 30, 40] {
            let blocks = [1u32, 2]
                .iter()
                .map(|track| BlockSpec { track: *track, rel_ts: 0, data: vec![*track as u8; 4] })
                .collect();
            builder.add_cluster(cluster_ts, blocks);
            builder.add_cue(cluster_ts, 1);
            builder.add_cue(cluster_ts, 2);
        }

        // Each track can be seeked using its own position within the shared cue point.
        let mut rdr = reader(builder.build());
        let seeked = seek_to_ts(&mut rdr, 25, 1);
        assert_eq!(seeked.actual_ts.get(), 20);

        let mut rdr = reader(builder.build());
        let seeked = seek_to_ts(&mut rdr, 25, 2);
        assert_eq!(seeked.actual_ts.get(), 20);
    }
}
