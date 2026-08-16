//! Added-audio-track bookkeeping across a background decode.
//!
//! A decode runs on a worker and finishes whenever it finishes; the user can
//! reorder or remove tracks in the meantime. These tests deliver the worker's
//! result by hand instead of spawning one, so the interleaving that used to be
//! a narrow timing window is exact and reproducible.

use clip_editor::app::App;
use clip_editor::audio::Pcm;
use clip_editor::jobs::Msg;
use std::sync::Arc;

/// PCM whose sample value identifies which track it belongs to.
fn tone(value: f32) -> Arc<Pcm> {
    Arc::new(Pcm {
        samples: vec![value; 64],
    })
}

fn app_with_two_tracks(ctx: &egui::Context) -> (App, clip_core::TrackId, clip_core::TrackId) {
    let mut app = App::with_context(ctx);
    let mut source = clip_core::SourceInfo {
        width: 640,
        height: 360,
        ..Default::default()
    };
    source.duration = clip_core::RationalTime::new(10, 1);
    source.avg_frame_rate = clip_core::Rational::new(30, 1);
    source.r_frame_rate = source.avg_frame_rate;
    app.project = Some(clip_core::Project::new(source));

    // Push tracks directly: `add_audio_track_from_path` would spawn a real
    // decode, and what is under test is the bookkeeping, not ffmpeg.
    let a = clip_core::AudioTrack::new("/m/a.mp3".into());
    let b = clip_core::AudioTrack::new("/m/b.mp3".into());
    let (a_id, b_id) = (a.id, b.id);
    let project = app.project.as_mut().unwrap();
    project.added_tracks.push(a);
    project.added_tracks.push(b);
    (app, a_id, b_id)
}

/// The original bug: add A, add B, remove A while B is still decoding. B's
/// result carried B's *position*, which the removal had just given to a
/// different track, so B's samples landed on the wrong track and the mixer
/// played them confidently.
#[test]
fn removing_a_track_mid_decode_does_not_misroute_the_result() {
    let ctx = egui::Context::default();
    let (mut app, _a_id, b_id) = app_with_two_tracks(&ctx);

    app.remove_audio_track(0);
    assert_eq!(app.project.as_ref().unwrap().added_tracks.len(), 1);

    // B's decode finishes now, after the removal.
    app.tx
        .send(Msg::TrackReady {
            id: b_id,
            duration: Some(clip_core::RationalTime::new(7, 1)),
            pcm: Some(tone(0.5)),
        })
        .unwrap();
    app.drain_messages(&ctx);

    let tracks = &app.project.as_ref().unwrap().added_tracks;
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].id, b_id, "the surviving track should be B");
    assert_eq!(
        tracks[0].duration,
        Some(clip_core::RationalTime::new(7, 1)),
        "B's duration must land on B"
    );
    assert_eq!(
        app.track_pcm[&b_id].samples[0], 0.5,
        "B's samples must be stored under B"
    );
    assert_eq!(app.track_pcm.len(), 1, "no other track picked up samples");
}

