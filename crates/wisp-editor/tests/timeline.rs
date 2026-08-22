//! Keyframes, scrubbing, onion skin, and the claim that the editor's preview
//! is the shipping player rather than a lookalike.

mod support;

use support::{canonical, isolate, shipped_doc, shipped_editor};

use wisp_editor::cmd::Command;
use wisp_editor::preview::{Preview, STEP_S};
use wisp_editor::timeline::{self, Onion, TimelineState};
use wisp_rig::skin::doc::{EaseSpec, SkinDoc};
use wisp_rig::{ClipPlayer, Skin};

fn clip_named(doc: &SkinDoc, name: &str) -> usize {
    timeline::clip_index(doc, name).unwrap_or_else(|| panic!("the shipped skin has a {name} clip"))
}

#[test]
fn a_key_on_a_bone_with_no_track_creates_the_track() {
    isolate();
    let mut doc = shipped_doc();
    let clip = clip_named(&doc, "idle");
    let before = doc.clips[clip].tracks.len();
    let cmd = timeline::set_key(&doc, clip, "root", "sx", 400.0, 1.2, Some("out")).unwrap();
    assert!(matches!(cmd, Command::InsertTrack { .. }));
    cmd.apply(&mut doc).unwrap();
    assert_eq!(doc.clips[clip].tracks.len(), before + 1);
    let t = timeline::track_index(&doc, clip, "root", "sx").expect("the track now exists");
    assert_eq!(doc.clips[clip].tracks[t].t.len(), 1);
    assert_eq!(doc.clips[clip].tracks[t].ease, Some(EaseSpec::All("out".into())));
    Skin::compile(doc).expect("a keyed skin still compiles");
}

#[test]
fn a_key_on_an_existing_track_lands_in_time_order() {
    isolate();
    let mut doc = shipped_doc();
    let clip = clip_named(&doc, "idle");
    // Find a track with at least two keys and key between them.
    let track = doc.clips[clip]
        .tracks
        .iter()
        .position(|t| t.t.len() >= 2)
        .expect("idle has a real curve");
    let (bone, channel) = {
        let t = &doc.clips[clip].tracks[track];
        (t.bone.clone(), t.channel.clone())
    };
    let mid = {
        let t = &doc.clips[clip].tracks[track];
        (t.t[0].0 + t.t[1].0) * 0.5
    };
    let before = doc.clips[clip].tracks[track].t.len();
    let cmd = timeline::set_key(&doc, clip, &bone, &channel, mid, 0.75, None).unwrap();
    cmd.apply(&mut doc).unwrap();

    let t = &doc.clips[clip].tracks[track];
    assert_eq!(t.t.len(), before + 1);
    assert_eq!(t.v.len(), t.t.len(), "the parallel arrays stay in step");
    assert!(t.t.windows(2).all(|w| w[0].0 <= w[1].0), "keys must not go backwards");
    Skin::compile(doc).expect("still valid");
}

#[test]
fn keying_on_top_of_an_existing_key_replaces_it_rather_than_stacking() {
    isolate();
    let mut doc = shipped_doc();
    let clip = clip_named(&doc, "idle");
    let track = doc.clips[clip].tracks.iter().position(|t| !t.t.is_empty()).unwrap();
    let (bone, channel, at) = {
        let t = &doc.clips[clip].tracks[track];
        (t.bone.clone(), t.channel.clone(), t.t[0].0)
    };
    let before = doc.clips[clip].tracks[track].t.len();
    // Half a frame away: within the snap window.
    let cmd = timeline::set_key(&doc, clip, &bone, &channel, at + 8.0, 9.0, None).unwrap();
    assert!(matches!(cmd, Command::SetKey { .. }));
    cmd.apply(&mut doc).unwrap();
    assert_eq!(doc.clips[clip].tracks[track].t.len(), before);
    assert_eq!(doc.clips[clip].tracks[track].v[0].0, 9.0);
}

#[test]
fn moving_a_key_is_clamped_between_its_neighbours() {
    isolate();
    let mut doc = shipped_doc();
    let clip = clip_named(&doc, "idle");
    let track = doc.clips[clip].tracks.iter().position(|t| t.t.len() >= 3).expect("a 3-key track");
    let (t0, t2) = {
        let t = &doc.clips[clip].tracks[track];
        (t.t[0].0, t.t[2].0)
    };
    // Drag the middle key far past the last one.
    let cmd = timeline::move_key(&doc, clip, track, 1, 1e6, 0.0).unwrap();
    cmd.apply(&mut doc).unwrap();
    let t = &doc.clips[clip].tracks[track];
    assert!(t.t[1].0 <= t2 + 1e-3, "it must stop at the next key");
    assert!(t.t[1].0 >= t0 - 1e-3);
    assert!(t.t.windows(2).all(|w| w[0].0 <= w[1].0));
}

#[test]
fn an_out_of_order_key_is_refused_outright() {
    isolate();
    let mut doc = shipped_doc();
    let clip = clip_named(&doc, "idle");
    let track = doc.clips[clip].tracks.iter().position(|t| t.t.len() >= 3).unwrap();
    let before = canonical(&doc);
    let err = (Command::SetKey { clip, track, at: 1, t: -5000.0, v: 0.0 })
        .apply(&mut doc)
        .expect_err("a key cannot jump behind its predecessor");
    assert!(err.to_string().contains("must not go backwards"), "{err}");
    assert_eq!(before, canonical(&doc));
}

#[test]
fn deleting_the_last_key_of_a_track_takes_the_track_with_it() {
    isolate();
    let mut doc = shipped_doc();
    let clip = clip_named(&doc, "idle");
    // Make a one-key track, then delete its key.
    timeline::set_key(&doc, clip, "root", "alpha", 0.0, 1.0, None)
        .unwrap()
        .apply(&mut doc)
        .unwrap();
    let track = timeline::track_index(&doc, clip, "root", "alpha").unwrap();
    let cmd = timeline::delete_key(&doc, clip, track, 0).unwrap();
    assert!(matches!(cmd, Command::RemoveTrack { .. }), "an empty track is a validation error");
    cmd.apply(&mut doc).unwrap();
    assert!(timeline::track_index(&doc, clip, "root", "alpha").is_none());
    Skin::compile(doc).expect("still valid");
}

#[test]
fn deleting_one_key_of_many_leaves_the_track() {
    isolate();
    let mut doc = shipped_doc();
    let clip = clip_named(&doc, "idle");
    let track = doc.clips[clip].tracks.iter().position(|t| t.t.len() >= 3).unwrap();
    let before = doc.clips[clip].tracks[track].t.len();
    timeline::delete_key(&doc, clip, track, 1).unwrap().apply(&mut doc).unwrap();
    let t = &doc.clips[clip].tracks[track];
    assert_eq!(t.t.len(), before - 1);
    assert_eq!(t.v.len(), t.t.len());
    Skin::compile(doc).expect("still valid");
}

#[test]
fn a_per_key_easing_keeps_the_arrays_the_same_length() {
    isolate();
    let mut doc = shipped_doc();
    let clip = clip_named(&doc, "idle");
    let track = doc.clips[clip].tracks.iter().position(|t| t.t.len() >= 3).unwrap();
    timeline::set_key_ease(&doc, clip, track, 1, "spring")
        .unwrap()
        .apply(&mut doc)
        .unwrap();
    let t = &doc.clips[clip].tracks[track];
    match t.ease.as_ref().expect("an easing") {
        EaseSpec::Each(list) => assert_eq!(list.len(), t.t.len()),
        EaseSpec::All(_) => panic!("one key differs, so it cannot be a single easing"),
    }
    // Inserting a key on a per-key track keeps them in step.
    let mid = (t.t[0].0 + t.t[1].0) * 0.5;
    let (bone, channel) = (t.bone.clone(), t.channel.clone());
    timeline::set_key(&doc, clip, &bone, &channel, mid, 1.0, Some("out"))
        .unwrap()
        .apply(&mut doc)
        .unwrap();
    let t = &doc.clips[clip].tracks[track];
    if let Some(EaseSpec::Each(list)) = &t.ease {
        assert_eq!(list.len(), t.t.len(), "an inserted key needs an easing too");
    }
    Skin::compile(doc).expect("still valid");
}

#[test]
fn setting_every_key_to_the_same_easing_writes_the_compact_form_back() {
    isolate();
    let mut doc = shipped_doc();
    let clip = clip_named(&doc, "idle");
    let track = doc.clips[clip].tracks.iter().position(|t| t.t.len() >= 3).unwrap();
    let n = doc.clips[clip].tracks[track].t.len();
    for k in 0..n {
        timeline::set_key_ease(&doc, clip, track, k, "spring")
            .unwrap()
            .apply(&mut doc)
            .unwrap();
    }
    assert_eq!(
        doc.clips[clip].tracks[track].ease,
        Some(EaseSpec::All("spring".into())),
        "a track where every key agrees should read as one easing, not a list of eight"
    );
}