/// The case that actually misroutes rather than merely losing a result. With
/// three tracks, removing the first shifts B to 0 and C to 1 — so a result
/// carrying B's old position of 1 lands on C, and C plays B's audio.
#[test]
fn a_removal_does_not_hand_one_tracks_audio_to_another() {
    let ctx = egui::Context::default();
    let mut app = App::with_context(&ctx);
    let mut source = clip_core::SourceInfo {
        width: 640,
        height: 360,
        ..Default::default()
    };
    source.duration = clip_core::RationalTime::new(10, 1);
    source.avg_frame_rate = clip_core::Rational::new(30, 1);
    source.r_frame_rate = source.avg_frame_rate;
    app.project = Some(clip_core::Project::new(source));

    let a = clip_core::AudioTrack::new("/m/a.mp3".into());
    let b = clip_core::AudioTrack::new("/m/b.mp3".into());
    let c = clip_core::AudioTrack::new("/m/c.mp3".into());
    let (b_id, c_id) = (b.id, c.id);
    let project = app.project.as_mut().unwrap();
    project.added_tracks.extend([a, b, c]);

    app.remove_audio_track(0);

    // B, decoding since before the removal, finishes.
    app.tx
        .send(Msg::TrackReady {
            id: b_id,
            duration: Some(clip_core::RationalTime::new(7, 1)),
            pcm: Some(tone(0.5)),
        })
        .unwrap();
    app.drain_messages(&ctx);

    assert_eq!(
        app.track_pcm[&b_id].samples[0], 0.5,
        "B keeps its own audio"
    );
    assert!(
        !app.track_pcm.contains_key(&c_id),
        "C must not receive B's audio"
    );
    let tracks = &app.project.as_ref().unwrap().added_tracks;
    let c = tracks.iter().find(|t| t.id == c_id).unwrap();
    assert!(c.duration.is_none(), "C must not receive B's duration");
}

/// The result for a track that no longer exists must be dropped, and must not
/// leave state behind for the next track to inherit.
#[test]
fn a_result_for_a_removed_track_is_dropped() {
    let ctx = egui::Context::default();
    let (mut app, a_id, b_id) = app_with_two_tracks(&ctx);

    app.remove_audio_track(0); // remove A
    app.tx
        .send(Msg::TrackReady {
            id: a_id,
            duration: Some(clip_core::RationalTime::new(3, 1)),
            pcm: Some(tone(0.9)),
        })
        .unwrap();
    app.drain_messages(&ctx);

    assert!(
        app.track_pcm.is_empty(),
        "a removed track's samples must not be kept"
    );
    let tracks = &app.project.as_ref().unwrap().added_tracks;
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].id, b_id);
    assert!(
        tracks[0].duration.is_none(),
        "A's duration must not land on B"
    );
}

/// Ids must not be reused when a removal frees a position, or a decode still
/// running for the removed track would be mistaken for the new one's.
#[test]
fn a_new_track_does_not_inherit_a_removed_tracks_identity() {
    let ctx = egui::Context::default();
    let (mut app, a_id, _b_id) = app_with_two_tracks(&ctx);

    app.remove_audio_track(0);
    let fresh = clip_core::AudioTrack::new("/m/c.mp3".into());
    assert_ne!(fresh.id, a_id, "a new track must not reuse a freed id");

    let fresh_id = fresh.id;
    app.project.as_mut().unwrap().added_tracks.push(fresh);

    // A's decode, still running when it was removed, now finishes.
    app.tx
        .send(Msg::TrackReady {
            id: a_id,
            duration: Some(clip_core::RationalTime::new(3, 1)),
            pcm: Some(tone(0.9)),
        })
        .unwrap();
    app.drain_messages(&ctx);

    assert!(
        !app.track_pcm.contains_key(&fresh_id),
        "the new track must not receive the removed track's audio"
    );
}

/// The mixer reads by id too, so a track with no decoded audio is absent from
/// the mix rather than playing whatever sat in its old slot.
#[test]
fn the_mixer_matches_audio_to_tracks_by_identity() {
    let ctx = egui::Context::default();
    let (mut app, a_id, b_id) = app_with_two_tracks(&ctx);

    // Only B has finished decoding.
    app.tx
        .send(Msg::TrackReady {
            id: b_id,
            duration: None,
            pcm: Some(tone(0.5)),
        })
        .unwrap();
    app.drain_messages(&ctx);

    let project = app.project.as_ref().unwrap();
    let state = clip_editor::playback::MixState::build(project, None, &app.track_pcm);
    assert_eq!(
        state.tracks.len(),
        1,
        "only the track with decoded audio belongs in the mix"
    );
    assert_eq!(state.tracks[0].pcm.samples[0], 0.5, "and it is B's audio");
    assert!(!app.track_pcm.contains_key(&a_id));
}