#[test]
fn an_unknown_channel_or_easing_is_refused_by_name() {
    isolate();
    let doc = shipped_doc();
    let clip = clip_named(&doc, "idle");
    let err = timeline::set_key(&doc, clip, "root", "wobble", 0.0, 0.0, None).unwrap_err();
    assert!(err.to_string().contains("wobble"), "{err}");
    let err = timeline::set_track_ease(&doc, clip, 0, "bouncy").unwrap_err();
    assert!(err.to_string().contains("bouncy"), "{err}");
}

// ------------------------------------------------------------------ sampling

#[test]
fn the_editor_samples_a_curve_the_way_the_rig_does() {
    isolate();
    let doc = shipped_doc();
    let clip = clip_named(&doc, "idle");
    let skin = Skin::compile(doc.clone()).unwrap();

    for (track, tr) in doc.clips[clip].tracks.iter().enumerate() {
        let rig_track = &skin.clips[clip].tracks[track];
        // The file authors degrees and the rig runs in radians; compilation
        // converts once. The curve editor shows the number the operator typed,
        // so the comparison converts here and nowhere else.
        let to_editor_units = |v: f32| {
            if tr.channel == "rot" {
                wisp_rig::math::rad_to_deg(v)
            } else {
                v
            }
        };
        for step in 0..=20 {
            let t_ms = step as f32 * doc.clips[clip].duration_ms.0 / 20.0;
            let mine = timeline::sample(&doc, clip, track, t_ms).unwrap();
            let theirs = to_editor_units(rig_track.sample(t_ms / 1000.0));
            assert!(
                (mine - theirs).abs() < 1e-3,
                "track {track} ({} {}) at {t_ms} ms: editor {mine}, rig {theirs}",
                tr.bone,
                tr.channel
            );
        }
    }
}

// ------------------------------------------------------------------- preview

#[test]
fn a_scrub_is_deterministic() {
    isolate();
    let skin = Skin::compile(shipped_doc()).unwrap();
    let mut a = Preview::new(skin.clone());
    let mut b = Preview::new(skin);
    a.seek(0, 0, 1234.0);
    // Reach the same time by a different route: forwards in small steps.
    for t in [200.0, 600.0, 900.0, 1234.0] {
        b.seek(0, 0, t);
    }
    for (i, (x, y)) in a.pose().offsets.iter().zip(&b.pose().offsets).enumerate() {
        assert!(
            (x.rot - y.rot).abs() < 1e-4 && (x.tx - y.tx).abs() < 1e-4,
            "bone {i} differs between two routes to the same time"
        );
    }
}

#[test]
fn seeking_backwards_replays_from_the_start_and_still_matches() {
    isolate();
    let skin = Skin::compile(shipped_doc()).unwrap();
    let mut p = Preview::new(skin.clone());
    p.seek(0, 0, 800.0);
    let forward: Vec<f32> = p.pose().offsets.iter().map(|o| o.rot).collect();
    p.seek(0, 0, 2400.0);
    p.seek(0, 0, 800.0);
    let backward: Vec<f32> = p.pose().offsets.iter().map(|o| o.rot).collect();
    for (a, b) in forward.iter().zip(&backward) {
        assert!((a - b).abs() < 1e-4, "a backwards scrub must land on the same pose");
    }
}

#[test]
fn the_preview_frame_comes_out_in_canvas_units() {
    isolate();
    let doc = shipped_doc();
    let skin = Skin::compile(doc.clone()).unwrap();
    let mut p = Preview::new(skin);
    p.seek(0, 0, 0.0);
    let f = p.frame();
    assert!((f.scale - 1.0).abs() < 1e-4, "scale should be 1: {}", f.scale);
    // Her bounds sit inside the canvas she was authored on.
    let size = wisp_rig::math::Vec2::new(doc.canvas.size[0].0, doc.canvas.size[1].0);
    assert!(f.bounds.min.x > -size.x, "{:?}", f.bounds);
    assert!(f.bounds.max.x < size.x * 2.0, "{:?}", f.bounds);
    assert!(!f.shapes.is_empty());
}

#[test]
fn the_cross_fade_preview_matches_a_hand_driven_clip_player() {
    isolate();
    let doc = shipped_doc();
    let skin = Skin::compile(doc.clone()).unwrap();
    let idle = clip_named(&doc, "idle");
    let walk = clip_named(&doc, "walk");
    let fade_ms = 220.0;
    let at_ms = 140.0; // part-way through the fade, where the blend is doing work

    // The editor's route.
    let mut p = Preview::new(skin.clone());
    p.seek_crossfade(0, idle, walk, fade_ms, at_ms);
    let mine: Vec<_> = p.rig().player().current(0).into_iter().collect();
    assert_eq!(mine, vec![walk]);

    // The same thing driven by hand, one `ClipPlayer` and nothing else.
    let mut player = ClipPlayer::new(skin.layers.clone(), skin.skeleton.len());
    player.play(0, idle, 0.0);
    player.snap();
    player.play(0, walk, fade_ms / 1000.0);
    let mut elapsed = 0.0f32;
    let target = at_ms / 1000.0;
    while elapsed + 1e-6 < target {
        let dt = STEP_S.min(target - elapsed);
        player.update(&skin.clips, dt);
        elapsed += dt;
    }
    let mut theirs = vec![wisp_rig::BoneOffsets::IDENTITY; skin.skeleton.len()];
    player.evaluate(&skin.clips, &mut theirs);

    // The rig's pose offsets after step 1 of `Rig::update` are exactly what the
    // player produced; procedural motion is layered on afterwards, so compare
    // the player's own output on both sides.
    let mut mine_offsets = vec![wisp_rig::BoneOffsets::IDENTITY; skin.skeleton.len()];
    let mut mine_player = p.rig().player().clone();
    mine_player.evaluate(&skin.clips, &mut mine_offsets);

    assert!(p.rig().player().fading(0), "the fade should still be in flight at {at_ms} ms");
    for (i, (a, b)) in mine_offsets.iter().zip(&theirs).enumerate() {
        assert!(
            (a.tx - b.tx).abs() < 1e-4
                && (a.ty - b.ty).abs() < 1e-4
                && (a.rot - b.rot).abs() < 1e-4
                && (a.sx - b.sx).abs() < 1e-4
                && (a.sy - b.sy).abs() < 1e-4
                && (a.alpha - b.alpha).abs() < 1e-4,
            "bone {i}: editor {a:?}, hand-driven player {b:?}"
        );
    }
}

#[test]
fn a_clip_that_is_only_part_way_through_a_fade_is_not_yet_the_target_pose() {
    isolate();
    let doc = shipped_doc();
    let skin = Skin::compile(doc.clone()).unwrap();
    let idle = clip_named(&doc, "idle");
    let walk = clip_named(&doc, "walk");

    let mut mid = Preview::new(skin.clone());
    mid.seek_crossfade(0, idle, walk, 400.0, 100.0);
    let mut done = Preview::new(skin.clone());
    done.seek_crossfade(0, idle, walk, 400.0, 800.0);

    assert!(mid.rig().player().fading(0));
    assert!(!done.rig().player().fading(0), "the fade is over by 800 ms");
    let differs = mid
        .pose()
        .offsets
        .iter()
        .zip(&done.pose().offsets)
        .any(|(a, b)| (a.rot - b.rot).abs() > 1e-3 || (a.ty - b.ty).abs() > 1e-3);
    assert!(differs, "a fade in progress must not already be the destination pose");
}

// ---------------------------------------------------------------- onion skin

#[test]
fn onion_ghosts_wrap_for_a_looping_clip_and_clamp_for_a_one_shot() {
    let onion = Onion { enabled: true, before: 2, after: 1, spacing_ms: 100.0, strength: 0.4 };
    let looped = onion.ghosts(50.0, 1000.0, true);
    assert_eq!(looped.len(), 3);
    assert!(looped.iter().all(|(t, _)| (0.0..1000.0).contains(t)), "{looped:?}");
    assert!(looped.iter().any(|(t, _)| *t > 900.0), "one ghost wraps round the end: {looped:?}");

    let once = onion.ghosts(50.0, 1000.0, false);
    assert!(once.iter().all(|(t, _)| (0.0..=1000.0).contains(t)), "{once:?}");
    assert!(once.iter().all(|(t, _)| *t >= 0.0));
}

#[test]
fn onion_is_off_by_default_and_produces_nothing_when_off() {
    let onion = Onion::default();
    assert!(!onion.enabled, "a replay per ghost is the expensive call in the editor");
    assert!(onion.ghosts(500.0, 1000.0, true).is_empty());
}

#[test]
fn ghost_alpha_fades_with_distance_from_the_playhead() {
    let onion = Onion { enabled: true, before: 3, after: 0, spacing_ms: 100.0, strength: 0.6 };
    let g = onion.ghosts(500.0, 1000.0, true);
    assert_eq!(g.len(), 3);
    assert!(g[0].1 >= g[1].1 && g[1].1 >= g[2].1, "further ghosts must be fainter: {g:?}");
    assert!(g.iter().all(|(_, a)| *a > 0.0 && *a <= 0.6));
}

#[test]
fn the_preview_produces_one_frame_per_ghost() {
    isolate();
    let doc = shipped_doc();
    let skin = Skin::compile(doc.clone()).unwrap();
    let mut p = Preview::new(skin);
    let onion = Onion { enabled: true, before: 2, after: 1, spacing_ms: 120.0, strength: 0.35 };
    let times = onion.ghosts(600.0, doc.clips[0].duration_ms.0, doc.clips[0].looping);
    let ghosts = p.ghosts(0, 0, &times);
    assert_eq!(ghosts.len(), times.len());
    assert!(ghosts.iter().all(|g| !g.frame.shapes.is_empty()));
    assert!(ghosts.iter().all(|g| g.alpha > 0.0 && g.alpha < 1.0));
}

// -------------------------------------------------------------------- state

#[test]
fn the_playhead_loops_or_stops_as_the_preview_asks() {
    let mut tl = TimelineState { playing: true, loop_preview: true, ..Default::default() };
    tl.tick(1500.0, 1000.0);
    assert!((tl.playhead_ms - 500.0).abs() < 1e-3);
    assert!(tl.playing);

    let mut tl = TimelineState { playing: true, loop_preview: false, ..Default::default() };
    tl.tick(1500.0, 1000.0);
    assert!((tl.playhead_ms - 1000.0).abs() < 1e-3);
    assert!(!tl.playing, "a one-shot preview stops at the end");
}

#[test]
fn scrubbing_maps_pixels_to_time_and_back() {
    let tl = TimelineState { scale_px_per_ms: 0.2, scroll_ms: 400.0, ..Default::default() };
    let ruler_x = 148.0;
    for ms in [0.0f32, 400.0, 1234.5, 3200.0] {
        let px = tl.time_to_px(ms, ruler_x);
        let back = tl.px_to_time(px, ruler_x);
        assert!((ms - back).abs() < 1e-2, "{ms} -> {px} -> {back}");
    }
}

#[test]
fn scrubbing_is_clamped_to_the_clip() {
    let mut tl = TimelineState { scale_px_per_ms: 0.2, ..Default::default() };
    tl.scrub_to_px(-5000.0, 148.0, 1000.0);
    assert_eq!(tl.playhead_ms, 0.0);
    tl.scrub_to_px(5000.0, 148.0, 1000.0);
    assert_eq!(tl.playhead_ms, 1000.0);
}

#[test]
fn every_clip_and_every_expression_is_reachable_from_the_editor() {
    let mut ed = shipped_editor();
    assert_eq!(ed.doc().clips.len(), 16);
    for i in 0..ed.doc().clips.len() {
        ed.open_clip(i);
        assert_eq!(ed.timeline.clip, i);
        // ...and it can take a keyframe.
        let cmd = wisp_editor::timeline::set_key(ed.doc(), i, "root", "rot", 10.0, 1.0, None)
            .expect("every clip takes a key");
        ed.apply(cmd).expect("and it applies");
        assert!(ed.validation().ok(), "clip {i}: {:?}", ed.validation().problems);
    }
    assert_eq!(ed.doc().expressions.len(), 8);
    for i in 0..ed.doc().expressions.len() {
        ed.open_expression(i).expect("every expression opens its clip");
    }
}

#[test]
fn the_timelines_rows_group_tracks_by_bone_in_document_order() {
    isolate();
    let doc = shipped_doc();
    let clip = clip_named(&doc, "walk");
    let rows = timeline::rows(&doc, clip);
    assert!(!rows.is_empty());
    let total: usize = rows.iter().map(|r| r.tracks.len()).sum();
    assert_eq!(total, doc.clips[clip].tracks.len(), "every track appears exactly once");
    // Rows follow the bone list, so the timeline and the tree read alike.
    let order: Vec<usize> = rows
        .iter()
        .filter_map(|r| doc.bones.iter().position(|b| b.name == r.bone))
        .collect();
    assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
}
